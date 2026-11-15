//! `rustore-mcp serve` — запуск MCP-сервера через stdio или HTTP (`--http`).
//!
//! stdio: stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Ключ RuStore API берётся только из профиля в системном хранилище; по нему сервер получает
//! JWE-токен (обновляется по сроку и после 401, только в памяти). Переменные окружения ключ
//! не задают.
//!
//! HTTP ([`http_server`]): ключ остаётся на сервере (профиль или файл секрета
//! `--key-file`), клиенты присылают только свой токен доступа к MCP
//! ([`access_tokens`]). Уровень доступа и наборы — флагами и настройками токенов доступа,
//! локальные файлы выключены.
//!
//! `serve --status` печатает действующие настройки (профиль, ключ, уровень доступа, наборы,
//! разрешённые каталоги) и выходит, не запуская сервер и не обращаясь к API.
//!
//! Пользовательские инструменты ([`crate::cli::tools`]) читаются при запуске: в stdio — файл
//! пользователя и `--tools-file`, в HTTP — только `--tools-file`. Фильтры `jq` выполняются в
//! дочернем процессе этого бинарника ([`crate::cli::jq_worker`]).

mod access_tokens;
mod http_server;
mod key_file;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::Args;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rustore_api::auth::{AuthError, KEYRING_SERVICE, KeyStore, KeyringStore};
use rustore_api::executor::{ApiClient, ClientConfig};
use rustore_api::{AccessLevel, ApiKey, KeyTokens};
use rustore_mcp_core::{
    ALL_TOOLSETS, FilePolicy, RuStoreServer, ServerOptions, ToolRegistry, access_from_flags,
    access_name, normalize_toolsets,
};
use tracing_subscriber::EnvFilter;

