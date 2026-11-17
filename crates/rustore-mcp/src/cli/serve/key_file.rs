//! Ключ RuStore API из файла секрета (`serve --http --key-file`).
//!
//! Единственное исключение из правила «ключ только в системном хранилище»: в контейнере
//! (Docker, Kubernetes) системного хранилища нет, и ключ приходит файлом секрета
//! (`/run/secrets/...`). Файл должен быть обычным файлом без прав для группы и остальных
//! (0600 или 0400): ключ, который может прочитать любой пользователь хоста, — ошибка
//! запуска, а не предупреждение. Права проверяются у уже открытого файла (`fstat`), поэтому
//! подмена файла между проверкой и чтением не проходит. Символическая ссылка допускается
//! (так монтирует секреты Kubernetes): проверяется файл, на который она ведёт.
//!
//! Файл — JSON `{"keyId", "privateKey"}` или только приватный ключ; тогда keyId задаёт
//! `--key-id`. Содержимое файла в ответы MCP не попадает; в тексте ошибки разбора — только
//! причина без фрагментов ключа ([`rustore_api::key::KeyError`]).
//!
//! Так же, с теми же проверками прав, читается файл секрета push (`--push-file`): JSON
//! `{"projectId", "serviceToken"}` ([`load_push`]).

use std::io::Read;
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use rustore_api::ApiKey;
use rustore_api::key::MAX_KEY_FILE_BYTES;
use rustore_api::push::PushCredentials;
use zeroize::Zeroizing;

/// Открывает файл секрета, проверяет тип и права и читает его не больше `limit` байт.
/// `what` — название файла в ошибках («key file», «push file»).
fn read_secret_file(path: &Path, what: &str, limit: usize) -> anyhow::Result<Zeroizing<String>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open the {what} {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot read the {what} {}", path.display()))?;
    if !metadata.is_file() {
        bail!("the {what} {} is not a regular file", path.display());
    }
    check_permissions(path, what, &metadata)?;
    // Лимит + 1: файл больше лимита распознаётся без чтения его целиком.
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read the {what} {}", path.display()))?;
    if bytes.len() > limit {
        bail!("the {what} {} is larger than {limit} bytes", path.display());
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("the {what} {} is not UTF-8 text", path.display()))?;
    Ok(Zeroizing::new(text.to_owned()))
}

/// Предел файла секрета push: JSON с идентификатором проекта и сервисным токеном.
const MAX_PUSH_FILE_BYTES: usize = 8 * 1024;

/// Читает проект push и сервисный токен из файла секрета (`--push-file`).
pub fn load_push(path: &Path) -> anyhow::Result<PushCredentials> {
    let text = read_secret_file(path, "push file", MAX_PUSH_FILE_BYTES)?;
    PushCredentials::from_stored(&text).map_err(|err| {
        anyhow!(
            "the push file {} is not valid: {err}; it must be JSON {{\"projectId\": ..., \
             \"serviceToken\": ...}}",
            path.display()
        )
    })
}

/// Читает и проверяет ключ RuStore API из файла секрета.
pub fn load(path: &Path, key_id: Option<&str>) -> anyhow::Result<ApiKey> {
    let text = read_secret_file(path, "key file", MAX_KEY_FILE_BYTES)?;
    match ApiKey::parse_key_file(&text, key_id) {
        Ok(Some(key)) => Ok(key),
        Ok(None) => bail!(
            "the key file {} holds only the private key: pass its key ID with --key-id",
            path.display()
        ),
        Err(err) => bail!(
            "the key file {} is not a valid RuStore API key: {err}",
            path.display()
        ),
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path, what: &str, metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "the {what} {} is accessible by other users (mode {mode:04o}): allow access \
             only for its owner (chmod 600, or mode 0400 for a Docker secret)",
            path.display()
        );
    }
    Ok(())
}

