//! Приём нового ключа RuStore API: общая часть `auth login` и `connect`.
//!
//! Ключ принимается только из файла (флагом или ответом на вопрос) или из stdin, не
//! содержимым в аргументе или переменной окружения. Файл ключа — JSON `{"keyId",
//! "privateKey"}` или только приватный ключ (Base64 из консоли RuStore или PEM); keyId тогда
//! задаётся флагом `--key-id` или, в терминале, ответом на вопрос. Ключ проверяется
//! (разбор, размер RSA), по умолчанию — пробным получением токена и запросом списка
//! приложений, и только после этого сохраняется в системном хранилище.
//!
//! Удалить исходный файл предлагает вызывающая команда ([`offer_to_delete_key_file`]),
//! один раз и в самом конце.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rustore_api::ApiKey;
use rustore_api::auth::{KEYRING_SERVICE, KeyStore};
use rustore_api::key::{CONSOLE_HINT, MAX_KEY_FILE_BYTES};
use zeroize::Zeroizing;

use crate::ui::Ui;
use crate::verify::KeyVerifier;

/// Откуда взять ключ.
pub enum KeySource<'a> {
    /// Файл ключа (`--key-file`).
    File(&'a Path),
    /// Поток (`--key-stdin`): stdin процесса или его подмена в тестах. Вопросов нет: stdin
    /// занят ключом.
    Reader(&'a mut dyn Read),
    /// Не задан: в терминале спросить путь к файлу.
    Ask,
}

/// Новый ключ, сохранённый в системном хранилище.
#[derive(Debug)]
pub struct NewKey {
    /// Исходный файл ключа (если ключ пришёл из файла): его можно предложить удалить.
    pub file: Option<PathBuf>,
}

/// Путь из ответа пользователя: без кавычек, которые терминал добавляет при
/// перетаскивании файла, с `\ ` вместо пробелов и `~/` в начале.
pub fn parse_path_answer(answer: &str) -> Option<PathBuf> {
    let mut text = answer.trim();
    for quote in ['"', '\''] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            text = &text[1..text.len() - 1];
        }
    }
    // `\ ` — пробел, экранированный оболочкой Unix (перетаскивание файла в терминал). На
    // Windows `\` — разделитель пути, а путь с пробелами Проводник берёт в кавычки.
    let text = if cfg!(windows) {
        text.to_owned()
    } else {
        text.replace("\\ ", " ")
    };
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text
        .strip_prefix("~/")
        .or_else(|| text.strip_prefix("~\\").filter(|_| cfg!(windows)))
        && let Some(home) = dirs::home_dir()
    {
        return Some(home.join(rest));
    }
    Some(PathBuf::from(text))
}

/// Читает текст ключа из потока не больше [`MAX_KEY_FILE_BYTES`]. Содержимое в ошибки не
/// попадает.
pub fn read_key_text(reader: &mut dyn Read, origin: &str) -> anyhow::Result<Zeroizing<String>> {
    let mut text = Zeroizing::new(String::new());
    reader
        .take(MAX_KEY_FILE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("cannot read the key from {origin} as text"))?;
    if text.len() > MAX_KEY_FILE_BYTES {
        bail!("{origin} is larger than {MAX_KEY_FILE_BYTES} bytes: it is not a RuStore API key");
    }
    Ok(text)
}

/// Читает текст файла ключа. Содержимое в ошибки не попадает.
pub fn read_key_file(path: &Path) -> anyhow::Result<Zeroizing<String>> {
    let display = path.display();
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot open the key file `{display}`"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot read the key file `{display}`"))?;
    if !metadata.is_file() {
        bail!("`{display}` is not a regular file");
    }
    read_key_text(&mut file, &format!("`{display}`"))
}

