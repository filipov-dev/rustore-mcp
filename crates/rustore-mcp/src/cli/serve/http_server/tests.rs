//! Сквозные тесты HTTP-режима: настоящий HTTP-сервер на loopback, RuStore API и выпуск
//! токенов подменены `wiremock`, запросы MCP отправляются вручную через `reqwest`. Ключ
//! RuStore API сгенерирован в тесте и читается из файла с правами 0600, как секрет Docker.

use std::path::{Path, PathBuf};

use rustore_api::auth::MemoryStore;
use rustore_api::testing;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::access_tokens::hash_hex;
use super::*;

const PROTOCOL_VERSION: &str = "2025-06-18";
/// JWE-токен, который выпускает подменённый `/public/auth/` по ключу сервера.
const SERVER_TOKEN: &str = "jwe-http-server-token-0123456789";

/// Токены доступа клиентов: собираются из частей, длина — не меньше минимальной.
fn access_token(name: &str) -> String {
    format!("{name}-{}", "0123456789abcdef".repeat(3))
}

/// Имя первого набора спецификации: тесты не зависят от состава наборов.
fn first_toolset() -> &'static str {
    rustore_api::spec::toolsets()[0].slug
}

fn tokens_file(dir: &Path) -> PathBuf {
    let text = format!(
        r#"
        [[tokens]]
        name = "reader"
        sha256 = "{reader}"

        [[tokens]]
        name = "writer"
        sha256 = "{writer}"
        access = "write"
        toolsets = ["{toolset}"]
        "#,
        reader = hash_hex(&access_token("reader")),
        writer = hash_hex(&access_token("writer")),
        toolset = first_toolset(),
    );
    let path = dir.join("tokens.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn key_file(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("rustore-key.json");
    let text = json!({
        "keyId": testing::TEST_KEY_ID,
        "privateKey": testing::private_key_base64()
    });
    std::fs::write(&path, text.to_string()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

#[derive(Debug, clap::Parser)]
struct Wrapper {
    #[command(flatten)]
    serve: ServeArgs,
}

fn try_parse(args: &[&str]) -> Result<ServeArgs, clap::Error> {
    use clap::Parser;
    let mut argv = vec!["serve"];
    argv.extend_from_slice(args);
    Wrapper::try_parse_from(argv).map(|w| w.serve)
}

fn load(args: &[&str]) -> anyhow::Result<HttpSetup> {
    HttpSetup::load(&try_parse(args).unwrap(), &MemoryStore::new(), None)
}

/// Средства теста: временный каталог, файлы ключа и токенов.
struct Fixture {
    _dir: tempfile::TempDir,
    tokens: String,
    key: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let tokens = tokens_file(dir.path()).to_str().unwrap().to_owned();
    let key = key_file(dir.path()).to_str().unwrap().to_owned();
    Fixture {
        _dir: dir,
        tokens,
        key,
    }
}

struct TestServer {
    base: String,
    http: reqwest::Client,
    cancel: CancellationToken,
    _fixture: Fixture,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Сервер из флагов `serve --http ...` с RuStore API и выпуском токенов на `api`.
async fn start(api: &MockServer, extra: &[&str]) -> TestServer {
    let fixture = fixture();
    let mut args = vec![
        "--http",
        "--access-tokens",
        fixture.tokens.as_str(),
        "--key-file",
        fixture.key.as_str(),
        "--allow-write",
    ];
    args.extend_from_slice(extra);
    let mut setup = load(&args).unwrap();
    setup.config.base_url = api.uri().parse().unwrap();
    setup.config.retry.max_retries = 0;
    let tokens = KeyTokens::with_base_url(testing::api_key(), &setup.config.base_url).unwrap();
    setup.config.tokens = Some(Arc::new(tokens));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut options = setup.http.clone();
    options.allowed_hosts = allowed_hosts(addr, &[]);
    let clients = setup.clients().unwrap();
    let cancel = CancellationToken::new();
    let app = router(clients, &options, cancel.clone());
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    TestServer {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        cancel,
        _fixture: fixture,
    }
}

impl TestServer {
    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{MCP_PATH}", self.base))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(body.to_string())
    }

    fn authed(&self, name: &str, body: &Value) -> reqwest::RequestBuilder {
        self.post(body)
            .header("authorization", format!("Bearer {}", access_token(name)))
    }

    async fn send(request: reqwest::RequestBuilder) -> (u16, String) {
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn rpc(&self, name: &str, body: &Value) -> Value {
        let (status, body) = Self::send(self.authed(name, body)).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn tools(&self, name: &str) -> Vec<Value> {
        let message = self.rpc(name, &list_tools()).await;
        message["result"]["tools"].as_array().unwrap().clone()
    }
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
}

fn list_tools() -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
}

fn call(name: &str, args: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {"name": name, "arguments": args}
    })
}

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

fn assert_no_secrets(text: &str) {
    let private = testing::private_key_base64();
    assert!(!text.contains(&private[..64]), "key material in: {text}");
    assert!(!text.contains(testing::TEST_KEY_ID), "key ID in: {text}");
    for name in ["reader", "writer"] {
        assert!(
            !text.contains(&access_token(name)),
            "access token in: {text}"
        );
    }
    assert!(!text.contains(SERVER_TOKEN), "{text}");
}

async fn mount_rustore(api: &MockServer, token_requests: u64) {
    Mock::given(method("POST"))
        .and(path("/public/auth/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": {"jwe": SERVER_TOKEN, "ttl": 900}
        })))
        .expect(token_requests)
        .mount(api)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application"))
        .and(header("public-token", SERVER_TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": {"content": [{"packageName": "com.example.app"}], "echo": SERVER_TOKEN}
        })))
        .expect(1)
        .mount(api)
        .await;
}