use crate::cli::{jq_worker, tools};
use crate::profiles::{
    DEFAULT_PROFILE, ProfileRepo, ProfileSettings, load_profile_with, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Лимит тела ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Уровень логов в stderr, если не задан ни `--log-level`, ни `RUST_LOG`.
const DEFAULT_LOG_LEVEL: &str = "warn";

#[derive(Debug, Clone, Args)]
#[command(after_help = "\
Access level and toolsets: flags given on the command line win; without them the settings \
saved for the profile are used. Without any, the server is read-only and exposes the default \
toolsets. --allow-destructive implies --allow-write. Use \
--toolsets all for every toolset; the error for an unknown name lists the available ones.

Dynamic toolsets: with --dynamic-toolsets (or the profile setting) the server starts with only \
`api_search`, `api_request`, `list_toolsets`, `enable_toolset` and `disable_toolset`, plus the \
toolsets given with --toolsets and the custom tools; the agent enables toolsets on demand and \
the client is notified that the tool list changed. Use it with clients that load all tool \
descriptions up front and reload the list on `notifications/tools/list_changed`. Not available \
in HTTP mode, which keeps no sessions.

Response filter: every API tool and `api_request` accept an optional `jq` argument (jaq \
dialect) applied to the JSON response after credentials are redacted; --max-response-bytes then \
limits the filtered result. Filters run in a child process without environment, files or \
network, with a timeout.

Custom tools: tools defined in tools.toml next to profiles.toml (~/.config/rustore-mcp/tools.toml) \
are registered when the file exists; files from other places (for example, a project \
repository) only with --tools-file. A custom tool calls one API operation with fixed arguments \
and an optional `jq` filter, and has the access level of that operation. Check the files with \
`rustore-mcp tools check`.

RuStore API key: read only from the profile in the system keychain (the key ID and the private \
key); store it with `rustore-mcp auth login`. The key is never read from environment variables \
or command-line arguments. The server signs a token request with the key, keeps the JWE token \
in memory only, renews it before it expires, and after HTTP 401 retries the request once with a \
new token. Default package names of the profile (`auth login --package`) are shown to the agent.

Local files: file arguments of tools (uploads, `save_to` for downloads) must be inside the \
server's working directory (unless it is the filesystem root or the home directory), the system \
temp directory, or a directory given with --allow-path. The temp directory is allowed for \
scratch files and downloads; other programs share it, so keep nothing private there. Paths are \
checked after resolving `..` and symbolic links: a link that leads outside these directories is \
rejected, and existing files are never overwritten.

Logs go to stderr only (stdout carries the MCP protocol). The level comes from --log-level, \
otherwise from RUST_LOG, otherwise `warn`.

HTTP mode (--http): the server listens on --listen (default 127.0.0.1:8766) and serves MCP at \
/mcp (Streamable HTTP, stateless, JSON responses) and a health check at /health. The RuStore API \
key stays on the server: from the keychain of --profile, or from a secret file given with \
--key-file (Unix only, mode 0600 or 0400, for Docker and Kubernetes secrets; add --key-id when \
the file holds only the private key). Clients send only their access token in \
`Authorization: Bearer <access token>`; --access-tokens names a TOML file with the SHA-256 of \
every token, its name, access level (read, write or destructive, not above the server's flags) \
and toolsets: \n\n  [[tokens]]\n  name = \"ci\"\n  sha256 = \"<64 hex characters>\"\n  access = \"read\"\n\n\
Make a token with `openssl rand -hex 32` and its hash with `printf %s \"$TOKEN\" | sha256sum`. \
Local files are disabled. The Host header must be a loopback name, the --listen address or an \
--allowed-host; browser requests are rejected unless their Origin is allowed with \
--allowed-origin. Outside loopback, put the server behind a TLS reverse proxy.")]
pub struct ServeArgs {
    /// Profile with the RuStore API key [default: default]. With --http: the key of the
    /// server, unless --key-file is given.
    #[arg(long)]
    pub profile: Option<String>,
    /// Register tools that create or modify data.
    #[arg(long)]
    pub allow_write: bool,
    /// Register tools that delete data (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable, or `all`. With --dynamic-toolsets: the toolsets
    /// enabled at start.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Load toolsets on demand: start with only the search, request and toolset tools, and
    /// let the agent enable toolsets with `enable_toolset`. Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub dynamic_toolsets: bool,
    /// Additional custom tools file (same format as ~/.config/rustore-mcp/tools.toml).
    /// Repeatable. In HTTP mode only these files are read.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Extra directory that tools may read files from or save files to. Repeatable.
    /// Not available with --http.
    #[arg(long, value_name = "DIR", conflicts_with = "http")]
    pub allow_path: Vec<PathBuf>,
    /// Timeout of one API request, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Maximum size of an API response returned to the agent; larger responses are an error.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,
    /// Log level or filter for stderr, e.g. `info` or `rustore_mcp_core=debug`.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// Print the effective profile, key, access level and toolsets, then exit
    /// without starting the server. Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub status: bool,
    /// Serve MCP over HTTP instead of stdio; clients send an access token, the key stays on
    /// the server.
    #[arg(long)]
    pub http: bool,
    /// HTTP mode: TOML file with the SHA-256 of the clients' access tokens and their access
    /// levels and toolsets.
    #[arg(long, value_name = "PATH", requires = "http")]
    pub access_tokens: Option<PathBuf>,
    /// HTTP mode: secret file with the RuStore API key (Unix, mode 0600 or 0400) instead of
    /// the keychain of --profile.
    #[arg(long, value_name = "PATH", requires = "http")]
    pub key_file: Option<PathBuf>,
    /// HTTP mode: key ID for a --key-file that holds only the private key.
    #[arg(long, value_name = "ID", requires = "key_file")]
    pub key_id: Option<String>,
    /// Address and port to listen on in HTTP mode [default: 127.0.0.1:8766].
    #[arg(long, value_name = "IP:PORT", requires = "http")]
    pub listen: Option<SocketAddr>,
    /// Extra allowed Host header value in HTTP mode: a name (any port) or name:port.
    /// Repeatable.
    #[arg(long, value_name = "HOST", requires = "http")]
    pub allowed_host: Vec<String>,
    /// Browser Origin allowed in HTTP mode, e.g. http://localhost:6274 (`:*` for any port).
    /// Repeatable. Requests without an Origin header are always allowed.
    #[arg(long, value_name = "ORIGIN", requires = "http")]
    pub allowed_origin: Vec<String>,
    /// Maximum size of an MCP request body in HTTP mode; larger requests get HTTP 413
    /// [default: 1048576].
    #[arg(long, value_name = "BYTES", requires = "http")]
    pub max_request_bytes: Option<usize>,
}

impl ServeArgs {
    fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or(DEFAULT_PROFILE)
    }

    /// Проверка аргументов до обращения к хранилищу.
    fn validate(&self) -> anyhow::Result<()> {
        if self.timeout == 0 {
            bail!("--timeout must be greater than 0");
        }
        if self.max_response_bytes == 0 {
            bail!("--max-response-bytes must be greater than 0");
        }
        if self.max_request_bytes == Some(0) {
            bail!("--max-request-bytes must be greater than 0");
        }
        validate_profile_name(self.profile())
    }
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    init_logging(args.log_level.as_deref())?;
    args.validate()?;
    if args.http {
        return http_server::run_from_args(&args).await;
    }
    let repo = ProfileRepo::open_default()?;
    if args.status {
        return status(&KeyringStore, &repo, &args, &mut TerminalUi);
    }
    let setup = Setup::load(&args, &KeyringStore, &repo)?;
    tracing::info!(
        profile = %setup.profile,
        access = ?setup.options.max_access,
        toolsets = ?setup.options.toolsets,
        dynamic_toolsets = setup.options.dynamic_toolsets,
        custom_tools = setup.options.custom_tools.len(),
        roots = %setup.files.describe_roots(),
        "запуск MCP-сервера RuStore"
    );
    let server = setup.server()?;
    let running = server
        .serve(stdio())
        .await
        .context("MCP initialization over stdio failed")?;
    running.waiting().await.context("MCP server task failed")?;
    Ok(())
}

/// Логи — только в stderr: stdout занят протоколом MCP.
fn init_logging(level: Option<&str>) -> anyhow::Result<()> {
    let filter = match level {
        Some(level) => EnvFilter::try_new(level)
            .map_err(|err| anyhow!("invalid --log-level `{level}`: {err}"))?,
        None => {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL))
        }
    };
    // Повторная инициализация (тесты) не ошибка: остаётся первый подписчик.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
    Ok(())
}

