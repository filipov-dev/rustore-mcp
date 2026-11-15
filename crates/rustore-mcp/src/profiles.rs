//! Профили: настройки в `~/.config/rustore-mcp/profiles.toml`, ключ RuStore API (keyId и
//! приватный ключ) — в системном хранилище под именем профиля.
//!
//! В файле профилей нет ключей и других секретов: только MCP-клиент, имена пакетов приложений
//! по умолчанию (подсказка агенту) и настройки доступа по умолчанию. keyId тоже лежит в
//! системном хранилище вместе с ключом: так запись ключа самодостаточна, а файл профилей
//! можно показывать.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rustore_api::ApiKey;
use rustore_api::auth::{AuthError, KeyStore, KeyringStore};
use serde::{Deserialize, Serialize};

/// Переменная окружения, которая переопределяет каталог конфигурации (нужна для тестов).
pub const CONFIG_DIR_ENV: &str = "RUSTORE_MCP_CONFIG_DIR";

/// Имя файла профилей в каталоге конфигурации.
pub const PROFILES_FILE: &str = "profiles.toml";

/// Профиль по умолчанию.
pub const DEFAULT_PROFILE: &str = "default";

/// Максимальная длина имени профиля.
const MAX_PROFILE_LEN: usize = 64;

/// Настройки профиля. Ключа здесь нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// MCP-клиент, в котором `connect` зарегистрировал профиль (`claude-code`, `codex`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Имена пакетов приложений по умолчанию: подсказка агенту в инструкциях сервера.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// Разрешены операции записи (`serve --allow-write`).
    pub allow_write: bool,
    /// Разрешены удаляющие операции (`serve --allow-destructive`).
    pub allow_destructive: bool,
    /// Включённые наборы инструментов (`serve --toolsets`); пустой список — набор по умолчанию.
    pub toolsets: Vec<String>,
    /// Наборы инструментов включаются агентом по запросу (`serve --dynamic-toolsets`).
    /// Не записывается, если выключено: файл остаётся читаемым прежними версиями.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dynamic_toolsets: bool,
}

impl ProfileSettings {
    /// Действующее разрешение на запись: удаление подразумевает запись.
    pub fn writes_allowed(&self) -> bool {
        self.allow_write || self.allow_destructive
    }

    /// Приводит флаги доступа к действующему уровню: `allow_destructive` включает `allow_write`.
    pub fn normalized(mut self) -> Self {
        self.allow_write = self.writes_allowed();
        self
    }
}

/// Содержимое `profiles.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProfilesDocument {
    profiles: BTreeMap<String, ProfileSettings>,
}

/// Файл профилей.
#[derive(Debug, Clone)]
pub struct ProfileRepo {
    path: PathBuf,
}

impl ProfileRepo {
    /// Файл профилей в каталоге `dir`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(PROFILES_FILE),
        }
    }

    /// Файл профилей в каталоге конфигурации пользователя.
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::in_dir(config_dir()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> anyhow::Result<ProfilesDocument> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfilesDocument::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        toml::from_str(&text).with_context(|| format!("failed to parse {}", self.path.display()))
    }

    fn save(&self, document: &ProfilesDocument) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(document).context("failed to serialize profiles")?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Настройки профиля, если он есть в файле.
    pub fn get(&self, name: &str) -> anyhow::Result<Option<ProfileSettings>> {
        Ok(self.load()?.profiles.remove(name))
    }

    /// Создаёт или заменяет профиль. Флаги доступа сохраняются нормализованными
    /// (см. [`ProfileSettings::normalized`]).
    pub fn upsert(&self, name: &str, settings: ProfileSettings) -> anyhow::Result<()> {
        validate_profile_name(name)?;
        let mut document = self.load()?;
        document
            .profiles
            .insert(name.to_owned(), settings.normalized());
        self.save(&document)
    }

    /// Удаляет профиль. Возвращает `true`, если профиль был.
    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let mut document = self.load()?;
        let removed = document.profiles.remove(name).is_some();
        if removed {
            self.save(&document)?;
        }
        Ok(removed)
    }
}

