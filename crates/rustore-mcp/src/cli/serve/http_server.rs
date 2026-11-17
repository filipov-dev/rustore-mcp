//! `serve --http`: MCP-сервер через Streamable HTTP (`POST /mcp`) с ключом на стороне сервера.
//!
//! В отличие от stdio, сервер работает не на машине агента и обслуживает нескольких
//! клиентов. Ключ RuStore API остаётся на сервере (системное хранилище по профилю или файл
//! секрета `--key-file`, [`super::key_file`]) и по сети не ходит: клиенты присылают только
//! свой токен доступа к MCP в `Authorization: Bearer <токен>` ([`super::access_tokens`]). У
//! каждого токена свой уровень доступа и наборы инструментов: для каждого сочетания при
//! запуске собирается свой [`RuStoreServer`], общий у них только источник токенов RuStore
//! (один JWE-токен по ключу сервера, обновляется по сроку).
//!
//! Обработчик `/mcp` проверяет запрос до MCP в таком порядке: `Host` и `Origin` (403, защита
//! от DNS rebinding), токен доступа (401 с `WWW-Authenticate`). Затем убирает `Authorization`
//! из запроса, чтобы его значение не попало ни в какой отладочный вывод частей запроса, и
//! передаёт запрос `rmcp`. В логи пишется только имя токена из файла.
//!
//! Сервер без состояния (stateless Streamable HTTP, ответы `application/json`): сессий нет,
//! каждый POST обрабатывается независимо, поэтому динамических наборов здесь нет. `rmcp`
//! повторно проверяет `Host` и `Origin` с теми же списками и ограничивает размер тела (413).
//! Локальных файлов у HTTP-сервера нет: файловые инструменты не регистрируются.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rustore_api::auth::KeyStore;
use rustore_api::executor::{ApiClient, ClientConfig};
use rustore_api::{AccessLevel, KeyTokens};
use rustore_mcp_core::{
    FilePolicy, RuStoreServer, ServerOptions, access_from_flags, access_name, normalize_toolsets,
};
use tokio_util::sync::CancellationToken;

use super::access_tokens::{AccessTokens, MAX_TOKEN_LEN, MIN_TOKEN_LEN};
use super::{ServeArgs, key_file};
use crate::cli::{jq_worker, tools};
use crate::profiles::{ProfileRepo, load_profile_with};

/// Адрес по умолчанию: только локальные подключения.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8766";
/// Путь MCP-эндпоинта.
pub const MCP_PATH: &str = "/mcp";
/// Путь проверки готовности (без токена).
pub const HEALTH_PATH: &str = "/health";
/// Имена хостов, разрешённые в `Host` всегда (с любым портом).
pub const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];
/// Лимит тела запроса MCP по умолчанию: 1 MiB. Аргументы инструментов — JSON без файлов.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;
const CHALLENGE: &str = "Bearer realm=\"rustore-mcp\"";

/// Настройки HTTP-транспорта.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Допустимые значения `Host` (имя — с любым портом, или `имя:порт`).
    pub allowed_hosts: Vec<String>,
    /// Допустимые `Origin` браузерных клиентов; пусто — любой присланный `Origin` отклоняется.
    pub allowed_origins: Vec<String>,
    /// Лимит тела запроса MCP, байт.
    pub max_request_bytes: usize,
}

/// Всё, что нужно HTTP-режиму: адрес, транспорт, токены доступа и шаблон сервера.
pub struct HttpSetup {
    pub listen: SocketAddr,
    pub http: HttpOptions,
    pub tokens: AccessTokens,
    /// Шаблон серверов токенов: уровень доступа сервера и наборы по умолчанию.
    pub options: ServerOptions,
    /// Клиент API с источником токенов RuStore по ключу сервера.
    pub config: ClientConfig,
}

