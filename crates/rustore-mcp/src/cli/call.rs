//! `rustore-mcp call` — вызов инструментов сервера из консоли.
//!
//! Сервер собирается так же, как stdio-сервер `serve` (права профиля, одобрение на macOS,
//! флаги только сужают), и вызывается MCP-клиентом в этом же процессе ([`mcp_kit::call`]):
//! проверка аргументов, уровни доступа и вырезание секретов те же, что у агента. По
//! умолчанию доступны все наборы инструментов, без наборов по запросу.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::anyhow;
use clap::Args;
use mcp_kit::approval;
use mcp_kit::call::{self, CallArgs, Session, TerminalIo};
use rustore_api::auth::KeyringStore;

use crate::cli::serve::{
    DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_TIMEOUT_SECS, ServeArgs, Setup, init_logging,
};
use crate::clients::EXE_STEM;
use crate::profiles::{DEFAULT_PROFILE, ProfileRepo};

const CALL_HELP: &str = "\
Without TOOL, lists the tools the profile can use, with their access level. With TOOL and \
--schema, prints the input schema of the tool. Otherwise calls the tool and prints its result; \
the exit code is 1 when the tool returns an error.

Arguments: `name=value` takes the type from the tool's schema (numbers, booleans, JSON arrays \
and objects; a comma-separated list for an array of strings), `name:=JSON` is always JSON, and \
--args takes all arguments as one JSON object (`-` reads it from stdin). Examples:
  rustore-mcp call list_apps pageSize=5
  rustore-mcp call api_request method=GET path=/public/v1/application jq='[.content[].packageName]'

Access: the same as `serve` without flags: the access of the profile (approved on this Mac on \
macOS); --allow-write, --allow-destructive and --allow-path only narrow it. Destructive tools \
ask for confirmation unless --yes is given. Every toolset is enabled unless --toolsets is given.";

#[derive(Debug, Clone, Args)]
#[command(after_help = CALL_HELP)]
pub struct CallCommand {
    /// Profile with the RuStore API key [default: default].
    #[arg(long)]
    pub profile: Option<String>,
    /// Limit the access to write (the profile must allow it).
    #[arg(long)]
    pub allow_write: bool,
    /// Limit the access to destructive (the profile must allow it).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Limit the extra directories of the profile to this one. Repeatable.
    #[arg(long, value_name = "DIR")]
    pub allow_path: Vec<PathBuf>,
    /// Comma-separated toolsets to enable [default: all].
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Additional custom tools file. Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Timeout of one API request, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Maximum size of a tool result; larger results are an error.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,
    #[command(flatten)]
    pub call: CallArgs,
}

impl CallCommand {
    /// Аргументы `serve`, с которыми собирается сервер.
    pub fn serve_args(&self) -> ServeArgs {
        ServeArgs {
            allow_write: self.allow_write,
            allow_destructive: self.allow_destructive,
            toolsets: if self.toolsets.is_empty() {
                vec![rustore_mcp_core::ALL_TOOLSETS.to_owned()]
            } else {
                self.toolsets.clone()
            },
            tools_file: self.tools_file.clone(),
            allow_path: self.allow_path.clone(),
            timeout: self.timeout,
            max_response_bytes: self.max_response_bytes,
            ..ServeArgs::local(self.profile.as_deref().unwrap_or(DEFAULT_PROFILE))
        }
    }
}

pub async fn run(args: CallCommand) -> anyhow::Result<ExitCode> {
    init_logging(None)?;
    let repo = ProfileRepo::open_default()?;
    let approvals = approval::platform_store(EXE_STEM);
    let setup = Setup::load(
        &args.serve_args(),
        &KeyringStore,
        &repo,
        approvals.as_deref(),
    )?;
    if let Some(warning) = &setup.warning {
        eprintln!("{EXE_STEM}: {warning}");
    }
    let outcome = call_with(setup, &args.call, &mut TerminalIo::default()).await?;
    Ok(ExitCode::from(outcome.exit_code()))
}

