//! `rustore-mcp ui` — TUI профилей ([`mcp_kit::tui`]): профили и их настройки, права и
//! одобрение на macOS, регистрации в Claude Code и Codex, права ключа RuStore API и
//! инструменты с вызовом.
//!
//! Здесь только сервисная часть ([`RuStoreAdapter`]): чтение и запись `profiles.toml`,
//! состояние ключа (замаскированный keyId, без самого ключа), проверка прав ключа
//! инструментом `auth_check`, сборка сервера и повторная регистрация без флагов доступа.
//! Сервер для инструментов собирается так же, как `serve` без флагов: права профиля,
//! урезанные до одобренных.
//!
//! Права ключа RuStore API (методы и приложения) отмечаются в консоли RuStore и через API не
//! читаются: `auth_check` узнаёт их пробными запросами чтения по разделам.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Args;
use mcp_kit::ProfileAccess;
use mcp_kit::approval;
use mcp_kit::call::Session;
use mcp_kit::registrations::{self, Registration, ServerIdentity};
use mcp_kit::tui::{self, Field, ProfileView, ServiceAdapter};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use rustore_api::auth::{AuthError, KEYRING_SERVICE, KeyStore, KeyringStore};
use serde_json::Value;

use crate::cli::serve::{ServeArgs, Setup};
use crate::clients::claude::{self, SystemRunner};
use crate::clients::{EXE_STEM, Scope, ServerSpec, codex};
use crate::profiles::{DEFAULT_PROFILE, ProfileRepo, ProfileSettings};
use crate::ui::CaptureUi;

/// Инструмент проверки прав ключа (см. `rustore_mcp_core::auth_check`).
const AUTH_CHECK_TOOL: &str = "auth_check";

/// Профиль `serve` из переменной окружения не берётся: у `rustore-mcp` такой переменной
/// нет, поэтому имя пустое и в записях клиентов никогда не совпадает.
const NO_PROFILE_ENV: &str = "";

const UI_HELP: &str = "\
Shows the profiles in profiles.toml with their settings and the state of the RuStore API key \
(the masked key ID, never the key), the access of each profile and whether it is approved, \
where the server is registered in Claude Code and Codex and whether those registrations agree \
with the profile, what the key may do in RuStore (checked with read-only requests, as \
`rustore-mcp auth check`), and the tools the profile gives the agent.

The access (read, write or destructive, and the extra directories for local files) is changed \
here: r, w and d set the level, a adds a directory, x removes one, s saves. On macOS, saving \
more access than was approved asks for Touch ID or the macOS password; access that is not \
approved does not take effect. f registers the selected entry again without access flags. \
Press ? for the keys.";

#[derive(Debug, Clone, Args)]
#[command(after_help = UI_HELP)]
pub struct UiArgs {}

pub async fn run(_args: UiArgs) -> anyhow::Result<()> {
    let adapter = RuStoreAdapter {
        repo: ProfileRepo::open_default()?,
        keys: &KeyringStore,
        exe: std::env::current_exe()
            .map_err(|err| anyhow!("cannot determine the path of rustore-mcp: {err}"))?,
        codex_home: codex::default_codex_home().ok(),
    };
    tui::run(&adapter).await.map_err(|err| anyhow!(err))
}

/// Сервисная часть TUI для RuStore.
pub struct RuStoreAdapter<'a> {
    pub repo: ProfileRepo,
    pub keys: &'a dyn KeyStore,
    /// Путь к бинарнику для повторной регистрации.
    pub exe: PathBuf,
    /// `$CODEX_HOME`; `None` — не определить (тогда Codex не регистрируется заново).
    pub codex_home: Option<PathBuf>,
}