impl HttpSetup {
    /// Собирает настройки из флагов. `repo` — каталог профилей: без него (`--key-file` в
    /// контейнере без каталога конфигурации) настройки профиля пустые.
    pub fn load(
        args: &ServeArgs,
        store: &dyn KeyStore,
        repo: Option<&ProfileRepo>,
    ) -> anyhow::Result<Self> {
        args.validate()?;
        let tokens_path = args.access_tokens.as_deref().ok_or_else(|| {
            anyhow!(
                "--http needs --access-tokens <file>: a TOML file with the SHA-256 of every \
                 access token of MCP clients (see `rustore-mcp serve --help`)"
            )
        })?;
        let max_access = access_from_flags(args.allow_write, args.allow_destructive);
        let tokens = AccessTokens::load(tokens_path, max_access)?;
        let toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;

        let profile = args.profile();
        let settings = match repo {
            Some(repo) => repo.get(profile)?.unwrap_or_default(),
            None => Default::default(),
        };
        // Ключ сервера: файл секрета (контейнер) или профиль из системного хранилища хоста.
        let key = match args.key_file.as_deref() {
            Some(path) => key_file::load(path, args.key_id.as_deref())?,
            None => {
                let repo = repo.ok_or_else(|| {
                    anyhow!(
                        "cannot read the profiles: pass the RuStore API key with \
                         --key-file <path>"
                    )
                })?;
                load_profile_with(repo, store, profile)
                    .with_context(|| format!("cannot load profile `{profile}`"))?
                    .key
            }
        };
        // Ключ уходит в источник токенов; токен выпускается при первом запросе.
        let key_tokens = KeyTokens::new(key)
            .map_err(|err| anyhow!("cannot use the RuStore API key of the server: {err}"))?;
        let mut config = ClientConfig::new(Some(Arc::new(key_tokens)))
            .map_err(|err| anyhow!("cannot configure the RuStore API client: {err}"))?;
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        // Push: файл секрета или профиль хоста. С `--key-file` (контейнер) системного
        // хранилища может не быть, поэтому тогда — только `--push-file`.
        config.push = match (args.push_file.as_deref(), args.key_file.is_some()) {
            (Some(path), _) => Some(
                rustore_api::push::PushConfig::new(&key_file::load_push(path)?).map_err(|err| {
                    anyhow!("cannot configure the RuStore Push API client: {err}")
                })?,
            ),
            (None, true) => None,
            (None, false) => super::load_push(store, profile)?,
        };

        let options = ServerOptions {
            max_access,
            toolsets: (!toolsets.is_empty()).then_some(toolsets),
            // Без сессий включённые наборы негде хранить (флаг с --http отклоняет clap).
            dynamic_toolsets: false,
            // Сервер не на машине агента: файловых инструментов нет, инструкции — для HTTP.
            local_machine: false,
            profile: None,
            packages: settings.packages.clone(),
            // Только явные файлы: каталог конфигурации пользователя к HTTP-серверу не
            // относится. Это файлы оператора сервера, а не агента.
            custom_tools: tools::load(None, &args.tools_file)?,
            // Фильтры `jq` и здесь выполняются в отдельном процессе: это защита сервера от
            // зависших фильтров, а не доступ к файлам.
            jq: jq_worker::current_runner(),
            composite_tools: rustore_mcp_core::composite::all(),
        };
        let listen = match args.listen {
            Some(listen) => listen,
            None => DEFAULT_LISTEN
                .parse()
                .context("invalid default listen address")?,
        };
        Ok(Self {
            listen,
            http: HttpOptions {
                allowed_hosts: allowed_hosts(listen, &args.allowed_host),
                allowed_origins: args.allowed_origin.clone(),
                max_request_bytes: args.max_request_bytes.unwrap_or(DEFAULT_MAX_REQUEST_BYTES),
            },
            tokens,
            options,
            config,
        })
    }

    /// Серверы токенов: по одному на сочетание уровня доступа и наборов. Ошибка — например,
    /// неизвестный набор в `--tools-file`.
    pub fn clients(self) -> anyhow::Result<Clients> {
        let client = ApiClient::new(self.config)
            .map_err(|err| anyhow!("cannot create the RuStore API client: {err}"))?;
        let mut built: Vec<(Variant, RuStoreServer)> = Vec::new();
        let mut servers = Vec::new();
        for token in self.tokens.clients() {
            let variant = Variant {
                access: token.access,
                toolsets: token
                    .toolsets
                    .clone()
                    .or_else(|| self.options.toolsets.clone()),
            };
            if let Some((_, server)) = built.iter().find(|(known, _)| known == &variant) {
                servers.push(server.clone());
                continue;
            }
            let mut options = self.options.clone();
            options.max_access = variant.access;
            options.toolsets = variant.toolsets.clone();
            // Клон клиента делит источник токенов: один JWE-токен на все серверы.
            let server = RuStoreServer::new(client.clone(), options, FilePolicy::disabled())
                .map_err(|err| anyhow!(err))
                .with_context(|| format!("access token `{}`", token.name))?;
            servers.push(server.clone());
            built.push((variant, server));
        }
        Ok(Clients {
            tokens: self.tokens,
            servers,
        })
    }
}

