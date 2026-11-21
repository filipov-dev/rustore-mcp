//! `rustore-mcp connect` — подключение сервера к MCP-клиенту одной командой.
//!
//! Сценарий: ключ RuStore API профиля (рабочий сохранённый можно оставить, иначе — новый из
//! файла: `--key-file` или путь, который спросит терминал) → проверка получением токена и
//! списка приложений → ключ в системное хранилище, настройки в `profiles.toml` (права —
//! только если заданы флаги; на macOS повышение прав одобряется Touch ID или паролем) →
//! запись сервера в конфигурацию клиента после подтверждения, без флагов доступа. Ключ не
//! проходит через агента и не попадает ни в аргументы, ни в конфигурацию клиента: клиент
//! получает только команду `rustore-mcp serve --profile <профиль> ...`.

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use mcp_kit::approval::{self, ApprovalStore, Presence};
use mcp_kit::{AccessLevel, ProfileAccess};
use rustore_api::auth::{AuthError, KeyStore, KeyringStore};
use rustore_mcp_core::normalize_toolsets;

use crate::clients::claude::{self, CommandRunner, SystemRunner};
use crate::clients::{ClientKind, EXE_STEM, Scope, ServerSpec, codex, default_server_name};
use crate::login::{KeySource, offer_to_delete_key_file, store_new_key};
use crate::profiles::{
    DEFAULT_PROFILE, ProfileRepo, ProfileSettings, validate_package_name, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, KeyVerifier};

#[derive(Debug, Clone, Args)]
#[command(after_help = "\
The server works with one RuStore API key of a company: create it in the RuStore console \
(Company > RuStore API), tick the API methods and the apps it may use, and save the private key \
to a file. connect reads the key from --key-file (or asks for the path in a terminal), checks it \
by getting a token and listing the apps, and stores it only in the system keychain under the \
profile name. If the profile already has a working key, connect offers to keep it.

The client config gets only the command `rustore-mcp serve --profile <profile> ...` without \
access flags; never the key or tokens. The access is a setting of the profile: --allow-write \
and --allow-destructive change it (without them the profile keeps its access, read-only for a \
new profile), and `rustore-mcp ui` changes it later. On macOS, raising the access asks for \
Touch ID or the macOS password; access that is not approved does not take effect. The server name in the client is rustore for the default \
profile and rustore-<profile> for others; it may not be longer than 17 characters, so that the \
full tool names (mcp__<server>__<tool>) fit the 64-character limit of the clients. Use a \
separate profile for every company key.")]
pub struct ConnectArgs {
    /// MCP client to register the server in.
    #[arg(value_enum)]
    pub client: ClientKind,
    /// Profile name (one profile per RuStore API key).
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// MCP server name in the client config [default: rustore for the default profile,
    /// rustore-<profile> for others].
    #[arg(long)]
    pub name: Option<String>,
    /// Where to register the server in the client. Codex supports only `user`.
    #[arg(long, value_enum, default_value_t = Scope::User)]
    pub scope: Scope,
    /// Set the access of the profile to write: tools that create or modify data in RuStore.
    #[arg(long)]
    pub allow_write: bool,
    /// Set the access of the profile to destructive: also tools that delete data, publish to
    /// users or affect money (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable (passed to `serve --toolsets`). Default: the
    /// default set of `serve`.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Start with only the search, request and toolset tools; the agent loads toolsets on
    /// demand (passed to `serve --dynamic-toolsets`). Needs a client that reloads the tool
    /// list when notified.
    #[arg(long)]
    pub dynamic_toolsets: bool,
    /// File with the RuStore API key: JSON with keyId and privateKey, or only the private
    /// key (then pass --key-id or answer the question).
    #[arg(long, value_name = "PATH")]
    pub key_file: Option<PathBuf>,
    /// Key ID (keyId) of the key, shown next to it in the RuStore console.
    #[arg(long, value_name = "ID")]
    pub key_id: Option<String>,
    /// Default package name of the profile's apps (repeatable), shown to the agent.
    #[arg(long = "package", value_name = "NAME")]
    pub packages: Vec<String>,
    /// Do not ask for confirmation: keep a working stored key and change the client config.
    ///
    /// An entry with the same name that does not run rustore-mcp is never replaced with --yes.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Store the key without checking it against the RuStore API.
    #[arg(long)]
    pub no_verify: bool,
}

pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot determine the path of rustore-mcp")?;
    let codex_home = match args.client {
        ClientKind::Codex => codex::default_codex_home()?,
        ClientKind::ClaudeCode => PathBuf::new(),
    };
    let verifier = if args.no_verify {
        None
    } else {
        Some(ApiVerifier::new()?)
    };
    let approvals = approval::platform_store(EXE_STEM);
    let presence = approval::platform_presence();
    let mut env = ConnectEnv {
        approval: approvals.as_deref().zip(presence.as_deref()),
        store: &KeyringStore,
        repo: &ProfileRepo::open_default()?,
        verifier: verifier.as_ref(),
        runner: &mut SystemRunner,
        ui: &mut TerminalUi,
        exe,
        codex_home,
    };
    connect(&mut env, &args).await
}

/// Зависимости `connect`; в тестах подменяются хранилищем в памяти и временными каталогами.
pub struct ConnectEnv<'a, V> {
    /// Одобрение прав (macOS); `None` — платформа без одобрения.
    pub approval: Option<(&'a dyn ApprovalStore, &'a dyn Presence)>,
    pub store: &'a dyn KeyStore,
    pub repo: &'a ProfileRepo,
    /// Проверка ключа; `None` — `--no-verify`.
    pub verifier: Option<&'a V>,
    pub runner: &'a mut dyn CommandRunner,
    pub ui: &'a mut dyn Ui,
    pub exe: PathBuf,
    pub codex_home: PathBuf,
}

pub async fn connect<V: KeyVerifier>(
    env: &mut ConnectEnv<'_, V>,
    args: &ConnectArgs,
) -> anyhow::Result<()> {
    let client = args.client;
    let profile = args.profile.as_str();
    validate_profile_name(profile)?;

    // Всё, что можно проверить без вопросов, — до первого вопроса пользователю.
    let toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow::anyhow!(err))?;
    for package in &args.packages {
        validate_package_name(package)?;
    }
    let mut settings = env.repo.get(profile)?.unwrap_or_default();
    let requested_level = AccessLevel::requested_by_flags(args.allow_write, args.allow_destructive);
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| default_server_name(profile));
    let spec = ServerSpec::new(
        &name,
        &env.exe,
        profile,
        &ProfileSettings {
            toolsets: toolsets.clone(),
            dynamic_toolsets: args.dynamic_toolsets,
            ..Default::default()
        },
    )?;
    match client {
        ClientKind::ClaudeCode => claude::ensure_available(env.runner)?,
        ClientKind::Codex => codex::ensure_scope(args.scope)?,
    }

    let key_file = profile_key(env, args, profile).await?;

    settings.client = Some(client.name().to_owned());
    if let Some(level) = requested_level {
        change_access(env.ui, env.approval, profile, &mut settings, level);
    }
    settings.toolsets = toolsets;
    settings.dynamic_toolsets = args.dynamic_toolsets;
    if !args.packages.is_empty() {
        settings.packages = args.packages.clone();
    }
    env.repo.upsert(profile, settings)?;
    env.ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        env.repo.path().display()
    ));

    let registered = match client {
        ClientKind::ClaudeCode => {
            claude::register(env.runner, env.ui, args.scope, &spec, args.yes)?
        }
        ClientKind::Codex => codex::register(env.ui, &env.codex_home, &spec, args.yes)?,
    };
    if registered {
        env.ui.say(&format!(
            "Done: {} now runs `{}` as MCP server `{}`. Restart the client to pick it up.",
            client.display_name(),
            spec.display(),
            spec.name
        ));
    } else {
        env.ui.say(&format!(
            "{} config was not changed. The key and profile are kept; re-run \
             `rustore-mcp connect {}` to register later.",
            client.display_name(),
            client.name()
        ));
    }
    if let Some(file) = &key_file {
        offer_to_delete_key_file(env.ui, file);
    }
    Ok(())
}