impl RuStoreAdapter<'_> {
    /// Сервер с правами профиля без флагов (как `serve` без флагов), со всеми наборами.
    fn setup(&self, profile: &str) -> anyhow::Result<Setup> {
        let args = ServeArgs {
            toolsets: vec![rustore_mcp_core::ALL_TOOLSETS.to_owned()],
            ..ServeArgs::local(profile)
        };
        let approvals = approval::platform_store(EXE_STEM);
        let mut setup = Setup::load(&args, self.keys, &self.repo, approvals.as_deref())?;
        setup.options.dynamic_toolsets = false;
        Ok(setup)
    }

    async fn session(&self, profile: &str) -> Result<Session, String> {
        let setup = self.setup(profile).map_err(|err| format!("{err:#}"))?;
        let server = setup.server().map_err(|err| format!("{err:#}"))?;
        Session::start(server).await.map_err(|err| err.to_string())
    }

    fn profile_fields(&self, name: &str, settings: &ProfileSettings) -> Vec<Field> {
        let key = match self.keys.get(name) {
            Ok(key) => format!(
                "{} (RSA {} bits), in the system keychain (service `{KEYRING_SERVICE}`)",
                key.key_id().masked(),
                key.bits()
            ),
            Err(AuthError::NotFound { .. }) => format!(
                "missing: run `rustore-mcp auth login --profile {name} --key-file <key-file>`"
            ),
            Err(err) => format!("cannot read the keychain: {err}"),
        };
        let push = crate::cli::auth::push::describe(self.keys, name)
            .unwrap_or_else(|err| format!("cannot read the keychain: {err}"));
        let list = |items: &[String], empty: &str| {
            if items.is_empty() {
                empty.to_owned()
            } else {
                items.join(", ")
            }
        };
        vec![
            Field::new("API key", key),
            Field::new("Push", push),
            Field::new("Packages", list(&settings.packages, "-")),
            Field::new("Toolsets", list(&settings.toolsets, "default")),
            Field::new(
                "Dynamic toolsets",
                if settings.dynamic_toolsets {
                    "on"
                } else {
                    "off"
                },
            ),
            Field::new(
                "Connected to",
                settings.client.clone().unwrap_or_else(|| "-".into()),
            ),
        ]
    }
}

impl ServiceAdapter for RuStoreAdapter<'_> {
    fn bin(&self) -> &str {
        EXE_STEM
    }

    fn service_name(&self) -> &str {
        "RuStore"
    }

    fn identity(&self) -> ServerIdentity<'_> {
        ServerIdentity {
            exe_stem: EXE_STEM,
            default_profile: DEFAULT_PROFILE,
            profile_env: NO_PROFILE_ENV,
        }
    }

    fn load_profiles(&self) -> Result<Vec<ProfileView>, String> {
        let profiles = self.repo.list().map_err(|err| format!("{err:#}"))?;
        Ok(profiles
            .iter()
            .map(|(name, settings)| ProfileView {
                name: name.clone(),
                access: settings.access(),
                fields: self.profile_fields(name, settings),
            })
            .collect())
    }

    fn save_access(&self, profile: &str, access: &ProfileAccess) -> Result<(), String> {
        let mut settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        settings.set_access(access);
        self.repo
            .upsert(profile, settings)
            .map_err(|err| format!("{err:#}"))
    }

    async fn check_account(&self, profile: &str) -> Result<Vec<Field>, String> {
        let session = self.session(profile).await?;
        let result = session.call(AUTH_CHECK_TOOL, JsonObject::new()).await;
        session.close().await;
        let result = result.map_err(|err| err.to_string())?;
        Ok(report_fields(&result))
    }

    async fn tools(&self, profile: &str) -> Result<Vec<Tool>, String> {
        let session = self.session(profile).await?;
        let tools = session.tools().await.map_err(|err| err.to_string());
        session.close().await;
        tools
    }

    async fn call_tool(
        &self,
        profile: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let session = self.session(profile).await?;
        let result = session
            .call(tool, arguments)
            .await
            .map_err(|err| err.to_string());
        session.close().await;
        result
    }

    fn reregister(&self, registration: &Registration) -> Result<String, String> {
        reregister(
            registration,
            &self.exe,
            self.codex_home.as_deref(),
            std::env::current_dir().ok().as_deref(),
        )
    }
}

/// Сколько приложений показывать в TUI; остальные — числом.
const MAX_APPS_SHOWN: usize = 10;

