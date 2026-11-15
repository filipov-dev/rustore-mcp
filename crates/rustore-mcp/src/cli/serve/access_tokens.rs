//! Токены доступа клиентов к HTTP-серверу (`serve --http --access-tokens <file>`).
//!
//! Токен доступа — случайная строка, которую клиент MCP присылает в
//! `Authorization: Bearer <токен>`. Это не ключ RuStore API и не токен RuStore: он только
//! пускает клиента к серверу и выбирает его уровень доступа и наборы инструментов. Ключ
//! RuStore API остаётся на сервере. В файле лежат только SHA-256 токенов (hex), сами токены
//! сервер не хранит и не логирует.
//!
//! ```toml
//! [[tokens]]
//! name = "ci"                     # имя для логов: [A-Za-z0-9._-], до 64 символов
//! sha256 = "<64 hex-символа>"     # SHA-256 токена
//! access = "read"                 # read | write | destructive, по умолчанию read
//! toolsets = ["apps"]             # по умолчанию — --toolsets сервера или наборы по умолчанию
//! ```
//!
//! Уровень токена не выше уровня сервера (`--allow-write`, `--allow-destructive`): токен с
//! более высоким уровнем — ошибка запуска, а не молчаливое понижение. Сравнение хешей — за
//! постоянное время ([`subtle`]), перебираются все записи.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use rustore_api::AccessLevel;
use rustore_mcp_core::{access_name, normalize_toolsets};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Лимит размера файла токенов: в нём только хеши и настройки.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Максимальная длина имени токена.
const MAX_NAME_LEN: usize = 64;
/// Минимальная длина токена в запросе: короче — слишком мало случайности, такой токен
/// сервер не принимает, даже если его хеш есть в файле.
pub const MIN_TOKEN_LEN: usize = 32;
/// Максимальная длина токена в запросе.
pub const MAX_TOKEN_LEN: usize = 512;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    tokens: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    sha256: String,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    toolsets: Option<Vec<String>>,
}

/// Настройки клиента с этим токеном.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    /// Имя токена из файла (в логах вместо самого токена).
    pub name: String,
    pub access: AccessLevel,
    /// Наборы токена; `None` — наборы сервера по умолчанию.
    pub toolsets: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Token {
    hash: [u8; 32],
    client: Client,
}

/// Токены доступа из файла. `Debug` показывает только имена.
#[derive(Clone)]
pub struct AccessTokens {
    tokens: Vec<Token>,
}

impl std::fmt::Debug for AccessTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.tokens.iter().map(|t| &t.client.name))
            .finish()
    }
}

/// SHA-256 токена в hex (нижний регистр), как его записывают в файл токенов.
#[cfg(test)]
pub fn hash_hex(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl AccessTokens {
    /// Читает файл токенов. `max_access` — уровень сервера: токен выше него — ошибка.
    pub fn load(path: &Path, max_access: AccessLevel) -> anyhow::Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("cannot read the access tokens file {}", path.display()))?;
        if metadata.len() > MAX_FILE_BYTES {
            bail!(
                "the access tokens file {} is larger than {MAX_FILE_BYTES} bytes",
                path.display()
            );
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the access tokens file {}", path.display()))?;
        Self::parse(&text, max_access)
            .with_context(|| format!("invalid access tokens file {}", path.display()))
    }

    pub fn parse(text: &str, max_access: AccessLevel) -> anyhow::Result<Self> {
        let file: File = toml::from_str(text).map_err(|err| anyhow!("{}", err.message()))?;
        if file.tokens.is_empty() {
            bail!(
                "no access tokens: add at least one [[tokens]] entry with `name` and `sha256` \
                 (the SHA-256 of the token in hex)"
            );
        }
        let mut names = HashSet::new();
        let mut hashes = HashSet::new();
        let mut tokens = Vec::with_capacity(file.tokens.len());
        for entry in file.tokens {
            let name = entry.name;
            let valid_name = !name.is_empty()
                && name.len() <= MAX_NAME_LEN
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if !valid_name {
                bail!(
                    "token name `{name}`: use 1-{MAX_NAME_LEN} characters from A-Z, a-z, 0-9, \
                     `.`, `_`, `-`"
                );
            }
            if !names.insert(name.clone()) {
                bail!("token name `{name}` is used twice");
            }
            let hash = parse_hash(&entry.sha256).ok_or_else(|| {
                anyhow!(
                    "token `{name}`: `sha256` must be 64 hex characters (the SHA-256 of the \
                     token, for example from `printf %s \"$TOKEN\" | sha256sum`), not the token \
                     itself"
                )
            })?;
            if !hashes.insert(hash) {
                bail!("token `{name}`: the same `sha256` is used by another token");
            }
            let access = match entry.access.as_deref() {
                None | Some("read") => AccessLevel::Read,
                Some("write") => AccessLevel::Write,
                Some("destructive") => AccessLevel::Destructive,
                Some(other) => bail!(
                    "token `{name}`: unknown access `{other}`, expected read, write or destructive"
                ),
            };
            if access > max_access {
                bail!(
                    "token `{name}` has access `{}`, but the server is started with `{}` \
                     access: start it with {} or lower the token's access",
                    access_name(access),
                    access_name(max_access),
                    match access {
                        AccessLevel::Destructive => "--allow-destructive",
                        _ => "--allow-write",
                    }
                );
            }
            let toolsets = match entry.toolsets {
                None => None,
                Some(list) => {
                    let list = normalize_toolsets(&list)
                        .map_err(|err| anyhow!("token `{name}`: {err}"))?;
                    if list.is_empty() {
                        bail!(
                            "token `{name}`: `toolsets` is empty; remove it for the server's \
                             toolsets"
                        );
                    }
                    Some(list)
                }
            };
            tokens.push(Token {
                hash,
                client: Client {
                    name,
                    access,
                    toolsets,
                },
            });
        }
        Ok(Self { tokens })
    }

    /// Клиенты в порядке файла (индекс — номер токена).
    pub fn clients(&self) -> impl Iterator<Item = &Client> {
        self.tokens.iter().map(|t| &t.client)
    }

    /// Номер токена по присланному значению. Хеш сравнивается со всеми записями за
    /// постоянное время: время ответа не зависит от того, совпал ли и какой токен.
    pub fn find(&self, token: &str) -> Option<usize> {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let mut found = None;
        for (index, entry) in self.tokens.iter().enumerate() {
            if bool::from(entry.hash.ct_eq(&digest)) {
                found = Some(index);
            }
        }
        found
    }
}