#[tokio::test]
async fn health_needs_no_token() {
    let api = MockServer::start().await;
    let server = start(&api, &[]).await;
    let response = server
        .http
        .get(format!("{}{HEALTH_PATH}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "rustore-mcp");
    assert_eq!(body["mcp_endpoint"], MCP_PATH);
}

#[tokio::test]
async fn missing_malformed_or_foreign_token_is_401() {
    let api = MockServer::start().await;
    let server = start(&api, &[]).await;

    let response = server.post(&initialize()).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(
        response.headers()["www-authenticate"],
        "Bearer realm=\"rustore-mcp\""
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("Authorization: Bearer <access token>"),
        "{body}"
    );

    let foreign = access_token("stranger");
    let short = "short-token";
    let cases = [
        (format!("Bearer {foreign}"), "invalid_token"),
        (format!("bearer {foreign}"), "invalid_token"),
        (format!("Bearer {short}"), "invalid_request"),
        (
            format!("Basic {}", access_token("reader")),
            "invalid_request",
        ),
        (access_token("reader"), "invalid_request"),
        ("Bearer".to_owned(), "invalid_request"),
    ];
    for (value, error) in cases {
        let response = server
            .post(&initialize())
            .header("authorization", value.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401, "{value}");
        let challenge = response.headers()["www-authenticate"].to_str().unwrap();
        assert!(challenge.contains(&format!("error=\"{error}\"")), "{value}");
        let body = response.text().await.unwrap();
        assert!(!body.contains(&foreign) && !body.contains(short), "{body}");
        assert_no_secrets(&body);
    }

    // Два заголовка Authorization — неоднозначно, отклоняется.
    let response = server
        .post(&initialize())
        .header(
            "authorization",
            format!("Bearer {}", access_token("reader")),
        )
        .header(
            "authorization",
            format!("Bearer {}", access_token("writer")),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);

    // До RuStore API и выпуска токенов ничего не дошло.
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn foreign_host_and_origin_are_403_even_without_token() {
    let api = MockServer::start().await;
    let server = start(&api, &["--allowed-origin", "http://localhost:6274"]).await;
    let (status, body) =
        TestServer::send(server.post(&initialize()).header("host", "evil.example")).await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("--allowed-host"), "{body}");
    let (status, body) = TestServer::send(
        server
            .authed("reader", &initialize())
            .header("origin", "http://evil.example"),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("--allowed-origin"), "{body}");
    let (status, _) = TestServer::send(
        server
            .authed("reader", &initialize())
            .header("origin", "http://localhost:6274"),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn reader_calls_rustore_with_the_server_key() {
    let api = MockServer::start().await;
    mount_rustore(&api, 1).await;
    let server = start(&api, &[]).await;

    let message = server.rpc("reader", &initialize()).await;
    let text = message.to_string();
    let instructions = message["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("operator of the"), "{instructions}");
    assert!(!instructions.contains("auth login"), "{instructions}");
    assert_no_secrets(&text);

    let tools = server.tools("reader").await;
    let tool_names = names(&tools);
    assert!(
        tool_names.contains(&"api_request".to_owned()),
        "{tool_names:?}"
    );
    // Только чтение, без файловых аргументов и динамических наборов.
    assert!(
        tools
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == json!(true))
    );
    let listed = serde_json::to_string(&tools).unwrap();
    assert!(!listed.contains("\"save_to\""), "file arguments listed");
    assert!(!tool_names.contains(&"enable_toolset".to_owned()));

    let result = server
        .rpc(
            "reader",
            &call(
                "api_request",
                json!({"method": "GET", "path": "/public/v1/application"}),
            ),
        )
        .await;
    let text = result.to_string();
    assert_eq!(result["result"]["isError"], json!(false), "{text}");
    assert!(text.contains("com.example.app"), "{text}");
    assert_no_secrets(&text);

    // Ключ ушёл в RuStore только подписью: в запросах нет приватного ключа, а токен доступа
    // клиента к RuStore не уходит.
    let private = testing::private_key_base64();
    for request in api.received_requests().await.unwrap() {
        let body = String::from_utf8_lossy(&request.body).to_string();
        assert!(!body.contains(&private[..64]), "{body}");
        for (_, value) in request.headers.iter() {
            let value = value.to_str().unwrap_or_default();
            assert!(!value.contains(&access_token("reader")), "{value}");
        }
    }
}

#[tokio::test]
async fn token_settings_choose_access_and_toolsets() {
    let api = MockServer::start().await;
    let server = start(&api, &[]).await;
    let writer_tools = server.tools("writer").await;
    let writer = names(&writer_tools);
    assert!(
        writer_tools
            .iter()
            .any(|t| t["annotations"]["readOnlyHint"] == json!(false)),
        "{writer:?}"
    );
    let reader_tools = server.tools("reader").await;
    assert!(
        reader_tools
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == json!(true))
    );
    // Удалять не может никто: сервер запущен с --allow-write.
    let message = server
        .rpc(
            "writer",
            &call(
                "api_request",
                json!({"method": "DELETE", "path": "/public/v1/application/x"}),
            ),
        )
        .await
        .to_string();
    assert!(message.contains("--allow-destructive"), "{message}");
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn large_request_body_is_413() {
    let api = MockServer::start().await;
    let server = start(&api, &["--max-request-bytes", "1024"]).await;
    let big = call("api_request", json!({"jq": "x".repeat(4096)}));
    let (status, _) = TestServer::send(server.authed("reader", &big)).await;
    assert_eq!(status, 413);
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[test]
fn flags_and_files_are_checked_at_start() {
    let fixture = fixture();
    let (tokens, key) = (fixture.tokens.as_str(), fixture.key.as_str());

    // Флаги HTTP-режима без --http и флаги stdio с --http — ошибки разбора.
    for args in [
        &["--listen", "127.0.0.1:1"][..],
        &["--access-tokens", tokens],
        &["--key-file", key],
        &["--http", "--status"],
        &["--http", "--dynamic-toolsets"],
        &["--http", "--allow-path", "/tmp"],
        &["--http", "--key-id", "x"],
        &["--push-file", "push.json"],
    ] {
        assert!(try_parse(args).is_err(), "{args:?}");
    }

    let err = load(&["--http"]).err().unwrap().to_string();
    assert!(err.contains("--access-tokens"), "{err}");
    // Токен с записью при сервере только на чтение — ошибка, а не понижение.
    let err = format!(
        "{:#}",
        load(&["--http", "--access-tokens", tokens, "--key-file", key])
            .err()
            .unwrap()
    );
    assert!(err.contains("token `writer` has access `write`"), "{err}");

    // Ключ из хранилища: без каталога профилей — подсказка про --key-file.
    let err = format!(
        "{:#}",
        load(&["--http", "--access-tokens", tokens, "--allow-write"])
            .err()
            .unwrap()
    );
    assert!(err.contains("--key-file"), "{err}");

    // Ключ, открытый другим пользователям, — ошибка запуска.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = format!(
            "{:#}",
            load(&[
                "--http",
                "--access-tokens",
                tokens,
                "--key-file",
                key,
                "--allow-write"
            ])
            .err()
            .unwrap()
        );
        assert!(err.contains("accessible by other users"), "{err}");
        assert_no_secrets(&err);
        std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let setup = load(&[
        "--http",
        "--access-tokens",
        tokens,
        "--key-file",
        key,
        "--allow-write",
    ])
    .unwrap();
    assert_eq!(setup.listen.to_string(), DEFAULT_LISTEN);
    assert_eq!(setup.http.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
    assert_eq!(setup.options.max_access, AccessLevel::Write);
    assert!(!setup.options.local_machine);
    assert!(setup.config.tokens.is_some());
    // В логах запуска — только имена токенов.
    let debug = format!("{:?}", setup.tokens);
    assert!(debug.contains("reader") && !debug.contains(&hash_hex(&access_token("reader"))));
    let clients = setup.clients().unwrap();
    assert_eq!(clients.servers.len(), 2);
    assert_eq!(clients.servers[0].options().toolsets, None);
    assert_eq!(
        clients.servers[1].options().toolsets,
        Some(vec![first_toolset().to_owned()])
    );
}

/// Push в HTTP-режиме: файл секрета `--push-file` с теми же проверками прав; с `--key-file`
/// без него push нет, системное хранилище не читается.
#[test]
fn push_file_is_read_like_the_key_file() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let (tokens, key) = (fixture.tokens.as_str(), fixture.key.as_str());
    let service_token = "push-service-token-abcdef0123456789";
    let push = fixture._dir.path().join("push.json");
    std::fs::write(
        &push,
        json!({"projectId": "myproject-b5ae1", "serviceToken": service_token}).to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&push, std::fs::Permissions::from_mode(0o400)).unwrap();
    let push = push.to_str().unwrap();
    let base = [
        "--http",
        "--access-tokens",
        tokens,
        "--key-file",
        key,
        "--allow-write",
    ];

    let setup = load(&base).unwrap();
    assert!(setup.config.push.is_none());

    let mut args = base.to_vec();
    args.extend(["--push-file", push]);
    let setup = load(&args).unwrap();
    let config = setup.config.push.as_ref().unwrap();
    assert_eq!(config.project_id, "myproject-b5ae1");
    assert!(!format!("{config:?}").contains(service_token));

    std::fs::set_permissions(push, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = format!("{:#}", load(&args).err().unwrap());
    assert!(err.contains("push file"), "{err}");
    assert!(err.contains("accessible by other users"), "{err}");
    assert!(!err.contains(service_token), "{err}");
}

#[test]
fn host_matching() {
    let check = |host: &str, allowed: &[&str]| {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_str(host).unwrap());
        let allowed: Vec<String> = allowed.iter().map(|h| (*h).to_owned()).collect();
        check_host(&headers, &allowed)
    };
    let loopback = LOOPBACK_HOSTS;
    for host in [
        "localhost:8766",
        "127.0.0.1:1",
        "[::1]:8766",
        "[::1]",
        "LOCALHOST",
    ] {
        assert_eq!(check(host, loopback), Ok(()), "{host}");
    }
    assert_eq!(
        check("evil.example:8766", loopback),
        Err(Rejection::HostNotAllowed)
    );
    assert_eq!(
        check("mcp.example.com:443", &["mcp.example.com:443"]),
        Ok(())
    );
    assert_eq!(
        check("mcp.example.com:8443", &["mcp.example.com:443"]),
        Err(Rejection::HostNotAllowed)
    );
    assert_eq!(check("", loopback), Err(Rejection::HostMissing));
    assert_eq!(
        check_host(&HeaderMap::new(), &["localhost".to_owned()]),
        Err(Rejection::HostMissing)
    );
}

#[test]
fn origin_matching() {
    let check = |origin: &str, allowed: &[&str]| {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
        let allowed: Vec<String> = allowed.iter().map(|h| (*h).to_owned()).collect();
        check_origin(&headers, &allowed).is_ok()
    };
    assert!(check_origin(&HeaderMap::new(), &[]).is_ok());
    assert!(!check("http://localhost:6274", &[]));
    assert!(check("http://localhost:6274", &["http://localhost:6274"]));
    assert!(check("https://app.example", &["https://app.example:443"]));
    assert!(check("https://app.example:443", &["https://app.example"]));
    assert!(!check("https://app.example:8443", &["https://app.example"]));
    assert!(check("http://localhost:1234", &["http://localhost:*"]));
    assert!(!check("https://localhost:1234", &["http://localhost:*"]));
    assert!(check("null", &["null"]));
    assert!(!check("null", &["http://localhost:*"]));
    assert!(!check("not an origin", &["http://localhost:*"]));
}

#[test]
fn allowed_hosts_include_specific_listen_address() {
    let loopback = allowed_hosts("127.0.0.1:8766".parse().unwrap(), &[]);
    assert_eq!(loopback, LOOPBACK_HOSTS);
    let any = allowed_hosts(
        "0.0.0.0:8766".parse().unwrap(),
        &["rustore-mcp".into(), " ".into(), "localhost".into()],
    );
    assert_eq!(any, ["localhost", "127.0.0.1", "::1", "rustore-mcp"]);
    let specific = allowed_hosts("10.0.0.5:8766".parse().unwrap(), &[]);
    assert_eq!(specific, ["localhost", "127.0.0.1", "::1", "10.0.0.5"]);
}