/// Собирает сервер из `setup` и выполняет `call`. Отдельно от [`run`] для тестов.
pub async fn call_with(
    mut setup: Setup,
    args: &CallArgs,
    io: &mut dyn call::CallIo,
) -> anyhow::Result<call::CallOutcome> {
    // Профиль может включать наборы по запросу; в консоли они не нужны.
    setup.options.dynamic_toolsets = false;
    let session = Session::start(setup.server()?)
        .await
        .map_err(|err| anyhow!(err))?;
    let outcome = call::run(&session, args, io).await;
    session.close().await;
    outcome.map_err(|err| anyhow!(err))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use clap::Parser;
    use rustore_api::auth::{KeyStore, MemoryStore};
    use rustore_api::testing;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::profiles::ProfileSettings;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        call: CallCommand,
    }

    fn parse(args: &[&str]) -> CallCommand {
        let mut argv = vec!["call"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).unwrap().call
    }

    #[derive(Default)]
    struct BufferIo {
        out: Vec<u8>,
        confirm: bool,
        asked: Vec<String>,
    }

    impl call::CallIo for BufferIo {
        fn out(&mut self) -> &mut dyn Write {
            &mut self.out
        }
        fn read_stdin(&mut self) -> io::Result<String> {
            Ok(String::new())
        }
        fn confirm(&mut self, question: &str) -> io::Result<bool> {
            self.asked.push(question.to_owned());
            Ok(self.confirm)
        }
    }

    /// Профиль во временном каталоге, ключ в памяти, API — wiremock: системный keychain и
    /// сеть не трогаются.
    fn load_setup(
        args: &CallCommand,
        settings: ProfileSettings,
        api: &MockServer,
    ) -> (tempfile::TempDir, Setup) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        repo.upsert("ci", settings).unwrap();
        let store = MemoryStore::new();
        store.set("ci", &testing::api_key()).unwrap();
        let mut setup = Setup::load(&args.serve_args(), &store, &repo, None).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        // В тестах текущий бинарник — тестовый, подкоманды `jq-worker` у него нет.
        setup.options.jq = rustore_mcp_core::JqRunner::in_process();
        (dir, setup)
    }

    fn settings(write: bool, destructive: bool) -> ProfileSettings {
        ProfileSettings {
            allow_write: write,
            allow_destructive: destructive,
            // Наборы по запросу в профиле не мешают консоли.
            dynamic_toolsets: true,
            ..ProfileSettings::default()
        }
    }

    #[test]
    fn parses_flags_and_tool_arguments() {
        let args = parse(&[
            "--profile",
            "ci",
            "--allow-write",
            "list_apps",
            "pageSize=5",
            "appName=Demo",
        ]);
        assert_eq!(args.call.tool.as_deref(), Some("list_apps"));
        assert_eq!(args.call.params, ["pageSize=5", "appName=Demo"]);
        let serve = args.serve_args();
        assert_eq!(serve.toolsets, ["all"]);
        assert_eq!(serve.profile.as_deref(), Some("ci"));
        assert!(serve.allow_write && !serve.dynamic_toolsets && !serve.http);
        assert_eq!(
            parse(&["--toolsets", "apps"]).serve_args().toolsets,
            ["apps"]
        );
        assert_eq!(parse(&[]).serve_args().profile.as_deref(), Some("default"));
    }

    #[tokio::test]
    async fn lists_and_calls_tools_within_the_profile_access() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/public/auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {"jwe": "jwe-call-test-token", "ttl": 900}
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .and(query_param("pageSize", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {"content": [{"packageName": "com.example.demo", "appName": "Demo"}]}
            })))
            .expect(1)
            .mount(&api)
            .await;

        let args = parse(&["--profile", "ci"]);
        let (_dir, setup) = load_setup(&args, settings(false, false), &api);
        let mut io = BufferIo::default();
        let outcome = call_with(setup, &args.call, &mut io).await.unwrap();
        assert_eq!(outcome.exit_code(), 0);
        let list = String::from_utf8(io.out).unwrap();
        assert!(list.contains("list_apps"), "{list}");
        // Все наборы, но только чтение: инструментов записи нет.
        assert!(list.contains("list_payments_by_date"), "{list}");
        assert!(!list.contains("create_draft_version"), "{list}");
        assert!(!list.contains("enable_toolset"), "{list}");

        let args = parse(&["--profile", "ci", "list_apps", "pageSize=5"]);
        let (_dir, setup) = load_setup(&args, settings(false, false), &api);
        let mut io = BufferIo::default();
        call_with(setup, &args.call, &mut io).await.unwrap();
        let out = String::from_utf8(io.out).unwrap();
        assert!(out.contains("com.example.demo"), "{out}");
        // Токен доступа агенту не показывается.
        assert!(!out.contains("jwe-call-test-token"), "{out}");
    }

    #[tokio::test]
    async fn destructive_tools_ask_first() {
        let api = MockServer::start().await;
        let args = parse(&[
            "--profile",
            "ci",
            "delete_review_answer",
            "packageName=com.example.demo",
            "feedbackId=5",
        ]);
        let (_dir, setup) = load_setup(&args, settings(true, true), &api);
        let mut io = BufferIo::default();
        let err = call_with(setup, &args.call, &mut io).await.unwrap_err();
        assert!(err.to_string().contains("delete_review_answer"), "{err}");
        assert_eq!(io.asked.len(), 1);
        assert!(api.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn flags_beyond_the_profile_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        repo.upsert("ci", settings(false, false)).unwrap();
        let store = MemoryStore::new();
        store.set("ci", &testing::api_key()).unwrap();
        let args = parse(&["--profile", "ci", "--allow-write"]);
        let err = Setup::load(&args.serve_args(), &store, &repo, None)
            .err()
            .unwrap();
        let text = format!("{err:#}");
        assert!(text.contains("`rustore-mcp ui` (profile `ci`)"), "{text}");
    }
}