/// Всё, что нужно для запуска сервера: настройки ядра, политика файлов, клиент API и ключ.
pub struct Setup {
    pub profile: String,
    pub options: ServerOptions,
    pub files: FilePolicy,
    /// Настройки клиента API без источника токенов: его собирает [`Setup::server`] по
    /// `config.base_url` (тесты подменяют адрес до сборки).
    pub config: ClientConfig,
    pub key: ApiKey,
}

impl Setup {
    /// Читает профиль (ключ — из `store`) и собирает настройки сервера.
    pub fn load(
        args: &ServeArgs,
        store: &dyn KeyStore,
        repo: &ProfileRepo,
    ) -> anyhow::Result<Self> {
        args.validate()?;
        let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;
        let profile_name = args.profile();
        let profile = load_profile_with(repo, store, profile_name)
            .with_context(|| format!("cannot load profile `{profile_name}`"))?;
        let mut options = effective_options(args, &profile.settings)?;
        options.custom_tools = tools::load(Some(&tools::user_tools_file(repo)), &args.tools_file)?;
        options.jq = jq_worker::current_runner();
        let mut config = ClientConfig::new(None)
            .map_err(|err| anyhow!("cannot configure the RuStore API client: {err}"))?;
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        Ok(Self {
            profile: profile_name.to_owned(),
            options,
            files,
            config,
            key: profile.key,
        })
    }