/// Без прав Unix проверить, что файл закрыт от других пользователей, нельзя, а файлы
/// секретов Docker и Kubernetes — механизм Linux: на других системах ключ берётся из
/// системного хранилища.
#[cfg(not(unix))]
fn check_permissions(path: &Path, what: &str, _metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    bail!(
        "the {what} {} is supported only on Unix (Docker and Kubernetes secrets); store the \
         credentials in the system keychain with `rustore-mcp auth login` (or `auth push \
         login`) and pass --profile instead",
        path.display()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use rustore_api::testing;

    use super::*;

    fn write(dir: &Path, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn key_json() -> String {
        serde_json::json!({
            "keyId": testing::TEST_KEY_ID,
            "privateKey": testing::private_key_base64()
        })
        .to_string()
    }

    #[test]
    fn reads_owner_only_key_file() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o600, 0o400] {
            let path = write(dir.path(), &format!("key-{mode:o}"), &key_json(), mode);
            let key = load(&path, None).unwrap();
            assert_eq!(key.key_id().as_str(), testing::TEST_KEY_ID);
        }
        // Ссылка на закрытый файл (так монтирует секреты Kubernetes) допускается.
        let target = dir.path().join("key-600");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(load(&link, None).is_ok());

        // Только приватный ключ: keyId из флага.
        let path = write(dir.path(), "bare", &testing::private_key_base64(), 0o600);
        let err = load(&path, None).unwrap_err().to_string();
        assert!(err.contains("--key-id"), "{err}");
        assert_eq!(
            load(&path, Some("flag-id")).unwrap().key_id().as_str(),
            "flag-id"
        );
    }

    #[test]
    fn rejects_open_or_broken_key_files() {
        let dir = tempfile::tempdir().unwrap();
        let json = key_json();
        for mode in [0o644, 0o640, 0o604, 0o660] {
            let path = write(dir.path(), &format!("key-{mode:o}"), &json, mode);
            let err = load(&path, None).unwrap_err().to_string();
            assert!(err.contains("accessible by other users"), "{err}");
            assert!(err.contains(&format!("{mode:04o}")), "{err}");
        }

        let err = load(&dir.path().join("missing"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot open the key file"), "{err}");
        let err = load(dir.path(), None).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");

        let path = write(
            dir.path(),
            "big",
            &"x".repeat(MAX_KEY_FILE_BYTES + 1),
            0o600,
        );
        let err = load(&path, None).unwrap_err().to_string();
        assert!(err.contains("larger than"), "{err}");
        assert!(err.contains("key file"), "{err}");

        // Ошибка разбора не повторяет содержимое файла.
        let broken = r#"{"keyId": "id-1", "privateKey": "AAAAsecretish"}"#;
        let path = write(dir.path(), "broken", broken, 0o600);
        let err = format!("{:#}", load(&path, None).unwrap_err());
        assert!(err.contains("not a valid RuStore API key"), "{err}");
        assert!(!err.contains("AAAAsecretish"), "{err}");
    }

    #[test]
    fn reads_push_files_like_key_files() {
        let dir = tempfile::tempdir().unwrap();
        let token = "push-service-token-abcdef0123456789";
        let json =
            serde_json::json!({"projectId": "myproject-b5ae1", "serviceToken": token}).to_string();
        let path = write(dir.path(), "push", &json, 0o400);
        assert_eq!(load_push(&path).unwrap().project_id(), "myproject-b5ae1");

        let open = write(dir.path(), "push-open", &json, 0o644);
        let err = load_push(&open).unwrap_err().to_string();
        assert!(err.contains("push file"), "{err}");
        assert!(err.contains("accessible by other users"), "{err}");

        // Только токен без JSON — ошибка без значения токена.
        let bare = write(dir.path(), "push-bare", token, 0o600);
        let err = format!("{:#}", load_push(&bare).unwrap_err());
        assert!(err.contains("projectId"), "{err}");
        assert!(!err.contains(token), "{err}");
    }
}