/// Отчёт `auth_check` строками TUI: разделы API, приложения, пояснения. Тела ответов в
/// отчёт не попадают (это обеспечивает сам инструмент).
fn report_fields(result: &CallToolResult) -> Vec<Field> {
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if result.is_error == Some(true) {
        let first: String = text
            .lines()
            .next()
            .unwrap_or("error")
            .chars()
            .take(200)
            .collect();
        return vec![Field::new("Check", format!("failed: {first}"))];
    }
    let report: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let str_of = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_owned();
    let mut fields = Vec::new();
    for section in report["sections"].as_array().into_iter().flatten() {
        let status = str_of(section, "status").replace('_', " ");
        let mut value = format!(
            "{status} ({} \"{}\")",
            str_of(section, "method"),
            str_of(section, "method_title")
        );
        if status != "available"
            && let Some(detail) = section["detail"].as_str()
        {
            value.push_str(&format!(": {detail}"));
        }
        fields.push(Field::new(str_of(section, "toolset"), value));
    }
    let apps = report["apps"].as_array().cloned().unwrap_or_default();
    let accessible = apps
        .iter()
        .filter(|app| app["status"] == "accessible")
        .count();
    let mut summary = format!("{accessible} of {} accessible", apps.len());
    if report["apps_source"] == "profile" {
        summary.push_str(" (default packages of the profile)");
    } else if report["apps_complete"] == false {
        summary.push_str(", the list is incomplete");
    }
    fields.push(Field::new("Apps", summary));
    for app in apps.iter().take(MAX_APPS_SHOWN) {
        let mut value = str_of(app, "status").replace('_', " ");
        if let Some(name) = app["appName"].as_str() {
            value.push_str(&format!(", {name}"));
        }
        fields.push(Field::new(
            format!("  {}", str_of(app, "packageName")),
            value,
        ));
    }
    if apps.len() > MAX_APPS_SHOWN {
        fields.push(Field::new(
            "  ...",
            format!("{} more", apps.len() - MAX_APPS_SHOWN),
        ));
    }
    for note in report["notes"].as_array().into_iter().flatten() {
        if let Some(note) = note.as_str() {
            fields.push(Field::new("Note", note));
        }
    }
    fields.push(Field::new(
        "Key permissions",
        "ticked per API method and per app in the RuStore console (Company > RuStore API); \
         methods and apps added later are not added to an existing key",
    ));
    fields
}

