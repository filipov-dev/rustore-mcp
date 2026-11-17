//! `rustore-mcp auth push` — второй источник учётных данных профиля: проект push и
//! сервисный токен Push API RuStore.
//!
//! Токен принимается только из файла (`--token-file`) или из stdin (`--token-stdin`), не из
//! аргумента и не из переменной окружения, хранится только в системном хранилище (запись
//! `<profile>/push`) и нигде не печатается. Безопасной проверки токена в Push API нет (любой
//! вызов отправляет или меняет подписки), поэтому при входе он не проверяется.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use rustore_api::auth::{AuthError, KEYRING_SERVICE, KeyStore};
use rustore_api::push::{CONSOLE_HINT, MAX_SERVICE_TOKEN_LEN, PushCredentials};
use zeroize::Zeroizing;

use crate::profiles::{DEFAULT_PROFILE, validate_profile_name};
use crate::ui::Ui;

/// Предел размера файла с токеном: токен — одна строка, JSON — две короткие строки.
const MAX_TOKEN_FILE_BYTES: usize = MAX_SERVICE_TOKEN_LEN + 1024;

#[derive(Debug, Subcommand)]
pub enum PushCommand {
    /// Store the push project ID and service token of a profile (RuStore Push API).
    ///
    /// The service token is read from a file (--token-file) or from stdin (--token-stdin),
    /// never from a command-line value or an environment variable. The file holds either
    /// only the token or JSON {"projectId": ..., "serviceToken": ...}; with only the token,
    /// pass --project-id. Both are shown in the RuStore console: the app > Push
    /// notifications > Projects. The token is saved in the system keychain only, is sent
    /// only to the RuStore push hosts and is not checked here: every Push API call sends
    /// a notification or changes subscriptions. Replaces the stored push credentials if
    /// there are any.
    Login(PushLoginArgs),
    /// Remove the stored push project ID and service token of the profile.
    Logout(PushLogoutArgs),
}

#[derive(Debug, Clone, Args)]
pub struct PushLoginArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Push project ID (project_id) from the RuStore console. Needed when the token file
    /// holds only the token.
    #[arg(long, value_name = "ID")]
    pub project_id: Option<String>,
    /// File with the service token, or JSON with projectId and serviceToken.
    #[arg(long, value_name = "PATH", conflicts_with = "token_stdin")]
    pub token_file: Option<PathBuf>,
    /// Read the service token (or the JSON) from stdin instead of a file.
    #[arg(long)]
    pub token_stdin: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PushLogoutArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
}

/// Читает токен из потока не больше [`MAX_TOKEN_FILE_BYTES`]. Содержимое в ошибки не
/// попадает.
fn read_token_text(reader: &mut dyn Read, origin: &str) -> anyhow::Result<Zeroizing<String>> {
    let mut text = Zeroizing::new(String::new());
    reader
        .take(MAX_TOKEN_FILE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("cannot read the service token from {origin} as text"))?;
    if text.len() > MAX_TOKEN_FILE_BYTES {
        bail!(
            "{origin} is larger than {MAX_TOKEN_FILE_BYTES} bytes: it is not a push service token"
        );
    }
    Ok(text)
}

fn read_token_file(path: &Path) -> anyhow::Result<Zeroizing<String>> {
    let display = path.display();
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot open the token file `{display}`"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot read the token file `{display}`"))?;
    if !metadata.is_file() {
        bail!("`{display}` is not a regular file");
    }
    read_token_text(&mut file, &format!("`{display}`"))
}

/// Учётные данные из текста: JSON с projectId и serviceToken или только токен с
/// `--project-id`. Значение токена в ошибки не попадает.
fn parse_credentials(text: &str, project_id: Option<&str>) -> anyhow::Result<PushCredentials> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        let credentials = PushCredentials::from_stored(trimmed)?;
        if let Some(flag) = project_id
            && flag.trim() != credentials.project_id()
        {
            bail!("--project-id does not match projectId in the token file");
        }
        return Ok(credentials);
    }
    let Some(project_id) = project_id else {
        bail!(
            "pass --project-id: the token file holds only the service token. The project ID \
             is shown next to the token in the {CONSOLE_HINT}"
        );
    };
    Ok(PushCredentials::new(project_id, trimmed)?)
}

