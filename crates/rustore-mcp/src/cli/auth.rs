//! `rustore-mcp auth` — ключи RuStore API профилей.
//!
//! Ключ (keyId и приватный ключ) принимается только из файла (`--key-file`, ответ на вопрос
//! в терминале) или из stdin (`--key-stdin`), хранится только в системном хранилище и нигде
//! не печатается. Команды, которая выводит ключ или токен, нет и быть не должно.

use std::io::Read;
use std::path::PathBuf;

use anyhow::bail;
use clap::{Args, Subcommand};
use rustore_api::auth::{AuthError, KEYRING_SERVICE, KeyStore, KeyringStore};
use rustore_api::key::CONSOLE_HINT;

use crate::login::{KeySource, offer_to_delete_key_file, store_new_key};
use crate::profiles::{
    DEFAULT_PROFILE, ProfileRepo, ProfileSettings, validate_package_name, validate_profile_name,
};
use crate::ui::{NonInteractiveUi, TerminalUi, Ui};
use crate::verify::{ApiVerifier, KeyVerifier};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a RuStore API key (key ID and private key) for a profile.
    ///
    /// The key is read from a file (--key-file, or a path asked in a terminal) or from stdin
    /// (--key-stdin), never from a command-line value or an environment variable. The file
    /// is either JSON {"keyId": ..., "privateKey": ...} or only the private key (the Base64
    /// text shown when the key is created, or PEM); then pass --key-id or answer the
    /// question. The key is checked by getting a token and listing the apps
    /// (GET /public/v1/application) and saved in the system keychain only if the check
    /// passes. It is never written to profiles.toml. Replaces the stored key if there is
    /// one.
    Login(LoginArgs),
    /// Show the profile, its key ID (masked) and whether RuStore accepts the key.
    ///
    /// The private key and the token are never printed.
    Status(StatusArgs),
    /// Remove the stored key of the profile from the system keychain.
    Logout(LogoutArgs),
}

#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Key ID (keyId) of the RuStore API key, shown next to the key in the RuStore console.
    /// Needed when the key file holds only the private key.
    #[arg(long, value_name = "ID")]
    pub key_id: Option<String>,
    /// File with the key: JSON with keyId and privateKey, or only the private key.
    #[arg(long, value_name = "PATH", conflicts_with = "key_stdin")]
    pub key_file: Option<PathBuf>,
    /// Read the key from stdin instead of a file. No questions are asked in this mode.
    #[arg(long)]
    pub key_stdin: bool,
    /// Default package name of the profile's apps (repeatable), shown to the agent.
    /// Replaces the saved list.
    #[arg(long = "package", value_name = "NAME")]
    pub packages: Vec<String>,
    /// Store the key without checking it against the RuStore API.
    #[arg(long)]
    pub no_verify: bool,
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Do not get a token and query the RuStore API.
    #[arg(long)]
    pub no_verify: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LogoutArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Also remove the profile settings from profiles.toml.
    #[arg(long)]
    pub forget: bool,
}

pub async fn run(command: AuthCommand) -> anyhow::Result<()> {
    let store = KeyringStore;
    let repo = ProfileRepo::open_default()?;
    match command {
        AuthCommand::Login(args) => {
            let verifier = if args.no_verify {
                None
            } else {
                Some(ApiVerifier::new()?)
            };
            if args.key_stdin {
                // stdin занят ключом: вопросов нет.
                let mut stdin = std::io::stdin().lock();
                login(
                    &store,
                    &repo,
                    verifier.as_ref(),
                    &mut NonInteractiveUi,
                    &args,
                    Some(&mut stdin),
                )
                .await
            } else {
                login(
                    &store,
                    &repo,
                    verifier.as_ref(),
                    &mut TerminalUi,
                    &args,
                    None,
                )
                .await
            }
        }
        AuthCommand::Status(args) => {
            let verifier = if args.no_verify {
                None
            } else {
                Some(ApiVerifier::new()?)
            };
            status(
                &store,
                &repo,
                verifier.as_ref(),
                &mut TerminalUi,
                &args.profile,
            )
            .await
        }
        AuthCommand::Logout(args) => {
            logout(&store, &repo, &mut TerminalUi, &args.profile, args.forget)
        }
    }
}