/// Сочетание настроек, для которого собирается отдельный сервер.
#[derive(Debug, PartialEq, Eq)]
struct Variant {
    access: AccessLevel,
    toolsets: Option<Vec<String>>,
}

/// Токены доступа и серверы клиентов (индекс сервера — номер токена).
pub struct Clients {
    tokens: AccessTokens,
    servers: Vec<RuStoreServer>,
}

/// Разрешённые `Host`: loopback-имена, конкретный адрес из `--listen` и `--allowed-host`.
pub fn allowed_hosts(listen: SocketAddr, extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| (*h).to_owned()).collect();
    let ip = listen.ip();
    if !ip.is_unspecified() && !ip.is_loopback() {
        hosts.push(ip.to_string());
    }
    for host in extra {
        let host = host.trim().to_owned();
        if !host.is_empty() && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

/// Состояние обработчика `/mcp`.
struct App {
    clients: Clients,
    config: StreamableHttpServerConfig,
    sessions: Arc<LocalSessionManager>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

/// Маршруты сервера: `/mcp` (с токеном доступа) и `/health` (без токена).
pub fn router(clients: Clients, options: &HttpOptions, cancel: CancellationToken) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(options.allowed_hosts.clone())
        .with_allowed_origins(options.allowed_origins.clone())
        // Пустой список `Origin` — отклонять любой присланный `Origin`, а не пропускать все.
        .enforce_origin_validation()
        // Без сессий: каждый запрос самостоятельный, сервер ничего не хранит между запросами.
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(options.max_request_bytes)
        .with_cancellation_token(cancel);
    let app = Arc::new(App {
        clients,
        config,
        sessions: Arc::new(LocalSessionManager::default()),
        allowed_hosts: options.allowed_hosts.clone(),
        allowed_origins: options.allowed_origins.clone(),
    });
    Router::new()
        .route(HEALTH_PATH, get(health))
        .route(MCP_PATH, any(mcp).with_state(app))
}

/// Запуск из `serve --http`: настройки, серверы токенов, ожидание SIGINT/SIGTERM.
pub async fn run_from_args(args: &ServeArgs) -> anyhow::Result<()> {
    // Каталог профилей нужен для ключа из системного хранилища и настроек профиля; с
    // `--key-file` в контейнере его может не быть.
    let repo = match ProfileRepo::open_default() {
        Ok(repo) => Some(repo),
        Err(err) if args.key_file.is_some() => {
            tracing::debug!(error = %err, "каталог профилей недоступен, настройки профиля пустые");
            None
        }
        Err(err) => return Err(err),
    };
    let setup = HttpSetup::load(args, &rustore_api::auth::KeyringStore, repo.as_ref())?;
    tracing::info!(
        tokens = ?setup.tokens,
        access = access_name(setup.options.max_access),
        custom_tools = setup.options.custom_tools.len(),
        "настройки MCP-сервера RuStore (HTTP)"
    );
    let listen = setup.listen;
    let http = setup.http.clone();
    let clients = setup.clients()?;
    run(clients, listen, http).await
}

/// Запускает сервер и ждёт SIGINT/SIGTERM.
pub async fn run(clients: Clients, listen: SocketAddr, options: HttpOptions) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener
        .local_addr()
        .context("cannot read the listen address")?;
    if !addr.ip().is_loopback() {
        // Предупреждение для человека, поэтому в stderr напрямую, независимо от уровня логов:
        // токены доступа идут открытым текстом, снаружи сервер должен быть доступен только
        // через TLS.
        eprintln!(
            "warning: listening on {addr}, not only on loopback. Access tokens travel in plain \
             HTTP: expose the server only through a TLS reverse proxy or a private network."
        );
    }
    tracing::info!(
        %addr,
        allowed_hosts = ?options.allowed_hosts,
        allowed_origins = ?options.allowed_origins,
        max_request_bytes = options.max_request_bytes,
        "запуск MCP-сервера RuStore по HTTP"
    );
    eprintln!("rustore-mcp: MCP endpoint http://{addr}{MCP_PATH}");

    let cancel = CancellationToken::new();
    let app = router(clients, &options, cancel.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("получен сигнал завершения, останавливаю сервер");
            cancel.cancel();
        })
        .await
        .context("HTTP server failed")?;
    Ok(())
}