/// `auth push login`. `stdin` — поток токена для `--token-stdin` (в тестах — подмена).
pub fn login(
    store: &dyn KeyStore,
    ui: &mut dyn Ui,
    args: &PushLoginArgs,
    stdin: Option<&mut dyn Read>,
) -> anyhow::Result<()> {
    validate_profile_name(&args.profile)?;
    let text = match (&args.token_file, args.token_stdin, stdin) {
        (Some(path), _, _) => read_token_file(path)?,
        (None, true, Some(reader)) => read_token_text(reader, "stdin")?,
        (None, true, None) => bail!("--token-stdin: stdin is not available"),
        (None, false, _) => bail!(
            "pass the service token with --token-file <file> or --token-stdin; it is never \
             read from a command-line value. Get it in the {CONSOLE_HINT}"
        ),
    };
    let credentials = parse_credentials(&text, args.project_id.as_deref())?;
    store.set_push(&args.profile, &credentials)?;
    let profile = &args.profile;
    ui.say(&format!(
        "Push project {} of profile `{profile}` saved in the system keychain (service \
         `{KEYRING_SERVICE}`). The service token is not checked: test it by sending a \
         notification to your own device.",
        credentials.masked_project_id()
    ));
    ui.say(&format!(
        "Push tools are off by default: start the server with --toolsets push (plus \
         --allow-destructive to send notifications), or `rustore-mcp connect --profile \
         {profile} --toolsets push --allow-destructive`."
    ));
    if let Some(path) = &args.token_file {
        offer_to_delete_token_file(ui, path);
    }
    Ok(())
}

fn offer_to_delete_token_file(ui: &mut dyn Ui, path: &Path) {
    if !ui.is_interactive() {
        return;
    }
    let question = format!(
        "Delete the token file {}? The server only needs the keychain copy; the token stays \
         available in the {CONSOLE_HINT}.",
        path.display()
    );
    match ui.confirm(&question, false) {
        Ok(true) => match fs::remove_file(path) {
            Ok(()) => ui.say(&format!("Deleted {}.", path.display())),
            Err(err) => ui.say(&format!(
                "Could not delete {}: {err}. Delete it yourself.",
                path.display()
            )),
        },
        Ok(false) => ui.say(&format!(
            "Kept {}. Store it safely or delete it.",
            path.display()
        )),
        // Токен уже сохранён: ошибка ввода не делает команду неудачной.
        Err(_) => {}
    }
}