/// `auth login`: читает ключ, проверяет и сохраняет; при `--package` обновляет профиль.
/// `stdin` — поток ключа для `--key-stdin` (в тестах — подмена).
pub async fn login<V: KeyVerifier>(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    args: &LoginArgs,
    stdin: Option<&mut dyn Read>,
) -> anyhow::Result<()> {
    validate_profile_name(&args.profile)?;
    for package in &args.packages {
        validate_package_name(package)?;
    }
    // Файл профилей читается до ключа: испорченный файл не должен оставить ключ без профиля.
    let settings = repo.get(&args.profile)?;
    let source = match (&args.key_file, args.key_stdin, stdin) {
        (Some(path), _, _) => KeySource::File(path),
        (None, true, Some(reader)) => KeySource::Reader(reader),
        (None, true, None) => bail!("--key-stdin: stdin is not available"),
        (None, false, _) => KeySource::Ask,
    };
    let new_key = store_new_key(
        store,
        verifier,
        ui,
        &args.profile,
        source,
        args.key_id.as_deref(),
    )
    .await?;
    if !args.packages.is_empty() || settings.is_none() {
        let mut settings = settings.unwrap_or_default();
        if !args.packages.is_empty() {
            settings.packages = args.packages.clone();
        }
        repo.upsert(&args.profile, settings)?;
        if !args.packages.is_empty() {
            ui.say(&format!(
                "Default packages of profile `{}`: {}.",
                args.profile,
                args.packages.join(", ")
            ));
        }
    }
    if let Some(file) = &new_key.file {
        offer_to_delete_key_file(ui, file);
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub async fn status<V: KeyVerifier>(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let settings = repo.get(profile)?;
    ui.say(&format!("Profile:   {profile}"));
    let key = match store.get(profile) {
        Ok(key) => Some(key),
        Err(AuthError::NotFound { .. }) => None,
        Err(err @ AuthError::Corrupted { .. }) => {
            ui.say(&format!("Key:       {err}"));
            None
        }
        Err(err) => return Err(err.into()),
    };
    match &key {
        Some(key) => ui.say(&format!(
            "Key:       {} (RSA {} bits), stored in the system keychain (service \
             `{KEYRING_SERVICE}`)",
            key.key_id().masked(),
            key.bits()
        )),
        None => ui.say(&format!(
            "Key:       not found; store one with `rustore-mcp auth login --profile {profile} \
             --key-file <key-file>`"
        )),
    }
    let check = match (&key, verifier) {
        (Some(key), Some(verifier)) => match verifier.check(key).await {
            Ok(outcome) => format!("ok: {outcome}"),
            Err(err) => format!("failed: {err:#}"),
        },
        (Some(_), None) => "not checked (--no-verify)".to_owned(),
        (None, _) => "-".to_owned(),
    };
    ui.say(&format!("Check:     {check}"));
    match &settings {
        Some(settings) => describe_settings(ui, settings),
        None => ui.say("Settings:  none (read-only defaults)"),
    }
    ui.say(&format!("Profiles:  {}", repo.path().display()));
    Ok(())
}

fn describe_settings(ui: &mut dyn Ui, settings: &ProfileSettings) {
    let packages = if settings.packages.is_empty() {
        "-".to_owned()
    } else {
        settings.packages.join(", ")
    };
    ui.say(&format!("Packages:  {packages}"));
    ui.say(&format!(
        "Client:    {}",
        settings.client.as_deref().unwrap_or("-")
    ));
    // Действующий уровень, как у `serve`: удаление подразумевает запись.
    ui.say(&format!("Write:     {}", yes_no(settings.writes_allowed())));
    ui.say(&format!(
        "Delete:    {}",
        yes_no(settings.allow_destructive)
    ));
    let toolsets = if settings.toolsets.is_empty() {
        "default".to_owned()
    } else {
        settings.toolsets.join(",")
    };
    ui.say(&format!("Toolsets:  {toolsets}"));
}

pub fn logout(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "Key of profile `{profile}` removed from the system keychain. If it is no longer \
             needed anywhere, also delete it in the {CONSOLE_HINT}."
        )),
        Err(AuthError::NotFound { .. }) => {
            ui.say(&format!("No key stored for profile `{profile}`."));
        }
        Err(err) => return Err(err.into()),
    }
    if forget {
        if repo.remove(profile)? {
            ui.say(&format!(
                "Profile `{profile}` removed from {}.",
                repo.path().display()
            ));
        } else {
            ui.say(&format!("Profile `{profile}` has no saved settings."));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rustore_api::auth::MemoryStore;
    use rustore_api::testing;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::testing::FakeVerifier;

    fn args(profile: &str) -> LoginArgs {
        LoginArgs {
            profile: profile.into(),
            key_id: None,
            key_file: None,
            key_stdin: false,
            packages: Vec::new(),
            no_verify: false,
        }
    }

    struct Setup {
        dir: tempfile::TempDir,
        repo: ProfileRepo,
        store: MemoryStore,
    }

    impl Setup {
        /// Файл ключа в формате JSON с keyId.
        fn json_key_file(&self, name: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            let text = json!({
                "keyId": testing::TEST_KEY_ID,
                "privateKey": testing::private_key_base64()
            });
            fs::write(&path, text.to_string()).unwrap();
            path
        }

        /// Файл только с приватным ключом.
        fn bare_key_file(&self, name: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            fs::write(&path, testing::private_key_base64()).unwrap();
            path
        }
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        Setup {
            dir,
            repo,
            store: MemoryStore::new(),
        }
    }

    fn no_stdin() -> Option<&'static mut dyn Read> {
        None
    }

    #[tokio::test]
    async fn login_status_logout_with_flags() {
        let s = setup();
        let verifier = FakeVerifier {
            apps: Some(2),
            ..FakeVerifier::default()
        };
        let file = s.json_key_file("key.json");
        let mut ui = ScriptedUi::default();
        let login_args = LoginArgs {
            key_file: Some(file.clone()),
            packages: vec!["com.example.app".into()],
            ..args("codex")
        };
        login(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ui,
            &login_args,
            no_stdin(),
        )
        .await
        .unwrap();
        assert_eq!(verifier.checked(), [testing::TEST_KEY_ID]);
        assert_eq!(
            s.store.get("codex").unwrap().key_id().as_str(),
            testing::TEST_KEY_ID
        );
        let text = ui.text();
        assert!(text.contains("Key is valid"), "{text}");
        assert!(text.contains("saved in the system keychain"), "{text}");
        assert!(!text.contains(testing::TEST_KEY_ID), "{text}");
        assert!(!text.contains(&testing::private_key_base64()), "{text}");
        // Без терминала файл не удаляется и вопроса нет.
        assert!(file.exists());
        assert!(ui.questions.is_empty());
        let settings = s.repo.get("codex").unwrap().unwrap();
        assert_eq!(settings.packages, ["com.example.app"]);

        let mut ui = ScriptedUi::default();
        status(&s.store, &s.repo, Some(&verifier), &mut ui, "codex")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("Key:       test***"), "{text}");
        assert!(text.contains("listed 2 apps"), "{text}");
        assert!(text.contains("Packages:  com.example.app"), "{text}");
        assert!(!text.contains(testing::TEST_KEY_ID), "{text}");

        let mut ui = ScriptedUi::default();
        logout(&s.store, &s.repo, &mut ui, "codex", true).unwrap();
        assert!(!s.store.contains("codex").unwrap());
        assert!(s.repo.get("codex").unwrap().is_none());
        assert!(ui.text().contains("Company > RuStore API"), "{}", ui.text());

        let mut ui = ScriptedUi::default();
        status(&s.store, &s.repo, Some(&verifier), &mut ui, "codex")
            .await
            .unwrap();
        assert!(ui.text().contains("not found"), "{}", ui.text());
    }

    #[tokio::test]
    async fn login_reads_stdin_and_asks_in_a_terminal() {
        let s = setup();
        let verifier = FakeVerifier::default();

        // stdin: только приватный ключ, keyId флагом.
        let private = testing::private_key_base64();
        let mut reader = private.as_bytes();
        let mut ui = ScriptedUi::default();
        let stdin_args = LoginArgs {
            key_stdin: true,
            key_id: Some("id-from-flag".into()),
            ..args("ci")
        };
        login(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ui,
            &stdin_args,
            Some(&mut reader),
        )
        .await
        .unwrap();
        assert_eq!(s.store.get("ci").unwrap().key_id().as_str(), "id-from-flag");

        // stdin без keyId: ошибка, ничего не сохранено.
        let mut reader = private.as_bytes();
        let stdin_args = LoginArgs {
            key_stdin: true,
            ..args("ci2")
        };
        let err = login(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ScriptedUi::default(),
            &stdin_args,
            Some(&mut reader),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--key-id"), "{err}");
        assert!(!s.store.contains("ci2").unwrap());

        // Терминал: путь к файлу, keyId и удаление файла — ответами на вопросы.
        let file = s.bare_key_file("bare.txt");
        let path = file.display().to_string();
        let mut ui = ScriptedUi::answering(&[&path, "id-from-answer", "y"]);
        login(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ui,
            &args("term"),
            no_stdin(),
        )
        .await
        .unwrap();
        assert_eq!(
            s.store.get("term").unwrap().key_id().as_str(),
            "id-from-answer"
        );
        assert!(!file.exists(), "{}", ui.text());
        assert_eq!(ui.questions.len(), 3, "{:?}", ui.questions);

        // Без терминала и без флага ключа — подсказка.
        let err = login(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ScriptedUi::default(),
            &args("none"),
            no_stdin(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--key-file"), "{err}");
    }

    #[tokio::test]
    async fn failures_store_nothing() {
        let s = setup();
        let file = s.json_key_file("key.json");
        let failing = FakeVerifier::failing("RuStore rejected the API key");
        let login_args = LoginArgs {
            key_file: Some(file.clone()),
            ..args("bad")
        };
        let err = login(
            &s.store,
            &s.repo,
            Some(&failing),
            &mut ScriptedUi::default(),
            &login_args,
            no_stdin(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("Nothing was stored"), "{text}");
        assert!(text.contains("--no-verify"), "{text}");
        assert!(!s.store.contains("bad").unwrap());
        assert!(s.repo.get("bad").unwrap().is_none());

        // --no-verify: проверки нет, ключ сохранён.
        let no_verify = LoginArgs {
            no_verify: true,
            ..login_args.clone()
        };
        let mut ui = ScriptedUi::default();
        login::<FakeVerifier>(&s.store, &s.repo, None, &mut ui, &no_verify, no_stdin())
            .await
            .unwrap();
        assert!(s.store.contains("bad").unwrap());
        assert!(ui.text().contains("--no-verify"), "{}", ui.text());

        // Испорченный файл, неверный пакет, неверное имя профиля.
        let broken = s.dir.path().join("broken.json");
        fs::write(
            &broken,
            "{\"keyId\": \"x\", \"privateKey\": \"secret-ish\"}",
        )
        .unwrap();
        let err = login::<FakeVerifier>(
            &s.store,
            &s.repo,
            None,
            &mut ScriptedUi::default(),
            &LoginArgs {
                key_file: Some(broken),
                ..args("broken")
            },
            no_stdin(),
        )
        .await
        .unwrap_err();
        assert!(!format!("{err:#}").contains("secret-ish"), "{err:#}");
        assert!(!s.store.contains("broken").unwrap());
        let err = login::<FakeVerifier>(
            &s.store,
            &s.repo,
            None,
            &mut ScriptedUi::default(),
            &LoginArgs {
                packages: vec!["not a package".into()],
                ..login_args.clone()
            },
            no_stdin(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("package name"), "{err}");
        assert!(
            login::<FakeVerifier>(
                &s.store,
                &s.repo,
                None,
                &mut ScriptedUi::default(),
                &args("-bad"),
                no_stdin(),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn login_and_status_against_mock_rustore() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/public/auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {"jwe": "jwe-token-for-tests", "ttl": 900}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "code": "FORBIDDEN",
                "message": "Access denied"
            })))
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(&server.uri()).unwrap();
        let s = setup();
        let file = s.json_key_file("key.json");
        let err = login(
            &s.store,
            &s.repo,
            Some(&verifier),
            &mut ScriptedUi::default(),
            &LoginArgs {
                key_file: Some(file),
                ..args("mock")
            },
            no_stdin(),
        )
        .await
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("tick the API methods"), "{text}");
        assert!(text.contains("Nothing was stored"), "{text}");
        assert!(!text.contains("jwe-token-for-tests"), "{text}");
        assert!(!s.store.contains("mock").unwrap());

        // Status показывает неудачную проверку, но не падает.
        s.store.set("mock", &testing::api_key()).unwrap();
        let mut ui = ScriptedUi::default();
        status(&s.store, &s.repo, Some(&verifier), &mut ui, "mock")
            .await
            .unwrap();
        assert!(ui.text().contains("Check:     failed"), "{}", ui.text());
        assert!(!ui.text().contains("jwe-token-for-tests"), "{}", ui.text());
    }
}