/// Ждёт SIGINT (Ctrl+C) или SIGTERM (`docker stop`, systemd).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(%err, "не удалось подписаться на SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "не удалось подписаться на SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn health() -> Response {
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "status": "ok",
            "service": "rustore-mcp",
            "version": env!("CARGO_PKG_VERSION"),
            "mcp_endpoint": MCP_PATH,
        }),
    )
}

/// `/mcp`: проверки до MCP, затем запрос уходит серверу токена.
async fn mcp(State(app): State<Arc<App>>, mut request: Request) -> Response {
    let index = match check_request(&app, request.headers()) {
        Ok(index) => index,
        Err(rejection) => {
            tracing::debug!(?rejection, "запрос к MCP отклонён до обработки");
            return reject(rejection);
        }
    };
    request.headers_mut().remove(AUTHORIZATION);
    let Some(server) = app.clients.servers.get(index) else {
        return reject(Rejection::TokenUnknown);
    };
    if let Some(client) = app.clients.tokens.clients().nth(index) {
        tracing::debug!(client = %client.name, method = %request.method(), "запрос MCP");
    }
    // Своя сессия ядра на каждый запрос: состояние сессии не делится между запросами и
    // агентами; общие у копий только неизменяемые данные и источник токенов.
    let server = server.new_session();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::clone(&app.sessions),
        app.config.clone(),
    );
    service.handle(request).await.map(Body::new)
}

/// Почему запрос отклонён до MCP. Значения заголовков в ответ и в логи не попадают.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    HostMissing,
    HostNotAllowed,
    OriginNotAllowed,
    TokenMissing,
    TokenMalformed,
    TokenUnknown,
}

/// Номер токена доступа запроса.
fn check_request(app: &App, headers: &HeaderMap) -> Result<usize, Rejection> {
    check_host(headers, &app.allowed_hosts)?;
    check_origin(headers, &app.allowed_origins)?;
    let token = bearer_token(headers)?;
    app.clients
        .tokens
        .find(token)
        .ok_or(Rejection::TokenUnknown)
}

/// Токен доступа из единственного заголовка `Authorization: Bearer <токен>`.
fn bearer_token(headers: &HeaderMap) -> Result<&str, Rejection> {
    let value = single_header(headers, &AUTHORIZATION)
        .map_err(|()| Rejection::TokenMalformed)?
        .ok_or(Rejection::TokenMissing)?;
    let (scheme, token) = value
        .trim()
        .split_once(' ')
        .ok_or(Rejection::TokenMalformed)?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.len() < MIN_TOKEN_LEN
        || token.len() > MAX_TOKEN_LEN
        || !token.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(Rejection::TokenMalformed);
    }
    Ok(token)
}

/// Значение заголовка, если он есть ровно один раз и это ASCII; `Err` — повтор или не ASCII.
fn single_header<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

/// Хост и порт без скобок IPv6 и в нижнем регистре.
#[derive(Debug, PartialEq, Eq)]
struct Authority {
    host: String,
    port: Option<u16>,
}

fn normalize_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

fn parse_authority(value: &str) -> Option<Authority> {
    let authority = axum::http::uri::Authority::try_from(value.trim()).ok()?;
    Some(Authority {
        host: normalize_host(authority.host()),
        port: authority.port_u16(),
    })
}

