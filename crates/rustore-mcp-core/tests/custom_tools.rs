//! Пользовательские инструменты из `tools.toml`: видны агенту в `tools/list`, отправляют
//! фиксированные аргументы, применяют свой фильтр `jq`, не регистрируются выше уровня
//! доступа. RuStore API подменён `wiremock`.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rustore_api::executor::{ApiClient, ClientConfig, RetryPolicy};
use rustore_api::{AccessLevel, StaticToken};
use rustore_mcp_core::{CustomTool, CustomToolsFile, FilePolicy, RuStoreServer, ServerOptions};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Те же поля, что в `tools.toml` (разбор TOML проверяют тесты бинарника).
fn custom_tools() -> Vec<CustomTool> {
    let file: CustomToolsFile = serde_json::from_value(json!({
        "tools": [
            {
                "name": "my_app_reviews",
                "description": "Latest reviews of my app",
                "operation": "list_reviews",
                "jq": ".[] | {commentId, text}",
                "args": {
                    "size": 2,
                    "packageName": "com.example.app"
                }
            },
            {
                "name": "draft_my_app",
                "description": "Create a draft version of my app",
                "operation": "create_draft_version",
                "args": {"packageName": "com.example.app"}
            }
        ]
    }))
    .unwrap();
    file.tools
        .into_iter()
        .map(|def| CustomTool::compile(def, "tools.toml").unwrap())
        .collect()
}

fn server(api: &MockServer, max_access: AccessLevel) -> RuStoreServer {
    let mut config =
        ClientConfig::new(Some(Arc::new(StaticToken::new("test-token-value")))).unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    let client = ApiClient::new(config).unwrap();
    let options = ServerOptions {
        max_access,
        custom_tools: custom_tools(),
        ..ServerOptions::default()
    };
    RuStoreServer::new(client, options, FilePolicy::new(&[]).unwrap()).unwrap()
}

/// Ответ `list_reviews` в конверте RuStore API.
fn reviews() -> Value {
    json!({
        "code": "OK",
        "message": null,
        "body": [
            {"commentId": 1, "text": "One", "appVersionName": "1.0"},
            {"commentId": 2, "text": "Two", "appVersionName": "1.0"}
        ],
        "timestamp": "2023-07-27T10:28:59+03:00"
    })
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

#[tokio::test]
async fn custom_tool_is_listed_and_calls_operation() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application/com.example.app/comment"))
        .and(query_param("size", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reviews()))
        .expect(1)
        .mount(&api)
        .await;

    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let server = server(&api, AccessLevel::Read);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "my_app_reviews")
        .expect("пользовательский инструмент в tools/list");
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.starts_with("Latest reviews of my app\n"),
        "{description}"
    );
    assert!(description.contains("`list_reviews`"), "{description}");
    let properties = tool.input_schema.get("properties").unwrap();
    // Фиксированные аргументы убраны из схемы, дополнительный фильтр остаётся.
    assert!(properties.get("size").is_none(), "{properties}");
    assert!(properties.get("packageName").is_none(), "{properties}");
    assert!(properties.get("jq").is_some(), "{properties}");
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    // Инструмент записи при уровне «чтение» не зарегистрирован.
    assert!(!tools.iter().any(|t| t.name == "draft_my_app"));
    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(instructions.contains("Custom tools"), "{instructions}");

    let result = client
        .call_tool(CallToolRequestParams::new("my_app_reviews").with_arguments(JsonObject::new()))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    // Конверт снят исполнителем: фильтр получает `body`; список без постраничной выдачи.
    assert_eq!(
        value,
        json!([
            {"commentId": 1, "text": "One"},
            {"commentId": 2, "text": "Two"}
        ])
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn agent_filter_is_applied_after_tool_filter() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application/com.example.app/comment"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reviews()))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read);
    let result = server
        .call("my_app_reviews", args(json!({"jq": ".commentId"})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), "[1,2]");
}

#[tokio::test]
async fn fixed_arguments_cannot_be_overridden() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reviews()))
        .expect(0)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read);
    let result = server
        .call("my_app_reviews", args(json!({"size": 100})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("size"), "{}", text(&result));
}

#[tokio::test]
async fn write_tool_needs_write_access() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/public/v1/application/com.example.app/version"))
        .and(body_json(json!({"whatsNew": "Fixes"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"code": "OK", "message": null, "body": 243242, "timestamp": "2023-07-27T10:28:59+03:00"}),
        ))
        .expect(1)
        .mount(&api)
        .await;

    let read_only = server(&api, AccessLevel::Read);
    assert!(read_only.registry().get("draft_my_app").is_none());
    let result = read_only
        .call("draft_my_app", args(json!({"body": {"whatsNew": "Fixes"}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"), "{}", text(&result));

    let writer = server(&api, AccessLevel::Write);
    let registry = writer.registry();
    let tool = registry.get("draft_my_app").unwrap();
    assert_eq!(
        tool.tool.annotations.as_ref().unwrap().read_only_hint,
        Some(false)
    );
    let result = writer
        .call("draft_my_app", args(json!({"body": {"whatsNew": "Fixes"}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
}