/// Ключ из текста файла: keyId из файла, из `key_id` или, если можно спрашивать, из ответа
/// пользователя.
pub fn parse_key(
    ui: &mut dyn Ui,
    text: &str,
    key_id: Option<&str>,
    origin: &str,
    can_ask: bool,
) -> anyhow::Result<ApiKey> {
    let parse = |key_id: Option<&str>| {
        ApiKey::parse_key_file(text, key_id)
            .with_context(|| format!("{origin} is not a usable RuStore API key"))
    };
    if let Some(key) = parse(key_id)? {
        return Ok(key);
    }
    if !can_ask {
        bail!(
            "{origin} holds only the private key; pass its key ID with --key-id <ID> (shown \
             next to the key in the {CONSOLE_HINT})"
        );
    }
    let answer = ui
        .ask(&format!(
            "Key ID (keyId) of this key, from the {CONSOLE_HINT}: "
        ))
        .context("failed to read the key ID; pass --key-id <ID>")?;
    if answer.trim().is_empty() {
        bail!("no key ID given; pass --key-id <ID>");
    }
    parse(Some(answer.trim()))?
        .with_context(|| format!("{origin} holds only the private key; pass --key-id <ID>"))
}

/// Читает ключ из источника; для файла возвращает и его путь.
fn read_from_source(
    ui: &mut dyn Ui,
    source: KeySource<'_>,
    key_id: Option<&str>,
) -> anyhow::Result<(ApiKey, Option<PathBuf>)> {
    match source {
        KeySource::File(path) => {
            let text = read_key_file(path)?;
            let origin = format!("`{}`", path.display());
            let can_ask = ui.is_interactive();
            let key = parse_key(ui, &text, key_id, &origin, can_ask)?;
            Ok((key, Some(path.to_path_buf())))
        }
        KeySource::Reader(reader) => {
            let text = read_key_text(reader, "stdin")?;
            Ok((parse_key(ui, &text, key_id, "stdin", false)?, None))
        }
        KeySource::Ask => {
            if !ui.is_interactive() {
                bail!("no key given; pass --key-file <PATH> or --key-stdin");
            }
            ui.say(&format!(
                "Create a key in the {CONSOLE_HINT}: tick the API methods and the apps it may \
                 use, then save the private key to a file."
            ));
            let answer = ui
                .ask("Path to the file with the private key: ")
                .context("failed to read the key file path; pass --key-file <PATH>")?;
            let path = parse_path_answer(&answer)
                .context("no key file given; pass --key-file <PATH> or --key-stdin")?;
            let text = read_key_file(&path)?;
            let origin = format!("`{}`", path.display());
            let key = parse_key(ui, &text, key_id, &origin, true)?;
            Ok((key, Some(path)))
        }
    }
}

/// Читает новый ключ, проверяет его (если `verifier` задан) и сохраняет в системном
/// хранилище. Если чтение или проверка не прошли, ничего не сохраняется.
pub async fn store_new_key<V: KeyVerifier>(
    store: &dyn KeyStore,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
    source: KeySource<'_>,
    key_id: Option<&str>,
) -> anyhow::Result<NewKey> {
    let (key, file) = read_from_source(ui, source, key_id)?;
    ui.say(&format!(
        "RuStore API key {} (RSA {} bits).",
        key.key_id().masked(),
        key.bits()
    ));

    match verifier {
        Some(verifier) => {
            let outcome = verifier.check(&key).await.map_err(|err| {
                anyhow::anyhow!(
                    "{err:#}. Nothing was stored. Re-run with --no-verify to store the key \
                     without verification."
                )
            })?;
            ui.say(&format!("Key is valid: {outcome}."));
        }
        None => ui.say("Skipping the key check (--no-verify)."),
    }

    let replaced = store.contains(profile)?;
    store.set(profile, &key)?;
    ui.say(&format!(
        "RuStore API key {} in the system keychain (service `{KEYRING_SERVICE}`, account \
         `{profile}`).",
        if replaced { "replaced" } else { "saved" }
    ));
    Ok(NewKey { file })
}