/// `auth push logout`.
pub fn logout(store: &dyn KeyStore, ui: &mut dyn Ui, profile: &str) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    match store.delete_push(profile) {
        Ok(()) => ui.say(&format!(
            "Push credentials of profile `{profile}` removed from the system keychain. If the \
             service token is no longer needed anywhere, delete it in the {CONSOLE_HINT}."
        )),
        Err(AuthError::PushNotFound { .. }) => {
            ui.say(&format!(
                "No push credentials stored for profile `{profile}`."
            ));
        }
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

/// Строка `Push:` для `auth status` и `serve --status`. Ни токен, ни проект целиком не
/// выводятся.
pub fn describe(store: &dyn KeyStore, profile: &str) -> anyhow::Result<String> {
    Ok(match store.get_push(profile) {
        Ok(credentials) => format!(
            "configured: project {}, service token in the system keychain (not checked)",
            credentials.masked_project_id()
        ),
        Err(AuthError::PushNotFound { .. }) => format!(
            "not configured; for push notifications run `rustore-mcp auth push login \
             --profile {profile} --project-id <ID> --token-file <file>`"
        ),
        Err(err @ AuthError::PushCorrupted { .. }) => err.to_string(),
        Err(err) => return Err(err.into()),
    })
}

#[cfg(test)]
mod tests {
    use rustore_api::auth::MemoryStore;

    use super::*;
    use crate::ui::testing::ScriptedUi;

    const TOKEN: &str = "push-service-token-abcdef0123456789";
    const PROJECT: &str = "myproject-b5ae1";

    fn args(profile: &str) -> PushLoginArgs {
        PushLoginArgs {
            profile: profile.into(),
            project_id: None,
            token_file: None,
            token_stdin: false,
        }
    }

    fn no_stdin() -> Option<&'static mut dyn Read> {
        None
    }

    #[test]
    fn login_from_a_file_and_from_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();

        // Файл только с токеном и `--project-id`.
        let file = dir.path().join("token.txt");
        fs::write(&file, format!("{TOKEN}\n")).unwrap();
        let mut ui = ScriptedUi::default();
        let login_args = PushLoginArgs {
            project_id: Some(PROJECT.into()),
            token_file: Some(file.clone()),
            ..args("codex")
        };
        login(&store, &mut ui, &login_args, no_stdin()).unwrap();
        let stored = store.get_push("codex").unwrap();
        assert_eq!(stored, PushCredentials::new(PROJECT, TOKEN).unwrap());
        let text = ui.text();
        assert!(!text.contains(TOKEN), "{text}");
        assert!(!text.contains(PROJECT), "{text}");
        assert!(text.contains("--toolsets push"), "{text}");
        // Без терминала файл не удаляется и вопроса нет.
        assert!(file.exists());
        assert!(ui.questions.is_empty());

        // stdin с JSON: проект берётся из него.
        let json = format!(r#"{{"projectId": "other-project1", "serviceToken": "{TOKEN}"}}"#);
        let mut reader = json.as_bytes();
        let stdin_args = PushLoginArgs {
            token_stdin: true,
            ..args("ci")
        };
        login(
            &store,
            &mut ScriptedUi::default(),
            &stdin_args,
            Some(&mut reader),
        )
        .unwrap();
        assert_eq!(store.get_push("ci").unwrap().project_id(), "other-project1");

        let line = describe(&store, "ci").unwrap();
        assert!(line.starts_with("configured"), "{line}");
        assert!(!line.contains(TOKEN), "{line}");
        assert!(!line.contains("other-project1"), "{line}");

        let mut ui = ScriptedUi::default();
        logout(&store, &mut ui, "ci").unwrap();
        assert!(ui.text().contains("removed"), "{}", ui.text());
        assert!(describe(&store, "ci").unwrap().contains("auth push login"));
        let mut ui = ScriptedUi::default();
        logout(&store, &mut ui, "ci").unwrap();
        assert!(ui.text().contains("No push credentials"), "{}", ui.text());
        // Ключ Public API профиля не трогается.
        assert!(!store.contains("ci").unwrap());
    }

    #[test]
    fn bad_input_stores_nothing_and_hides_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let file = dir.path().join("token.txt");
        fs::write(&file, TOKEN).unwrap();

        // Без проекта, без источника, неверный проект, расхождение с JSON, токен с пробелом.
        let cases: Vec<(PushLoginArgs, &str)> = vec![
            (
                PushLoginArgs {
                    token_file: Some(file.clone()),
                    ..args("p")
                },
                "--project-id",
            ),
            (args("p"), "--token-file"),
            (
                PushLoginArgs {
                    token_file: Some(file.clone()),
                    project_id: Some("../x".into()),
                    ..args("p")
                },
                "project ID",
            ),
            (
                PushLoginArgs {
                    token_stdin: true,
                    ..args("p")
                },
                "stdin is not available",
            ),
        ];
        for (login_args, expected) in cases {
            let err =
                login(&store, &mut ScriptedUi::default(), &login_args, no_stdin()).unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(expected), "{expected}: {text}");
            assert!(!text.contains(TOKEN), "{text}");
        }

        let json = format!(r#"{{"projectId": "{PROJECT}", "serviceToken": "{TOKEN}"}}"#);
        let mut reader = json.as_bytes();
        let err = login(
            &store,
            &mut ScriptedUi::default(),
            &PushLoginArgs {
                token_stdin: true,
                project_id: Some("another".into()),
                ..args("p")
            },
            Some(&mut reader),
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        let spaced = "token with spaces-0123456789";
        let mut reader = spaced.as_bytes();
        let err = login(
            &store,
            &mut ScriptedUi::default(),
            &PushLoginArgs {
                token_stdin: true,
                project_id: Some(PROJECT.into()),
                ..args("p")
            },
            Some(&mut reader),
        )
        .unwrap_err();
        assert!(!format!("{err:#}").contains(spaced), "{err:#}");

        let big = "x".repeat(MAX_TOKEN_FILE_BYTES + 1);
        let mut reader = big.as_bytes();
        assert!(
            login(
                &store,
                &mut ScriptedUi::default(),
                &PushLoginArgs {
                    token_stdin: true,
                    project_id: Some(PROJECT.into()),
                    ..args("p")
                },
                Some(&mut reader),
            )
            .is_err()
        );
        assert!(matches!(
            store.get_push("p"),
            Err(AuthError::PushNotFound { .. })
        ));
    }

    #[test]
    fn terminal_offers_to_delete_the_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        let file = dir.path().join("token.txt");
        fs::write(&file, TOKEN).unwrap();
        let mut ui = ScriptedUi::answering(&["y"]);
        login(
            &store,
            &mut ui,
            &PushLoginArgs {
                token_file: Some(file.clone()),
                project_id: Some(PROJECT.into()),
                ..args("term")
            },
            no_stdin(),
        )
        .unwrap();
        assert!(!file.exists(), "{}", ui.text());
        assert_eq!(ui.questions.len(), 1);
    }
}