/// Меняет уровень доступа профиля (каталоги профиля остаются). На macOS повышение сначала
/// одобряется владельцем; отказ не останавливает `connect`: профиль сохраняет прежние права.
fn change_access(
    ui: &mut dyn Ui,
    approval: Option<(&dyn ApprovalStore, &dyn Presence)>,
    profile: &str,
    settings: &mut ProfileSettings,
    level: AccessLevel,
) {
    let before = settings.access();
    let access = ProfileAccess::new(level, before.allow_path.clone());
    let what = format!("{EXE_STEM} profile \"{profile}\"");
    match approval::save_access_with(approval, profile, &what, &access, |access| {
        settings.set_access(access);
        Ok(())
    }) {
        Ok(saved) if saved.prompted => ui.say(&format!(
            "Access `{}` approved on this Mac.",
            saved.access.summary()
        )),
        Ok(_) => {}
        Err(err) => ui.say(&format!(
            "The access was not changed: {err}. The profile keeps `{}` access; change it later \
             with `rustore-mcp ui` (profile `{profile}`).",
            before.summary()
        )),
    }
}

/// Ключ профиля: рабочий сохранённый можно оставить, иначе — новый из файла. Возвращает
/// путь файла нового ключа (его можно предложить удалить в конце).
async fn profile_key<V: KeyVerifier>(
    env: &mut ConnectEnv<'_, V>,
    args: &ConnectArgs,
    profile: &str,
) -> anyhow::Result<Option<PathBuf>> {
    // Новый ключ из флага заменяет сохранённый без вопросов.
    if args.key_file.is_none() {
        let stored = match env.store.get(profile) {
            Ok(key) => Some(key),
            Err(AuthError::NotFound { .. }) => None,
            Err(err @ AuthError::Corrupted { .. }) => {
                env.ui.say(&format!("{err}; a new key is needed."));
                None
            }
            Err(err) => return Err(err.into()),
        };
        let keep = match (&stored, env.verifier) {
            (None, _) => false,
            (Some(key), Some(verifier)) => match verifier.check(key).await {
                Ok(outcome) => {
                    env.ui.say(&format!(
                        "Profile `{profile}` already has a working key {}: {outcome}.",
                        key.key_id().masked()
                    ));
                    args.yes || env.ui.confirm("Keep the stored key?", true)?
                }
                Err(err) => {
                    env.ui.say(&format!(
                        "The stored key of profile `{profile}` does not work ({err:#}); a new \
                         key is needed."
                    ));
                    false
                }
            },
            (Some(key), None) => {
                env.ui.say(&format!(
                    "Profile `{profile}` already has a stored key {} (not checked: \
                     --no-verify).",
                    key.key_id().masked()
                ));
                args.yes || env.ui.confirm("Keep the stored key?", true)?
            }
        };
        if keep {
            env.ui.say("Keeping the stored key.");
            return Ok(None);
        }
    }
    let source = match &args.key_file {
        Some(path) => KeySource::File(path),
        None => KeySource::Ask,
    };
    let new_key = store_new_key(
        env.store,
        env.verifier,
        env.ui,
        profile,
        source,
        args.key_id.as_deref(),
    )
    .await?;
    Ok(new_key.file)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use mcp_kit::approval::testing::{FakePresence, MemoryStore as MemoryApprovals};
    use rustore_api::auth::MemoryStore;
    use rustore_api::{ApiKey, testing};

    use super::*;
    use crate::clients::TEST_EXE;
    use crate::clients::claude::testing::FakeRunner;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::testing::FakeVerifier;

    struct Fixture {
        dir: tempfile::TempDir,
        store: MemoryStore,
        repo: ProfileRepo,
        verifier: FakeVerifier,
        no_verify: bool,
        runner: FakeRunner,
        ui: ScriptedUi,
        codex_home: PathBuf,
        /// Одобрение прав, как на macOS; `None` — как на Linux и Windows.
        approval: Option<(MemoryApprovals, FakePresence)>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self {
                store: MemoryStore::new(),
                repo: ProfileRepo::in_dir(dir.path().join("config")),
                verifier: FakeVerifier {
                    apps: Some(1),
                    ..FakeVerifier::default()
                },
                no_verify: false,
                runner: FakeRunner::default(),
                ui: ScriptedUi::default(),
                codex_home: dir.path().join("codex"),
                approval: None,
                dir,
            }
        }

        fn answers(&mut self, values: &[&str]) {
            self.ui.answers = values.iter().map(|value| (*value).to_owned()).collect();
        }

        /// Файл ключа в формате JSON с keyId.
        fn key_file(&self) -> PathBuf {
            let path = self.dir.path().join("key.json");
            let text = serde_json::json!({
                "keyId": testing::TEST_KEY_ID,
                "privateKey": testing::private_key_base64()
            });
            fs::write(&path, text.to_string()).unwrap();
            path
        }

        async fn connect(&mut self, args: &ConnectArgs) -> anyhow::Result<()> {
            let approval = self
                .approval
                .as_ref()
                .map(|(store, presence)| (store as &dyn ApprovalStore, presence as &dyn Presence));
            let mut env = ConnectEnv {
                approval,
                store: &self.store,
                repo: &self.repo,
                verifier: (!self.no_verify).then_some(&self.verifier),
                runner: &mut self.runner,
                ui: &mut self.ui,
                exe: PathBuf::from(TEST_EXE),
                codex_home: self.codex_home.clone(),
            };
            connect(&mut env, args).await
        }

        fn codex_config(&self) -> Option<String> {
            fs::read_to_string(codex::config_path(&self.codex_home)).ok()
        }
    }

    fn args(client: ClientKind) -> ConnectArgs {
        ConnectArgs {
            client,
            profile: DEFAULT_PROFILE.into(),
            name: None,
            scope: Scope::User,
            allow_write: false,
            allow_destructive: false,
            toolsets: Vec::new(),
            dynamic_toolsets: false,
            key_file: None,
            key_id: None,
            packages: Vec::new(),
            yes: false,
            no_verify: false,
        }
    }

    /// Аргументы `serve` в TOML-записи Codex.
    fn toml_args(args: &[&str]) -> String {
        format!("args = {args:?}")
    }

    fn first_toolset() -> &'static str {
        rustore_api::spec::toolsets()[0].slug
    }

    #[tokio::test]
    async fn first_codex_run_stores_key_profile_and_config() {
        let mut fx = Fixture::new();
        let file = fx.key_file();
        fx.answers(&["y"]);
        let mut args = args(ClientKind::Codex);
        args.key_file = Some(file.clone());
        args.allow_write = true;
        args.toolsets = vec![first_toolset().into(), " ".into(), first_toolset().into()];
        args.dynamic_toolsets = true;
        args.packages = vec!["com.example.app".into()];

        fx.connect(&args).await.unwrap();

        assert_eq!(
            fx.store.get("default").unwrap().key_id().as_str(),
            testing::TEST_KEY_ID
        );
        let settings = fx.repo.get("default").unwrap().unwrap();
        assert_eq!(settings.client.as_deref(), Some("codex"));
        assert!(settings.allow_write);
        assert!(!settings.allow_destructive);
        assert_eq!(settings.toolsets, [first_toolset()]);
        assert!(settings.dynamic_toolsets);
        assert_eq!(settings.packages, ["com.example.app"]);

        let config = fx.codex_config().unwrap();
        assert!(config.contains("[mcp_servers.rustore]"), "{config}");
        assert!(
            config.contains(&toml_args(&[
                "serve",
                "--profile",
                "default",
                "--toolsets",
                first_toolset(),
                "--dynamic-toolsets"
            ])),
            "{config}"
        );
        // Ключ не выводится и не попадает ни в конфиг клиента, ни в профили.
        let output = fx.ui.text();
        let private = testing::private_key_base64();
        for text in [
            &output,
            &config,
            &fs::read_to_string(fx.repo.path()).unwrap(),
        ] {
            assert!(!text.contains(testing::TEST_KEY_ID), "{text}");
            assert!(!text.contains(&private[..64]), "{text}");
        }
        assert!(output.contains("Key is valid"), "{output}");
        // Без терминала файл ключа не трогается.
        assert!(file.exists());
        assert!(fx.runner.calls.is_empty());
    }

    #[tokio::test]
    async fn rerun_keeps_working_key_or_replaces_broken_one() {
        let mut fx = Fixture::new();
        fx.store.set("ci", &testing::api_key()).unwrap();
        let mut args = args(ClientKind::Codex);
        args.profile = "ci".into();
        // «Оставить ключ?» — по умолчанию да; затем запись конфига.
        fx.answers(&["", "y"]);
        fx.connect(&args).await.unwrap();
        assert_eq!(
            fx.ui.questions,
            ["Keep the stored key?", "Write the Codex config?"]
        );
        let config = fx.codex_config().unwrap();
        assert!(config.contains("[mcp_servers.rustore-ci]"), "{config}");
        assert!(fx.ui.text().contains("already has a working key"));

        // `--yes` оставляет рабочий ключ без вопроса.
        let mut fx = Fixture::new();
        fx.store.set("default", &testing::api_key()).unwrap();
        let mut yes = args.clone();
        yes.profile = DEFAULT_PROFILE.into();
        yes.yes = true;
        fx.connect(&yes).await.unwrap();
        assert!(fx.ui.questions.is_empty());

        // Неработающий ключ заменяется новым из файла, о котором спрашивает терминал.
        let mut fx = Fixture::new();
        fx.verifier = FakeVerifier::failing("RuStore rejected the API key");
        fx.store.set("default", &testing::api_key()).unwrap();
        let mut fx_ui = ScriptedUi::answering(&[]);
        std::mem::swap(&mut fx.ui, &mut fx_ui);
        let err = fx.connect(&yes).await.unwrap_err();
        assert!(fx.ui.text().contains("does not work"), "{}", fx.ui.text());
        assert!(err.to_string().contains("--key-file"), "{err}");
    }

    #[tokio::test]
    async fn key_file_replaces_stored_key_and_can_be_deleted() {
        let mut fx = Fixture::new();
        let other = ApiKey::new("other-key-id-0002", &testing::private_key_base64()).unwrap();
        fx.store.set("default", &other).unwrap();
        let file = fx.key_file();
        // Терминал: запись конфига, затем удаление файла ключа.
        fx.ui = ScriptedUi::answering(&["y", "y"]);
        let mut args = args(ClientKind::Codex);
        args.key_file = Some(file.clone());
        fx.connect(&args).await.unwrap();
        assert_eq!(
            fx.store.get("default").unwrap().key_id().as_str(),
            testing::TEST_KEY_ID
        );
        assert!(!file.exists(), "{}", fx.ui.text());
        assert!(fx.ui.text().contains("replaced"), "{}", fx.ui.text());
    }

    #[tokio::test]
    async fn access_flags_change_the_profile_not_the_registration() {
        let cases = [(false, false), (true, false), (false, true), (true, true)];
        for (write, destructive) in cases {
            let mut fx = Fixture::new();
            fx.store.set("default", &testing::api_key()).unwrap();
            let mut args = args(ClientKind::Codex);
            args.allow_write = write;
            args.allow_destructive = destructive;
            args.yes = true;
            fx.connect(&args).await.unwrap();

            let settings = fx.repo.get("default").unwrap().unwrap();
            assert_eq!(settings.allow_write, write || destructive);
            assert_eq!(settings.allow_destructive, destructive);

            let config = fx.codex_config().unwrap();
            assert!(
                config.contains(&toml_args(&["serve", "--profile", "default"])),
                "{config}"
            );
        }

        // Без флагов права профиля не меняются.
        let mut fx = Fixture::new();
        fx.store.set("default", &testing::api_key()).unwrap();
        fx.repo
            .upsert(
                "default",
                ProfileSettings {
                    allow_write: true,
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        let mut args = args(ClientKind::Codex);
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.repo.get("default").unwrap().unwrap().allow_write);
    }

    #[tokio::test]
    async fn raising_access_needs_approval_on_macos() {
        let mut fx = Fixture::new();
        fx.store.set("default", &testing::api_key()).unwrap();
        fx.approval = Some((MemoryApprovals::default(), FakePresence::new(false)));
        let mut args = args(ClientKind::Codex);
        args.allow_destructive = true;
        args.yes = true;
        fx.connect(&args).await.unwrap();
        // Отказ: права прежние, но профиль и регистрация сохранены.
        let settings = fx.repo.get("default").unwrap().unwrap();
        assert!(!settings.allow_write && !settings.allow_destructive);
        let text = fx.ui.text();
        assert!(text.contains("The access was not changed"), "{text}");
        assert!(
            text.contains("`rustore-mcp ui` (profile `default`)"),
            "{text}"
        );
        assert!(fx.codex_config().is_some());

        fx.approval = Some((MemoryApprovals::default(), FakePresence::new(true)));
        fx.ui = ScriptedUi::default();
        fx.connect(&args).await.unwrap();
        let settings = fx.repo.get("default").unwrap().unwrap();
        assert!(settings.allow_destructive);
        let (approvals, presence) = fx.approval.as_ref().unwrap();
        assert_eq!(presence.asked.get(), 1);
        assert_eq!(
            approvals.load("default").unwrap().unwrap().level,
            AccessLevel::Destructive
        );
        assert!(
            fx.ui.text().contains("approved on this Mac"),
            "{}",
            fx.ui.text()
        );
    }

    #[tokio::test]
    async fn declined_config_write_keeps_key_and_profile() {
        let mut fx = Fixture::new();
        let mut args = args(ClientKind::Codex);
        args.key_file = Some(fx.key_file());
        fx.answers(&["n"]);
        fx.connect(&args).await.unwrap();
        assert!(fx.codex_config().is_none());
        assert!(fx.store.contains("default").unwrap());
        assert_eq!(
            fx.repo.get("default").unwrap().unwrap().client.as_deref(),
            Some("codex")
        );
        assert!(fx.ui.text().contains("config was not changed"));
    }

    #[tokio::test]
    async fn failed_check_stores_nothing() {
        let mut fx = Fixture::new();
        fx.verifier = FakeVerifier::failing("RuStore rejected the API key");
        let mut args = args(ClientKind::Codex);
        args.key_file = Some(fx.key_file());
        let err = fx.connect(&args).await.unwrap_err();
        assert!(err.to_string().contains("Nothing was stored"), "{err}");
        assert!(!fx.store.contains("default").unwrap());
        assert!(fx.codex_config().is_none());
        assert_eq!(fx.repo.get("default").unwrap(), None);

        // --no-verify сохраняет ключ без проверки.
        fx.no_verify = true;
        let mut args = args.clone();
        args.no_verify = true;
        args.yes = true;
        fx.connect(&args).await.unwrap();
        assert!(fx.store.contains("default").unwrap());
        assert!(fx.ui.text().contains("--no-verify"));
    }

    #[tokio::test]
    async fn claude_flow_with_custom_name_and_project_scope() {
        let mut fx = Fixture::new();
        fx.store.set("default", &testing::api_key()).unwrap();
        fx.answers(&["", "y"]);
        let mut args = args(ClientKind::ClaudeCode);
        args.scope = Scope::Project;
        args.name = Some("rustore-work".into());
        fx.connect(&args).await.unwrap();

        assert_eq!(
            fx.runner.calls,
            [
                vec!["--version".to_owned()],
                claude::add_args(
                    Scope::Project,
                    &ServerSpec {
                        name: "rustore-work".into(),
                        command: TEST_EXE.into(),
                        args: vec!["serve".into(), "--profile".into(), "default".into()],
                    }
                )
            ]
        );
        let settings = fx.repo.get("default").unwrap().unwrap();
        assert_eq!(settings.client.as_deref(), Some("claude-code"));
        assert!(fx.codex_config().is_none());
    }

    #[tokio::test]
    async fn claude_missing_cli_fails_before_questions() {
        let mut fx = Fixture::new();
        fx.runner.results = [claude::CommandOutput {
            success: false,
            stderr: "broken".into(),
            ..Default::default()
        }]
        .into();
        let err = fx.connect(&args(ClientKind::ClaudeCode)).await.unwrap_err();
        assert!(err.to_string().contains("broken"), "{err}");
        assert!(fx.ui.output.is_empty());
        assert!(fx.ui.questions.is_empty());
    }

    #[tokio::test]
    async fn invalid_input_fails_before_questions() {
        let mut cases = Vec::new();
        let mut codex_project = args(ClientKind::Codex);
        codex_project.scope = Scope::Project;
        cases.push((codex_project, "--scope user"));
        let mut toolset = args(ClientKind::Codex);
        toolset.toolsets = vec!["nope".into()];
        cases.push((toolset, "unknown toolset `nope`"));
        let mut profile = args(ClientKind::Codex);
        profile.profile = "--evil".into();
        cases.push((profile, "invalid profile name"));
        let mut name = args(ClientKind::Codex);
        name.name = Some("a b".into());
        cases.push((name, "invalid server name"));
        // Профиль с длинным именем: имя сервера по умолчанию длиннее 17 символов.
        let mut long = args(ClientKind::Codex);
        long.profile = "claude-code".into();
        cases.push((long, "longer than 17"));
        let mut package = args(ClientKind::Codex);
        package.packages = vec!["bad package".into()];
        cases.push((package, "invalid package name"));

        for (args, expected) in cases {
            let mut fx = Fixture::new();
            let err = fx.connect(&args).await.unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
            assert!(fx.ui.output.is_empty());
            assert_eq!(fx.repo.get(&args.profile).ok().flatten(), None);
        }
    }
}