    pub fn server(self) -> anyhow::Result<RuStoreServer> {
        let mut config = self.config;
        let tokens = KeyTokens::with_base_url(self.key, &config.base_url)
            .map_err(|err| anyhow!("cannot use the RuStore API key: {err}"))?;
        config.tokens = Some(Arc::new(tokens));
        let client = ApiClient::new(config)
            .map_err(|err| anyhow!("cannot create the RuStore API client: {err}"))?;
        RuStoreServer::new(client, self.options, self.files).map_err(|err| anyhow!(err))
    }
}

/// Откуда взят уровень доступа или наборы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Flags,
    Profile,
    Default,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Flags => "from command-line flags",
            Self::Profile => "from the profile",
            Self::Default => "default",
        }
    }
}

/// Уровень доступа: флаги командной строки, если заданы, иначе настройки профиля.
/// В обоих случаях `allow_destructive` подразумевает запись.
fn effective_access(args: &ServeArgs, settings: &ProfileSettings) -> (AccessLevel, Source) {
    if args.allow_write || args.allow_destructive {
        (
            access_from_flags(args.allow_write, args.allow_destructive),
            Source::Flags,
        )
    } else {
        let level = access_from_flags(settings.allow_write, settings.allow_destructive);
        let source = if level == AccessLevel::Read {
            Source::Default
        } else {
            Source::Profile
        };
        (level, source)
    }
}

/// Наборы: `--toolsets`, если задан, иначе из профиля; пусто — набор по умолчанию (`None`).
fn effective_toolsets(
    args: &ServeArgs,
    settings: &ProfileSettings,
) -> anyhow::Result<(Option<Vec<String>>, Source)> {
    let cli = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
    if !cli.is_empty() {
        return Ok((Some(cli), Source::Flags));
    }
    let saved =
        normalize_toolsets(&settings.toolsets).map_err(|err| anyhow!("profile settings: {err}"))?;
    Ok(if saved.is_empty() {
        (None, Source::Default)
    } else {
        (Some(saved), Source::Profile)
    })
}

fn effective_options(
    args: &ServeArgs,
    settings: &ProfileSettings,
) -> anyhow::Result<ServerOptions> {
    let (max_access, _) = effective_access(args, settings);
    let (toolsets, _) = effective_toolsets(args, settings)?;
    Ok(ServerOptions {
        max_access,
        toolsets,
        // Как и флаги доступа: флаг командной строки включает, без него — настройка профиля.
        dynamic_toolsets: args.dynamic_toolsets || settings.dynamic_toolsets,
        local_machine: true,
        profile: Some(args.profile().to_owned()),
        packages: settings.packages.clone(),
        // Пользовательские инструменты и исполнитель `jq` заполняет `Setup::load`.
        ..ServerOptions::default()
    })
}