/// Предлагает удалить исходный файл ключа: только в терминале, по умолчанию — нет
/// (RuStore показывает приватный ключ только при создании, и файл может быть единственной
/// резервной копией). Без терминала (CI, скрипты) вопроса нет и файл не трогается.
pub fn offer_to_delete_key_file(ui: &mut dyn Ui, key_file: &Path) {
    if !ui.is_interactive() {
        return;
    }
    let question = format!(
        "Delete the key file {}? RuStore shows the private key only when the key is created, \
         so keep a backup (for example, in a password manager) before deleting it.",
        key_file.display()
    );
    match ui.confirm(&question, false) {
        Ok(true) => match fs::remove_file(key_file) {
            Ok(()) => ui.say(&format!("Deleted {}.", key_file.display())),
            Err(err) => ui.say(&format!(
                "Could not delete {}: {err}. Delete it yourself.",
                key_file.display()
            )),
        },
        Ok(false) => ui.say(&format!(
            "Kept {}. Store it safely or delete it: the server only needs the keychain copy.",
            key_file.display()
        )),
        // Ошибка ввода здесь не повод считать команду неудачной: ключ уже сохранён.
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use rustore_api::testing;

    use super::*;
    use crate::ui::testing::ScriptedUi;

    #[test]
    fn path_answers() {
        assert_eq!(parse_path_answer("  "), None);
        assert_eq!(
            parse_path_answer("'/tmp/key.pem' "),
            Some(PathBuf::from("/tmp/key.pem"))
        );
        assert_eq!(
            parse_path_answer("\"/tmp/a b.pem\""),
            Some(PathBuf::from("/tmp/a b.pem"))
        );
        if !cfg!(windows) {
            assert_eq!(
                parse_path_answer("/tmp/a\\ b.pem"),
                Some(PathBuf::from("/tmp/a b.pem"))
            );
        }
        if let Some(home) = dirs::home_dir() {
            assert_eq!(parse_path_answer("~/k.pem"), Some(home.join("k.pem")));
        }
    }

    #[test]
    fn oversized_input_is_rejected() {
        let big = "x".repeat(MAX_KEY_FILE_BYTES + 10);
        let err = read_key_text(&mut big.as_bytes(), "stdin").unwrap_err();
        assert!(format!("{err:#}").contains("larger than"), "{err:#}");
    }

    #[test]
    fn key_id_comes_from_the_file_flag_or_answer() {
        let private = testing::private_key_base64();
        let json =
            serde_json::json!({"keyId": testing::TEST_KEY_ID, "privateKey": private}).to_string();
        let mut ui = ScriptedUi::default();
        let key = parse_key(&mut ui, &json, None, "f", false).unwrap();
        assert_eq!(key.key_id().as_str(), testing::TEST_KEY_ID);

        let key = parse_key(&mut ui, &private, Some("id-from-flag"), "f", false).unwrap();
        assert_eq!(key.key_id().as_str(), "id-from-flag");

        let err = parse_key(&mut ui, &private, None, "stdin", false).unwrap_err();
        assert!(err.to_string().contains("--key-id"), "{err}");
        assert!(ui.questions.is_empty());

        let mut ui = ScriptedUi::answering(&["id-from-answer"]);
        let key = parse_key(&mut ui, &private, None, "f", true).unwrap();
        assert_eq!(key.key_id().as_str(), "id-from-answer");
        assert_eq!(ui.questions.len(), 1);

        // Испорченный ключ не приводит к вопросу и не выводится в ошибке.
        let mut ui = ScriptedUi::answering(&["x"]);
        let err = parse_key(&mut ui, "AAAAnot-a-key", None, "f", true).unwrap_err();
        assert!(ui.questions.is_empty(), "{err:#}");
        assert!(!format!("{err:#}").contains("AAAAnot-a-key"), "{err:#}");
    }
}