/// Каталог конфигурации: `RUSTORE_MCP_CONFIG_DIR`, иначе `~/.config/rustore-mcp`.
///
/// На macOS тоже используется `~/.config`, а не `~/Library/Application Support`: так путь
/// одинаков на macOS и Linux. На Linux учитывается `XDG_CONFIG_HOME`.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join(".config"))
    } else {
        dirs::config_dir()
    };
    base.map(|base| base.join("rustore-mcp"))
        .context("cannot determine the user configuration directory")
}

/// Проверяет имя профиля. Имя попадает в аргументы команд MCP-клиентов, ключи TOML и имя
/// записи в keychain, поэтому допускаются только `[A-Za-z0-9._-]` и имя не начинается с `-`.
pub fn validate_profile_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() || name.len() > MAX_PROFILE_LEN || !valid_chars || name.starts_with('-') {
        bail!(
            "invalid profile name `{name}`: use 1-{MAX_PROFILE_LEN} characters from A-Z, a-z, 0-9, `.`, `_`, `-`, not starting with `-`"
        );
    }
    Ok(())
}

/// Проверяет имя пакета приложения (`com.example.app`): буквы, цифры, `_` и точки между
/// частями. Имя попадает в инструкции агенту, поэтому лишние символы не допускаются.
pub fn validate_package_name(name: &str) -> anyhow::Result<()> {
    let valid = name.len() <= 255
        && name.contains('.')
        && name.split('.').all(|part| {
            part.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if !valid {
        bail!(
            "invalid package name `{name}`: expected an Android package name like com.example.app"
        );
    }
    Ok(())
}

/// Профиль, готовый к работе: настройки и ключ RuStore API.
#[derive(Debug, Clone)]
pub struct Profile {
    pub settings: ProfileSettings,
    pub key: ApiKey,
}

/// Загружает профиль: настройки из `profiles.toml` и ключ из системного хранилища.
///
/// Отсутствие ключа — понятная пользователю ошибка с подсказкой, как его сохранить.
// `serve` загружает профиль через `load_profile_with` с явным хранилищем.
#[allow(dead_code)]
pub fn load_profile(name: &str) -> anyhow::Result<Profile> {
    load_profile_with(&ProfileRepo::open_default()?, &KeyringStore, name)
}

/// То же, что [`load_profile`], с явными файлом профилей и хранилищем ключей.
pub fn load_profile_with(
    repo: &ProfileRepo,
    store: &dyn KeyStore,
    name: &str,
) -> anyhow::Result<Profile> {
    validate_profile_name(name)?;
    let settings = repo.get(name)?.unwrap_or_default();
    let key = match store.get(name) {
        Ok(key) => key,
        Err(err @ (AuthError::NotFound { .. } | AuthError::Corrupted { .. })) => {
            return Err(err.into());
        }
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "cannot load the RuStore API key of profile `{name}`"
            )));
        }
    };
    Ok(Profile { settings, key })
}

#[cfg(test)]
mod tests {
    use rustore_api::auth::MemoryStore;
    use rustore_api::testing;

    use super::*;

    fn sample() -> ProfileSettings {
        ProfileSettings {
            client: Some("codex".into()),
            packages: vec!["com.example.app".into()],
            allow_write: true,
            allow_destructive: false,
            toolsets: vec!["apps".into()],
            dynamic_toolsets: true,
        }
    }

