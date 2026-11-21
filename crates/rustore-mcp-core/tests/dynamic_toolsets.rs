//! Динамические наборы инструментов: начальный список, `enable_toolset` с уведомлением
//! `notifications/tools/list_changed` (через сессию с `initialize` и через подписку протокола
//! 2026-07-28), уровень доступа, сессии и токены отдельных запросов. RuStore API подменён
//! `wiremock`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, JsonObject, ProtocolVersion, ServerNotification,
    SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::{ClientHandler, ClientServiceExt, RoleClient, ServiceExt};
use rustore_api::executor::{ApiClient, ClientConfig, RetryPolicy};
use rustore_api::{AccessLevel, StaticToken};
use rustore_mcp_core::{CustomTool, FilePolicy, RuStoreServer, ServerOptions};
use serde_json::{Value, json};
use tokio::sync::Notify;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CORE_TOOLS: [&str; 5] = [
    "api_request",
    "api_search",
    "disable_toolset",
    "enable_toolset",
    "list_toolsets",
];

/// Путь `get_app_rating` для приложения `com.example.app`.
const RATING_PATH: &str = "/public/v1/application/com.example.app/comment/statistic";

/// Клиент, который считает уведомления о смене списка инструментов.
#[derive(Clone, Default)]
struct CountingClient {
    changes: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ClientHandler for CountingClient {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

fn config(api: &MockServer) -> ClientConfig {
    let mut config =
        ClientConfig::new(Some(Arc::new(StaticToken::new("test-token-value")))).unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    config
}

fn server_with(api: &MockServer, options: ServerOptions) -> RuStoreServer {
    let client = ApiClient::new(config(api)).unwrap();
    RuStoreServer::new(client, options, FilePolicy::new(&[]).unwrap()).unwrap()
}

fn server(api: &MockServer, max_access: AccessLevel, dynamic: bool) -> RuStoreServer {
    server_with(
        api,
        ServerOptions {
            max_access,
            dynamic_toolsets: dynamic,
            ..ServerOptions::default()
        },
    )
}

fn spawn(server: RuStoreServer) -> tokio::io::DuplexStream {
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    client_io
}

fn args(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => panic!("не объект"),
    }
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
    name: &str,
    value: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(value)))
        .await
        .unwrap()
}

async fn tool_names<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    names
}