/// `serve --status`: действующие настройки сервера без запуска и без запросов к API.
/// Ключ не выводится: печатается только keyId в маске.
pub fn status(
    store: &dyn KeyStore,
    repo: &ProfileRepo,
    args: &ServeArgs,
    ui: &mut dyn Ui,
) -> anyhow::Result<()> {
    args.validate()?;
    let profile = args.profile();
    let settings = repo.get(profile)?.unwrap_or_default();
    let key = match store.get(profile) {
        Ok(key) => Ok(Some(key)),
        Err(AuthError::NotFound { .. }) => Ok(None),
        Err(err @ AuthError::Corrupted { .. }) => Err(err.to_string()),
        Err(err) => return Err(err.into()),
    };
    let (access, access_source) = effective_access(args, &settings);
    let (toolsets, toolsets_source) = effective_toolsets(args, &settings)?;
    let tools_file = tools::user_tools_file(repo);
    let options = ServerOptions {
        max_access: access,
        toolsets,
        dynamic_toolsets: args.dynamic_toolsets || settings.dynamic_toolsets,
        custom_tools: tools::load(Some(&tools_file), &args.tools_file)?,
        local_machine: true,
        profile: Some(profile.to_owned()),
        packages: settings.packages.clone(),
        ..ServerOptions::default()
    };
    let registry = ToolRegistry::new(&options).map_err(|err| anyhow!(err))?;
    let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;

    ui.say(&format!("Profile:   {profile}"));
    let key_ready = matches!(key, Ok(Some(_)));
    match &key {
        Ok(Some(key)) => ui.say(&format!(
            "Key:       {} (RSA {} bits), stored in the system keychain (service \
             `{KEYRING_SERVICE}`)",
            key.key_id().masked(),
            key.bits()
        )),
        Ok(None) => ui.say(&format!(
            "Key:       not found; store it with `rustore-mcp auth login --profile {profile} \
             --key-file <key-file>`"
        )),
        Err(err) => ui.say(&format!("Key:       {err}")),
    }
    let packages = if settings.packages.is_empty() {
        "-".to_owned()
    } else {
        settings.packages.join(", ")
    };
    ui.say(&format!("Packages:  {packages}"));
    ui.say(&format!(
        "Access:    {} ({}; {})",
        access_name(access),
        access_description(access),
        access_source.label()
    ));
    let toolsets_text = if registry.enabled_toolsets().len() == rustore_api::spec::toolsets().len()
    {
        ALL_TOOLSETS.to_owned()
    } else if registry.enabled_toolsets().is_empty() {
        "none".to_owned()
    } else {
        registry.enabled_toolsets().join(",")
    };
    ui.say(&format!(
        "Toolsets:  {toolsets_text} ({})",
        toolsets_source.label()
    ));
    // В динамическом режиме это наборы при запуске (без выбора — ни одного), остальные
    // агент включает сам.
    let dynamic = match (args.dynamic_toolsets, settings.dynamic_toolsets) {
        (true, _) => format!("on ({})", Source::Flags.label()),
        (false, true) => format!("on ({})", Source::Profile.label()),
        (false, false) => "off".to_owned(),
    };
    ui.say(&format!("Dynamic:   {dynamic}"));
    ui.say(&format!("Tools:     {}", registry.len()));
    let custom_registered = options
        .custom_tools
        .iter()
        .filter(|tool| registry.get(&tool.name).is_some())
        .count();
    ui.say(&format!(
        "Custom:    {} defined, {custom_registered} registered (user file: {}{})",
        options.custom_tools.len(),
        tools_file.display(),
        if tools_file.exists() {
            ""
        } else {
            ", not found"
        }
    ));
    ui.say(&format!("Files:     {}", files.describe_roots()));
    ui.say(&format!("Profiles:  {}", repo.path().display()));
    if !key_ready {
        ui.say("The server cannot start until the profile has a RuStore API key.");
    }
    Ok(())
}