    #[test]
    fn round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("nested"));
        assert_eq!(repo.get("codex").unwrap(), None);

        repo.upsert("codex", sample()).unwrap();
        repo.upsert(DEFAULT_PROFILE, ProfileSettings::default())
            .unwrap();
        assert_eq!(repo.get("codex").unwrap(), Some(sample()));
        assert_eq!(
            repo.get(DEFAULT_PROFILE).unwrap(),
            Some(ProfileSettings::default())
        );

        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("[profiles.codex]"), "{text}");
        assert!(text.contains("allow_write = true"), "{text}");
        assert!(text.contains("packages = [\"com.example.app\"]"), "{text}");
        assert!(text.contains("dynamic_toolsets = true"), "{text}");
        // Пустые необязательные поля и выключенные динамические наборы не записываются.
        assert_eq!(text.matches("packages").count(), 1, "{text}");
        assert_eq!(text.matches("dynamic_toolsets").count(), 1, "{text}");
        assert!(!text.contains("key"), "{text}");

        assert!(repo.remove("codex").unwrap());
        assert!(!repo.remove("codex").unwrap());
        assert_eq!(repo.get("codex").unwrap(), None);
        assert!(repo.get(DEFAULT_PROFILE).unwrap().is_some());
    }

    #[test]
    fn parses_hand_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(
            repo.path(),
            "# comment\n[profiles.ci]\nallow_write = true\npackages = [\"a.b\"]\n\n[profiles.empty]\n",
        )
        .unwrap();
        let ci = repo.get("ci").unwrap().unwrap();
        assert!(ci.allow_write);
        assert!(!ci.allow_destructive);
        assert!(ci.toolsets.is_empty());
        assert!(!ci.dynamic_toolsets);
        assert_eq!(ci.packages, ["a.b"]);
        assert_eq!(repo.get("empty").unwrap(), Some(ProfileSettings::default()));
    }

    #[test]
    fn destructive_implies_write() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        // Файл, написанный вручную: удаление без записи.
        fs::write(repo.path(), "[profiles.old]\nallow_destructive = true\n").unwrap();
        let old = repo.get("old").unwrap().unwrap();
        assert!(!old.allow_write);
        assert!(old.writes_allowed());

        let settings = ProfileSettings {
            allow_destructive: true,
            ..ProfileSettings::default()
        };
        repo.upsert("new", settings).unwrap();
        let saved = repo.get("new").unwrap().unwrap();
        assert!(saved.allow_write);
        assert!(saved.allow_destructive);
    }

    #[test]
    fn rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        for text in [
            "[profiles.ci]\ntoken = \"x\"\n",
            "[profiles.ci]\nkey_id = \"x\"\n",
            "[profiles.ci.org]\ntype = \"cloud\"\nid = \"1\"\n",
        ] {
            fs::write(repo.path(), text).unwrap();
            assert!(repo.get("ci").is_err(), "{text}");
        }
    }

    #[test]
    fn validates_profile_names() {
        for good in ["default", "claude-code", "ci_1", "a.b"] {
            validate_profile_name(good).unwrap();
        }
        for bad in ["", "-x", "a b", "a/b", "проф", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validates_package_names() {
        for good in ["com.example.app", "ru.a_b.c1"] {
            validate_package_name(good).unwrap();
        }
        for bad in [
            "",
            "app",
            "com..app",
            "1com.app",
            "com.app-x",
            "com.app;rm",
            ".com.app",
        ] {
            assert!(validate_package_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn load_profile_requires_key() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        repo.upsert("codex", sample()).unwrap();

        let err = load_profile_with(&repo, &store, "codex").unwrap_err();
        assert!(
            err.to_string().contains("auth login --profile codex"),
            "{err}"
        );

        store.set("codex", &testing::api_key()).unwrap();
        let profile = load_profile_with(&repo, &store, "codex").unwrap();
        assert_eq!(profile.settings, sample());
        assert_eq!(profile.key.key_id().as_str(), testing::TEST_KEY_ID);

        // Ключ без записи в файле профилей: настройки по умолчанию.
        store.set("bare", &testing::api_key()).unwrap();
        let profile = load_profile_with(&repo, &store, "bare").unwrap();
        assert_eq!(profile.settings, ProfileSettings::default());
    }
}