fn parse_hash(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        hash[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "test-access-token-0123456789abcdef-reader";

    fn file(entries: &str) -> String {
        entries.replace("HASH", &hash_hex(TOKEN))
    }

    #[test]
    fn hash_matches_sha256sum() {
        // `printf %s abc | sha256sum`.
        assert_eq!(
            hash_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Имя первого набора спецификации: тесты не зависят от состава наборов.
    fn first_toolset() -> &'static str {
        rustore_api::spec::toolsets()[0].slug
    }

    #[test]
    fn parses_tokens_with_defaults() {
        let text = file(&format!(
            r#"
            [[tokens]]
            name = "reader"
            sha256 = "HASH"

            [[tokens]]
            name = "release-bot"
            sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015AD"
            access = "write"
            toolsets = ["{}"]
            "#,
            first_toolset()
        ));
        let tokens = AccessTokens::parse(&text, AccessLevel::Write).unwrap();
        let clients: Vec<_> = tokens.clients().cloned().collect();
        assert_eq!(clients[0].name, "reader");
        assert_eq!(clients[0].access, AccessLevel::Read);
        assert_eq!(clients[0].toolsets, None);
        assert_eq!(clients[1].access, AccessLevel::Write);
        assert_eq!(clients[1].toolsets, Some(vec![first_toolset().to_owned()]));

        assert_eq!(tokens.find(TOKEN), Some(0));
        assert_eq!(tokens.find("abc"), Some(1));
        assert_eq!(tokens.find("abcd"), None);
        assert_eq!(tokens.find(""), None);
        // `Debug` без хешей.
        let debug = format!("{tokens:?}");
        assert!(!debug.contains(&hash_hex(TOKEN)), "{debug}");
        assert!(debug.contains("release-bot"), "{debug}");
    }

    #[test]
    fn rejects_bad_entries() {
        let cases = [
            ("", "no access tokens"),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\nx = 1\n",
                "unknown field",
            ),
            (
                "[[tokens]]\nname = \"a b\"\nsha256 = \"HASH\"\n",
                "token name `a b`",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"secret-token\"\n",
                "64 hex characters",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\n[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\n",
                "used twice",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\n[[tokens]]\nname = \"b\"\nsha256 = \"HASH\"\n",
                "used by another token",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\naccess = \"admin\"\n",
                "unknown access `admin`",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\naccess = \"write\"\n",
                "start it with --allow-write",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\ntoolsets = [\"nope\"]\n",
                "unknown toolset `nope`",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\ntoolsets = []\n",
                "`toolsets` is empty",
            ),
            (
                "[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\ngoogle_token = \"client\"\n",
                "unknown field",
            ),
        ];
        for (text, expected) in cases {
            let err = AccessTokens::parse(&file(text), AccessLevel::Read).unwrap_err();
            let err = format!("{err:#}");
            assert!(err.contains(expected), "{text}: {err}");
        }
        // Уровень destructive подсказывает свой флаг.
        let text = file("[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\naccess = \"destructive\"\n");
        let err = AccessTokens::parse(&text, AccessLevel::Write).unwrap_err();
        assert!(err.to_string().contains("--allow-destructive"), "{err}");
    }

    #[test]
    fn loads_file_and_reports_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.toml");
        std::fs::write(&path, "[[tokens]]\nname = \"a\"\n").unwrap();
        let err = format!(
            "{:#}",
            AccessTokens::load(&path, AccessLevel::Read).unwrap_err()
        );
        assert!(err.contains("tokens.toml"), "{err}");
        assert!(err.contains("missing field `sha256`"), "{err}");
        std::fs::write(&path, file("[[tokens]]\nname = \"a\"\nsha256 = \"HASH\"\n")).unwrap();
        assert!(AccessTokens::load(&path, AccessLevel::Read).is_ok());
        assert!(AccessTokens::load(&dir.path().join("missing"), AccessLevel::Read).is_err());
    }
}