fn access_description(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read tools only",
        AccessLevel::Write => "read and write tools, no delete",
        AccessLevel::Destructive => "read, write and delete tools",
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use rmcp::model::CallToolRequestParams;
    use rustore_api::auth::MemoryStore;
    use rustore_api::testing;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ui::testing::ScriptedUi;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn try_parse(args: &[&str]) -> Result<ServeArgs, clap::Error> {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).map(|w| w.serve)
    }

    fn parse(args: &[&str]) -> ServeArgs {
        try_parse(args).unwrap()
    }

    #[test]
    fn http_flags_need_http_and_local_flags_conflict() {
        for extra in [
            &["--allow-path", "/tmp"][..],
            &["--status"][..],
            &["--dynamic-toolsets"][..],
        ] {
            let mut argv = vec!["--http"];
            argv.extend_from_slice(extra);
            let err = try_parse(&argv).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{err}"
            );
        }
        // Флаги HTTP без --http — ошибка, а не молчаливое игнорирование.
        for flag in [
            &["--listen", "127.0.0.1:1"][..],
            &["--allowed-origin", "http://x"][..],
            &["--allowed-host", "x"][..],
            &["--access-tokens", "t.toml"][..],
            &["--key-file", "k.json"][..],
            &["--max-request-bytes", "1"][..],
        ] {
            let err = try_parse(flag).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{err}"
            );
        }
        assert!(try_parse(&["--http", "--key-id", "x"]).is_err());
        assert!(try_parse(&["--http", "--listen", "localhost:80"]).is_err());
        // Профиль в HTTP-режиме — источник ключа сервера.
        assert_eq!(parse(&["--http", "--profile", "ci"]).profile(), "ci");
    }

    fn settings(write: bool, destructive: bool, toolsets: &[&str]) -> ProfileSettings {
        ProfileSettings {
            allow_write: write,
            allow_destructive: destructive,
            toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
            ..ProfileSettings::default()
        }
    }

    fn options(args: &[&str], saved: &ProfileSettings) -> ServerOptions {
        effective_options(&parse(args), saved).unwrap()
    }

    /// Профиль во временном каталоге и ключ в памяти: системный keychain не трогаем.
    fn fixture(
        saved: Option<ProfileSettings>,
        key: bool,
    ) -> (tempfile::TempDir, ProfileRepo, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        if let Some(saved) = saved {
            repo.upsert("ci", saved).unwrap();
        }
        if key {
            store.set("ci", &testing::api_key()).unwrap();
        }
        (dir, repo, store)
    }

    fn status_text(args: &[&str], repo: &ProfileRepo, store: &MemoryStore) -> String {
        let mut ui = ScriptedUi::default();
        status(store, repo, &parse(args), &mut ui).unwrap();
        ui.text()
    }

    #[test]
    fn defaults() {
        let args = parse(&[]);
        assert_eq!(args.profile(), "default");
        assert_eq!(args.timeout, 60);
        assert_eq!(args.max_response_bytes, 256 * 1024);
        assert!(!args.status);
        let opts = options(&[], &ProfileSettings::default());
        assert_eq!(opts.max_access, AccessLevel::Read);
        assert_eq!(opts.toolsets, None);
        assert_eq!(opts.profile.as_deref(), Some("default"));
        assert!(opts.local_machine);
    }

    #[test]
    fn cli_flags_win_over_profile() {
        let saved = settings(true, true, &["reviews"]);
        let opts = options(&[], &saved);
        assert_eq!(opts.max_access, AccessLevel::Destructive);
        assert_eq!(opts.toolsets, Some(vec!["reviews".to_owned()]));

        let opts = options(&["--allow-write", "--toolsets", "catalog,access"], &saved);
        assert_eq!(opts.max_access, AccessLevel::Write);
        assert_eq!(
            opts.toolsets,
            Some(vec!["catalog".to_owned(), "access".to_owned()])
        );

        // --allow-destructive без --allow-write — удаление и запись.
        let opts = options(&["--allow-destructive"], &settings(false, false, &[]));
        assert_eq!(opts.max_access, AccessLevel::Destructive);

        let all = options(&["--toolsets", "all"], &saved);
        assert_eq!(
            all.toolsets.unwrap().len(),
            rustore_api::spec::toolsets().len()
        );
    }

    #[test]
    fn profile_access_combinations() {
        for (write, destructive, level) in [
            (false, false, AccessLevel::Read),
            (true, false, AccessLevel::Write),
            // Профиль, написанный вручную, без allow_write: удаление подразумевает запись.
            (false, true, AccessLevel::Destructive),
            (true, true, AccessLevel::Destructive),
        ] {
            let opts = options(&[], &settings(write, destructive, &[]));
            assert_eq!(
                opts.max_access, level,
                "write={write} destructive={destructive}"
            );
        }
    }

    #[test]
    fn invalid_toolsets_are_reported() {
        let err = effective_options(
            &parse(&["--toolsets", "repos"]),
            &ProfileSettings::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown toolset `repos`"), "{err}");
        let err = effective_options(&parse(&[]), &settings(false, false, &["repos"])).unwrap_err();
        assert!(err.to_string().contains("profile settings"), "{err}");
    }

    #[test]
    fn status_shows_destructive_without_write_flag() {
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let text = status_text(&["--profile", "ci", "--allow-destructive"], &repo, &store);
        assert!(text.contains("Profile:   ci"), "{text}");
        assert!(
            text.contains("Key:       test***0001 (RSA 2048 bits)"),
            "{text}"
        );
        assert!(
            text.contains(
                "Access:    destructive (read, write and delete tools; from command-line flags)"
            ),
            "{text}"
        );
        assert!(
            text.contains("Toolsets:  apps,releases,reviews (default)"),
            "{text}"
        );
        assert!(!text.contains(testing::TEST_KEY_ID), "{text}");
        assert!(!text.contains("cannot start"), "{text}");
        let destructive = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Destructive,
            ..ServerOptions::default()
        })
        .unwrap();
        assert!(
            text.contains(&format!("Tools:     {}", destructive.len())),
            "{text}"
        );
    }

    #[test]
    fn status_reads_profile_settings() {
        // Профиль только с allow_destructive (записан вручную) — тоже удаление и запись.
        let (dir, _, store) = fixture(None, false);
        std::fs::write(
            dir.path().join("profiles.toml"),
            "[profiles.ci]\nallow_destructive = true\ntoolsets = [\"catalog\"]\n",
        )
        .unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains(
                "Access:    destructive (read, write and delete tools; from the profile)"
            ),
            "{text}"
        );
        assert!(
            text.contains("Toolsets:  catalog (from the profile)"),
            "{text}"
        );
        assert!(text.contains("Key:       not found"), "{text}");
        assert!(text.contains("cannot start"), "{text}");

        let text = status_text(
            &["--profile", "ci", "--allow-write", "--toolsets", "all"],
            &repo,
            &store,
        );
        assert!(
            text.contains(
                "Access:    write (read and write tools, no delete; from command-line flags)"
            ),
            "{text}"
        );
        assert!(
            text.contains("Toolsets:  all (from command-line flags)"),
            "{text}"
        );
    }

    #[test]
    fn setup_requires_key() {
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), false);
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo)
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("auth login --profile ci"),
            "{err:#}"
        );

        // Ключ без записи в файле профилей: настройки по умолчанию.
        let (_dir, repo, store) = fixture(None, true);
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Read);
        assert_eq!(setup.key.key_id().as_str(), testing::TEST_KEY_ID);

        assert!(Setup::load(&parse(&["--timeout", "0"]), &store, &repo).is_err());
        assert!(Setup::load(&parse(&["--profile", "a b"]), &store, &repo).is_err());
    }

    const TOOLS_TOML: &str = r#"