/// Защита от DNS rebinding: имя в `Host` должно быть в списке. Запись без порта разрешает
/// любой порт, запись `имя:порт` — только этот порт. Те же правила у `rmcp`.
fn check_host(headers: &HeaderMap, allowed: &[String]) -> Result<(), Rejection> {
    let host = single_header(headers, &HOST)
        .ok()
        .flatten()
        .and_then(parse_authority)
        .ok_or(Rejection::HostMissing)?;
    let allowed = allowed.iter().any(|entry| {
        // `::1` без скобок не разбирается как authority: такие записи — имя без порта.
        let entry = parse_authority(entry).unwrap_or_else(|| Authority {
            host: normalize_host(entry.trim()),
            port: None,
        });
        entry.host == host.host && entry.port.is_none_or(|port| host.port == Some(port))
    });
    if allowed {
        Ok(())
    } else {
        Err(Rejection::HostNotAllowed)
    }
}

/// `Origin` браузера: (схема, хост, порт).
#[derive(Debug, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

fn parse_origin(value: &str) -> Option<Origin> {
    let uri = axum::http::Uri::try_from(value.trim()).ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let authority = uri.authority()?;
    let port = authority.port_u16().or_else(|| default_port(&scheme));
    Some(Origin {
        host: normalize_host(authority.host()),
        scheme,
        port,
    })
}

/// Запрос без `Origin` (обычный MCP-клиент, не браузер) проходит. С `Origin` — только если
/// он в списке: `схема://хост[:порт]` (без порта — порт схемы по умолчанию), `схема://хост:*`
/// (любой порт) или `null`.
fn check_origin(headers: &HeaderMap, allowed: &[String]) -> Result<(), Rejection> {
    let value = match single_header(headers, &ORIGIN) {
        Ok(None) => return Ok(()),
        Ok(Some(value)) => value.trim(),
        Err(()) => return Err(Rejection::OriginNotAllowed),
    };
    let matches = |entry: &String| {
        let entry = entry.trim();
        if entry.eq_ignore_ascii_case("null") || value.eq_ignore_ascii_case("null") {
            return entry.eq_ignore_ascii_case("null") && value.eq_ignore_ascii_case("null");
        }
        let Some(origin) = parse_origin(value) else {
            return false;
        };
        match entry.strip_suffix(":*") {
            Some(base) => parse_origin(base)
                .is_some_and(|base| base.scheme == origin.scheme && base.host == origin.host),
            None => parse_origin(entry) == Some(origin),
        }
    };
    if allowed.iter().any(matches) {
        Ok(())
    } else {
        Err(Rejection::OriginNotAllowed)
    }
}

fn reject(rejection: Rejection) -> Response {
    let (status, error, message) = match rejection {
        Rejection::HostMissing => (
            StatusCode::BAD_REQUEST,
            "bad_request",
            "missing or malformed Host header",
        ),
        Rejection::HostNotAllowed => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Host header is not allowed: start the server with --allowed-host <name> for this \
             name",
        ),
        Rejection::OriginNotAllowed => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Origin header is not allowed: start the server with --allowed-origin <origin> for \
             this browser client",
        ),
        Rejection::TokenMissing => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing access token: send `Authorization: Bearer <access token>` with a token \
             from the server operator",
        ),
        Rejection::TokenMalformed => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "malformed Authorization header: expected exactly one `Authorization: Bearer \
             <access token>`",
        ),
        Rejection::TokenUnknown => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "unknown access token: ask the server operator for a valid one",
        ),
    };
    let mut response = json_response(
        status,
        serde_json::json!({ "error": error, "message": message }),
    );
    if status == StatusCode::UNAUTHORIZED {
        let suffix = match rejection {
            Rejection::TokenMalformed => ", error=\"invalid_request\"",
            Rejection::TokenUnknown => ", error=\"invalid_token\"",
            _ => "",
        };
        if let Ok(value) = HeaderValue::from_str(&format!("{CHALLENGE}{suffix}")) {
            response.headers_mut().insert(WWW_AUTHENTICATE, value);
        }
    }
    response
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body.to_string(),
    )
        .into_response()
}

// Тесты читают ключ из файла с правами Unix.
#[cfg(all(test, unix))]
mod tests;