/// Регистрирует запись заново: та же команда `serve --profile <профиль>` с теми же наборами,
/// но без флагов доступа. Запись проекта (Claude Code) — только из каталога проекта.
/// HTTP-регистрации не трогаются: права HTTP-сервера задают его флаги.
fn reregister(
    registration: &Registration,
    exe: &Path,
    codex_home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<String, String> {
    let serve = &registration.serve;
    if serve.http {
        return Err(format!(
            "`{}` runs the server in HTTP mode, where the access comes from its flags; change \
             that entry by hand",
            registration.name
        ));
    }
    let settings = ProfileSettings {
        toolsets: serve.toolsets.clone(),
        dynamic_toolsets: serve.dynamic_toolsets.unwrap_or(false),
        ..ProfileSettings::default()
    };
    let spec = ServerSpec::new(&registration.name, exe, &serve.profile, &settings)
        .map_err(|err| format!("{err:#}"))?;
    let mut ui = CaptureUi::default();
    let done = match registration.client {
        registrations::Client::ClaudeCode => {
            let scope = match &registration.scope {
                registrations::Scope::User => Scope::User,
                registrations::Scope::Project(dir) if Some(dir.as_path()) == cwd => Scope::Project,
                other => {
                    return Err(format!(
                        "`{}` is registered for {other}; run `rustore-mcp connect claude-code \
                         --scope project --name {} --profile {}` in that project",
                        registration.name, registration.name, serve.profile
                    ));
                }
            };
            claude::register(&mut SystemRunner, &mut ui, scope, &spec, true)
        }
        registrations::Client::Codex => {
            let home = codex_home.ok_or("cannot determine $CODEX_HOME")?;
            codex::register(&mut ui, home, &spec, true)
        }
    }
    .map_err(|err| format!("{err:#}"))?;
    if !done {
        return Err(ui
            .lines
            .last()
            .cloned()
            .unwrap_or_else(|| "not changed".into()));
    }
    Ok(format!(
        "{} now runs `{}` as `{}`; restart the client to pick it up",
        registration.client.display_name(),
        spec.display(),
        spec.name
    ))
}

#[cfg(test)]
mod tests {
    use mcp_kit::registrations::ServeInvocation;
    use rustore_api::auth::MemoryStore;
    use rustore_api::testing;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::clients::TEST_EXE;

    fn adapter<'a>(dir: &Path, keys: &'a MemoryStore) -> RuStoreAdapter<'a> {
        RuStoreAdapter {
            repo: ProfileRepo::in_dir(dir.join("config")),
            keys,
            exe: PathBuf::from(TEST_EXE),
            codex_home: Some(dir.join("codex")),
        }
    }

    #[test]
    fn shows_profiles_without_the_key_and_saves_access() {
        let dir = tempfile::tempdir().unwrap();
        let keys = MemoryStore::new();
        keys.set("work", &testing::api_key()).unwrap();
        let adapter = adapter(dir.path(), &keys);
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    packages: vec!["com.example.app".into()],
                    allow_write: true,
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        adapter
            .repo
            .upsert("empty", ProfileSettings::default())
            .unwrap();

        let profiles = adapter.load_profiles().unwrap();
        assert_eq!(profiles.len(), 2);
        let work = profiles.iter().find(|p| p.name == "work").unwrap();
        assert_eq!(work.access.level, mcp_kit::AccessLevel::Write);
        let text = format!("{:?}", work.fields);
        assert!(text.contains("test***0001 (RSA 2048 bits)"), "{text}");
        assert!(text.contains("com.example.app"), "{text}");
        assert!(text.contains("Push"), "{text}");
        // Ни keyId целиком, ни приватного ключа.
        assert!(!text.contains(testing::TEST_KEY_ID), "{text}");
        assert!(
            !text.contains(&testing::private_key_base64()[..64]),
            "{text}"
        );
        let empty = profiles.iter().find(|p| p.name == "empty").unwrap();
        assert!(format!("{:?}", empty.fields).contains("auth login --profile empty"));

        let root = dir.path().join("files");
        adapter
            .save_access(
                "work",
                &ProfileAccess::new(mcp_kit::AccessLevel::Destructive, vec![root.clone()]),
            )
            .unwrap();
        let saved = adapter.repo.get("work").unwrap().unwrap();
        assert!(saved.allow_write && saved.allow_destructive);
        assert_eq!(saved.allow_path, vec![root]);
        // Остальные настройки профиля не тронуты.
        assert_eq!(saved.packages, ["com.example.app"]);
    }

    #[test]
    fn report_is_shown_as_fields() {
        let report = json!({
            "sections": [
                {"toolset": "apps", "section": "Apps", "status": "available",
                 "method": "list_apps", "method_title": "Get apps"},
                {"toolset": "purchases", "section": "Payments", "status": "denied",
                 "method": "list_payments_by_date", "method_title": "Payments",
                 "detail": "RuStore returned 403"},
                {"toolset": "subscriptions", "section": "Subscriptions",
                 "status": "not_checked", "method": "get_subscription_v5",
                 "method_title": "Subscription"}
            ],
            "apps": [
                {"packageName": "com.example.one", "appName": "One", "status": "accessible"},
                {"packageName": "com.example.two", "status": "denied"}
            ],
            "apps_source": "list_apps",
            "apps_complete": true,
            "notes": ["Tick the methods in the console."]
        });
        let ok = CallToolResult::success(vec![ContentBlock::text(report.to_string())]);
        let fields = report_fields(&ok);
        let text = format!("{fields:?}");
        assert!(text.contains("available (list_apps"), "{text}");
        assert!(text.contains("denied (list_payments_by_date"), "{text}");
        assert!(text.contains("RuStore returned 403"), "{text}");
        assert!(text.contains("not checked"), "{text}");
        assert!(text.contains("1 of 2 accessible"), "{text}");
        assert!(text.contains("com.example.one"), "{text}");
        assert!(text.contains("Tick the methods"), "{text}");
        assert!(text.contains("RuStore console"), "{text}");

        let err = CallToolResult::error(vec![ContentBlock::text("RuStore rejected the key\nmore")]);
        let fields = report_fields(&err);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].value, "failed: RuStore rejected the key");
    }

    /// Проверка ключа идёт через сервер с правами профиля: токен и пробные запросы — в
    /// wiremock, приватный ключ и токен в поля не попадают.
    #[tokio::test]
    async fn check_account_runs_auth_check_through_the_server() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/public/auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {"jwe": "jwe-ui-test-token", "ttl": 900}
            })))
            .mount(&api)
            .await;
        // Ключу не выдан ни один метод: все пробные запросы получают 403.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "code": "FORBIDDEN",
                "message": "Access denied"
            })))
            .mount(&api)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let keys = MemoryStore::new();
        keys.set("work", &testing::api_key()).unwrap();
        let adapter = adapter(dir.path(), &keys);
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    packages: vec!["com.example.app".into()],
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        let mut setup = adapter.setup("work").unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        setup.options.jq = rustore_mcp_core::JqRunner::in_process();
        let session = Session::start(setup.server().unwrap()).await.unwrap();
        let result = session
            .call(AUTH_CHECK_TOOL, JsonObject::new())
            .await
            .unwrap();
        session.close().await;
        let text = format!("{:?}", report_fields(&result));
        assert!(text.contains("denied"), "{text}");
        assert!(text.contains("com.example.app"), "{text}");
        assert!(!text.contains("jwe-ui-test-token"), "{text}");
    }

    fn registration(client: registrations::Client, scope: registrations::Scope) -> Registration {
        Registration {
            client,
            scope,
            name: "rustore-work".into(),
            command: PathBuf::from(TEST_EXE),
            args: vec![],
            env_names: vec![],
            enabled: true,
            serve: ServeInvocation {
                subcommand: Some("serve".into()),
                profile: "work".into(),
                toolsets: vec!["apps".into()],
                dynamic_toolsets: Some(true),
                ..ServeInvocation::default()
            },
        }
    }

    #[test]
    fn reregisters_codex_without_access_flags() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            codex::config_path(&home),
            format!(
                "[mcp_servers.rustore-work]\ncommand = {TEST_EXE:?}\nargs = [\"serve\", \"--profile\", \"work\", \"--allow-write\", \"--allow-destructive\"]\n"
            ),
        )
        .unwrap();
        let reg = registration(registrations::Client::Codex, registrations::Scope::User);
        let message = reregister(&reg, Path::new(TEST_EXE), Some(&home), None).unwrap();
        assert!(message.contains("Codex now runs"), "{message}");
        let config = std::fs::read_to_string(codex::config_path(&home)).unwrap();
        assert!(!config.contains("--allow"), "{config}");
        assert!(
            config.contains(r#"args = ["serve", "--profile", "work", "--toolsets", "apps", "--dynamic-toolsets"]"#),
            "{config}"
        );
    }

    #[test]
    fn project_and_http_registrations_are_explained() {
        let (here, elsewhere) = if cfg!(windows) {
            (r"C:\here", r"C:\elsewhere")
        } else {
            ("/here", "/elsewhere")
        };
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Project(PathBuf::from(elsewhere)),
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new(here))).unwrap_err();
        assert!(err.contains("--scope project"), "{err}");
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Local(PathBuf::from(here)),
        );
        assert!(reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new(here))).is_err());

        let mut reg = registration(registrations::Client::Codex, registrations::Scope::User);
        reg.serve.http = true;
        let err = reregister(&reg, Path::new(TEST_EXE), None, None).unwrap_err();
        assert!(err.contains("HTTP mode"), "{err}");
    }
}