[[tools]]
name = "my_app_reviews"
description = "Latest reviews of my app"
operation = "list_reviews"
jq = ".[] | {commentId, text}"

[tools.args]
packageName = "com.example.app"

[[tools]]
name = "draft_my_app"
description = "Create a draft version of my app"
operation = "create_draft_version"
args = { packageName = "com.example.app" }
"#;

    #[test]
    fn dynamic_toolsets_from_flag_or_profile() {
        assert!(!options(&[], &ProfileSettings::default()).dynamic_toolsets);
        assert!(options(&["--dynamic-toolsets"], &ProfileSettings::default()).dynamic_toolsets);
        let saved = ProfileSettings {
            dynamic_toolsets: true,
            ..settings(false, false, &["listing"])
        };
        let opts = options(&[], &saved);
        assert!(opts.dynamic_toolsets);
        assert_eq!(opts.toolsets, Some(vec!["listing".to_owned()]));

        // HTTP без сессий: включённые наборы негде хранить.
        let err = try_parse(&["--http", "--dynamic-toolsets"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "{err}"
        );

        let (_dir, repo, store) = fixture(Some(saved), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("Dynamic:   on (from the profile)"), "{text}");
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("Dynamic:   off"), "{text}");
        let text = status_text(&["--profile", "ci", "--dynamic-toolsets"], &repo, &store);
        assert!(
            text.contains("Dynamic:   on (from command-line flags)"),
            "{text}"
        );
        assert!(text.contains("Toolsets:  none"), "{text}");
    }

    #[test]
    fn status_counts_custom_tools() {
        let (dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("Custom:    0 defined, 0 registered (user file: "),
            "{text}"
        );
        assert!(text.contains("tools.toml, not found)"), "{text}");

        std::fs::write(dir.path().join("tools.toml"), TOOLS_TOML).unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        // Инструмент записи определён, но без --allow-write не зарегистрирован.
        assert!(
            text.contains("Custom:    2 defined, 1 registered"),
            "{text}"
        );
        let text = status_text(&["--profile", "ci", "--allow-write"], &repo, &store);
        assert!(
            text.contains("Custom:    2 defined, 2 registered"),
            "{text}"
        );
    }

    /// JWE-токен, который выпускает подменённый `/public/auth/`.
    const SERVE_TOKEN: &str = "jwe-serve-test-token";

    async fn mount_token(api: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/public/auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {"jwe": SERVE_TOKEN, "ttl": 900}
            })))
            .expect(1)
            .mount(api)
            .await;
    }

    /// Свой инструмент из `tools.toml` рядом с профилями появляется у агента и вызывает
    /// операцию со своим фильтром.
    #[tokio::test]
    async fn serves_custom_tools_from_config_dir() {
        let api = MockServer::start().await;
        mount_token(&api).await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application/com.example.app/comment"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": [{"commentId": 1, "text": "One", "appVersionName": "1.0"}]
            })))
            .expect(1)
            .mount(&api)
            .await;

        let (dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        std::fs::write(dir.path().join("tools.toml"), TOOLS_TOML).unwrap();
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        // В тестах текущий бинарник — тестовый, подкоманды `jq-worker` у него нет.
        setup.options.jq = rustore_mcp_core::JqRunner::in_process();
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(256 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == "my_app_reviews"));
        assert!(!tools.iter().any(|t| t.name == "draft_my_app"));

        let result = client
            .call_tool(CallToolRequestParams::new("my_app_reviews"))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = result.content[0].as_text().unwrap().text.clone();
        assert_eq!(text, r#"{"commentId":1,"text":"One"}"#);

        client.cancel().await.unwrap();
        handle.await.unwrap();
    }

    /// Сервер из профиля целиком: initialize, tools/list и вызов через in-memory транспорт,
    /// API — wiremock.
    #[tokio::test]
    async fn serves_tools_from_profile() {
        let api = MockServer::start().await;
        mount_token(&api).await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .and(header("public-token", SERVE_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {"content": [], "totalElements": 0}
            })))
            .expect(1)
            .mount(&api)
            .await;

        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(256 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();

        let info = client.peer_info().unwrap();
        let instructions = info.instructions.as_deref().unwrap();
        assert!(
            instructions.contains("auth login --profile ci"),
            "{instructions}"
        );
        let tools = client.list_all_tools().await.unwrap();
        assert!(
            tools.iter().any(|t| t.name == "list_apps"),
            "{:?}",
            tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert!(
            tools
                .iter()
                .all(|t| t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true))
        );

        let result = client
            .call_tool(CallToolRequestParams::new("list_apps"))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));

        client.cancel().await.unwrap();
        handle.await.unwrap();
    }
}