/// Главный сценарий: агент включает набор, получает уведомление и сразу вызывает
/// инструмент набора (с фильтром `jq`).
#[tokio::test]
async fn enable_toolset_adds_tools_and_notifies() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RATING_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "message": null,
            "body": {"averageUserRating": 4.5, "totalRatings": 10},
            "timestamp": "2023-07-27T10:28:59+03:00"
        })))
        .expect(1)
        .mount(&api)
        .await;

    let handler = CountingClient::default();
    let client = handler
        .clone()
        .serve(spawn(server(&api, AccessLevel::Read, true)))
        .await
        .unwrap();

    let info = client.peer_info().unwrap();
    let tools_capability = info.capabilities.tools.as_ref().unwrap();
    assert_eq!(tools_capability.list_changed, Some(true));
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("`enable_toolset`"), "{instructions}");
    assert!(
        instructions.contains("enabled at start: none"),
        "{instructions}"
    );

    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    // До включения набора инструмент недоступен, ошибка подсказывает `enable_toolset`.
    let result = call(
        &client,
        "get_app_rating",
        json!({"packageName": "com.example.app"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`enable_toolset`"),
        "{}",
        text(&result)
    );

    let listed: Value =
        serde_json::from_str(&text(&call(&client, "list_toolsets", json!({})).await)).unwrap();
    let reviews = listed["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["toolset"] == "reviews")
        .unwrap();
    assert_eq!(reviews["enabled"], false);

    let result = call(&client, "enable_toolset", json!({"toolset": "reviews"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 1);

    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_app_rating"), "{names:?}");
    assert!(names.iter().any(|n| n == "list_reviews"), "{names:?}");
    // Уровень доступа действует и для включённого набора.
    assert!(!names.iter().any(|n| n == "reply_to_review"), "{names:?}");
    assert!(
        !names.iter().any(|n| n == "delete_review_answer"),
        "{names:?}"
    );

    let result = call(
        &client,
        "get_app_rating",
        json!({"packageName": "com.example.app", "jq": "{averageUserRating}"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"averageUserRating":4.5}"#);

    // Повторное включение не меняет список и не шлёт уведомление.
    call(&client, "enable_toolset", json!({"toolset": "reviews"})).await;
    let result = call(&client, "disable_toolset", json!({"toolset": "reviews"})).await;
    assert_eq!(result.is_error, Some(false));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 2);
    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn write_tools_appear_only_with_write_access() {
    let api = MockServer::start().await;
    let client = ().serve(spawn(server(&api, AccessLevel::Write, true))).await.unwrap();
    call(
        &client,
        "enable_toolset",
        json!({"toolset": "releases,reviews"}),
    )
    .await;
    let names = tool_names(&client).await;
    assert!(
        names.iter().any(|n| n == "create_draft_version"),
        "{names:?}"
    );
    assert!(names.iter().any(|n| n == "list_reviews"), "{names:?}");
    // Удаление и публикация требуют `Destructive`.
    assert!(
        !names.iter().any(|n| n == "delete_review_answer"),
        "{names:?}"
    );
    assert!(!names.iter().any(|n| n == "publish_version"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn toolsets_from_flags_are_enabled_at_start() {
    let api = MockServer::start().await;
    let server = server_with(
        &api,
        ServerOptions {
            dynamic_toolsets: true,
            toolsets: Some(vec!["catalog".into()]),
            ..ServerOptions::default()
        },
    );
    assert!(server.registry().get("list_catalog_products").is_some());
    assert!(server.registry().get("get_app_rating").is_none());
    assert!(
        server.instructions().contains("enabled at start: catalog"),
        "{}",
        server.instructions()
    );
}

#[tokio::test]
async fn subscription_receives_list_changed() {
    let api = MockServer::start().await;
    let client = ()
        .serve_with_lifecycle(
            spawn(server(&api, AccessLevel::Read, true)),
            rmcp::ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let mut subscription = client
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .unwrap();
    assert_eq!(
        subscription.acknowledged(),
        &SubscriptionFilter::builder().tools_list_changed().build()
    );

    let result = call(&client, "enable_toolset", json!({"toolset": "apps"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let notification = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .expect("tools/list_changed");
    assert!(matches!(
        notification,
        ServerNotification::ToolListChangedNotification(_)
    ));
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "list_apps"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn static_mode_is_unchanged() {
    let api = MockServer::start().await;
    let client = ().serve(spawn(server(&api, AccessLevel::Read, false))).await.unwrap();
    let info = client.peer_info().unwrap();
    assert_ne!(
        info.capabilities.tools.as_ref().unwrap().list_changed,
        Some(true)
    );
    let names = tool_names(&client).await;
    assert!(names.iter().any(|n| n == "get_app_rating"));
    assert!(!names.iter().any(|n| n == "enable_toolset"));
    let result = client
        .call_tool(CallToolRequestParams::new("enable_toolset").with_arguments(JsonObject::new()))
        .await;
    assert!(result.is_err(), "неизвестный инструмент — ошибка протокола");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn sessions_are_independent() {
    let api = MockServer::start().await;
    let first = server(&api, AccessLevel::Read, true);
    let second = first.new_session();
    first
        .call("enable_toolset", args(json!({"toolset": "reviews"})))
        .await
        .unwrap();
    assert!(first.registry().get("get_app_rating").is_some());
    assert!(second.registry().get("get_app_rating").is_none());
    // Клон делит сессию.
    assert!(first.clone().registry().get("get_app_rating").is_some());
}

#[tokio::test]
async fn custom_tools_are_registered_from_start() {
    let custom = |name: &str, operation: &str| {
        CustomTool::compile(
            serde_json::from_value(json!({
                "name": name,
                "description": "Custom tool",
                "operation": operation,
                "args": {"packageName": "com.example.app"}
            }))
            .unwrap(),
            "tools.toml",
        )
        .unwrap()
    };
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RATING_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": "OK"})))
        .expect(1)
        .mount(&api)
        .await;
    let server = server_with(
        &api,
        ServerOptions {
            dynamic_toolsets: true,
            custom_tools: vec![
                custom("test_1", "get_app_rating"),
                custom("draft_my_app", "create_draft_version"),
            ],
            ..ServerOptions::default()
        },
    );
    let names = |server: &RuStoreServer| {
        let mut names: Vec<String> = server
            .registry()
            .iter()
            .map(|t| t.tool.name.to_string())
            .collect();
        names.sort();
        names
    };
    let mut expected: Vec<String> = CORE_TOOLS.iter().map(|s| s.to_string()).collect();
    expected.push("test_1".to_owned());
    expected.sort();
    assert_eq!(names(&server), expected);
    // Выше уровня доступа — не зарегистрирован, вызов объясняет, какой флаг нужен.
    let result = server
        .call("draft_my_app", args(json!({"body": {"whatsNew": "x"}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`write` access"),
        "{}",
        text(&result)
    );

    for tool in ["enable_toolset", "disable_toolset"] {
        let result = server
            .call(tool, args(json!({"toolset": "reviews"})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        assert!(server.registry().get("test_1").is_some(), "{tool}");
        // Пользовательские инструменты не попадают в списки добавленных и убранных.
        assert!(!text(&result).contains("test_1"), "{}", text(&result));
    }
    // Новая сессия тоже начинается с пользовательских инструментов.
    assert!(server.new_session().registry().get("test_1").is_some());

    let result = server.call("test_1", JsonObject::new()).await.unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
}
