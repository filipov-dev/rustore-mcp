//! Набор `push`: регистрация по уровню доступа и набору, отправка через хост push,
//! отказ без проекта push и лимит сообщения. RuStore API и Push API подменены `wiremock`.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject};
use rustore_api::executor::{ApiClient, ClientConfig, RetryPolicy};
use rustore_api::push::{MAX_MESSAGE_BYTES, PushConfig, PushCredentials};
use rustore_api::{AccessLevel, StaticToken};
use rustore_mcp_core::{FilePolicy, RuStoreServer, ServerOptions};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PUBLIC_TOKEN: &str = "public-access-token-0123456789";
const SERVICE_TOKEN: &str = "push-service-token-abcdef0123456789";
const PROJECT: &str = "myproject-b5ae1";
const DEVICE_TOKEN: &str = "bk3RNwTe3H0:CI2k_HHwgIpoDKCIZvvDMExUdFQ3P1";

const SEND: &str = "send_push_notification";
const PUBLISH: &str = "send_push_to_topic";
const SUBSCRIBE: &str = "subscribe_to_push_topic";
const UNSUBSCRIBE: &str = "unsubscribe_from_push_topic";

fn server(
    public: &MockServer,
    push: Option<&MockServer>,
    max_access: AccessLevel,
    toolsets: Option<Vec<String>>,
) -> RuStoreServer {
    let mut config = ClientConfig::new(Some(Arc::new(StaticToken::new(PUBLIC_TOKEN)))).unwrap();
    config.base_url = public.uri().parse().unwrap();
    config.timeout = Duration::from_secs(5);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    if let Some(push) = push {
        let credentials = PushCredentials::new(PROJECT, SERVICE_TOKEN).unwrap();
        let mut push_config = PushConfig::new(&credentials).unwrap();
        push_config.base_url = push.uri().parse().unwrap();
        push_config.topics_base_url = format!("{}/topics-host", push.uri()).parse().unwrap();
        config.push = Some(push_config);
    }
    RuStoreServer::new(
        ApiClient::new(config).unwrap(),
        ServerOptions {
            max_access,
            toolsets,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap()
}

fn push_toolset() -> Option<Vec<String>> {
    Some(vec!["push".into()])
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

fn message() -> Value {
    json!({"body": {"message": {
        "token": DEVICE_TOKEN,
        "notification": {"title": "Hi", "body": "Text"}
    }}})
}

#[tokio::test]
async fn push_tools_need_the_toolset_and_the_access_level() {
    let public = MockServer::start().await;
    let names = |server: &RuStoreServer| {
        server
            .registry()
            .tools()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect::<Vec<_>>()
    };

    // По умолчанию набора push нет даже с правом удаления.
    let default = server(&public, None, AccessLevel::Destructive, None);
    let tools = names(&default);
    for name in [SEND, PUBLISH, SUBSCRIBE, UNSUBSCRIBE] {
        assert!(!tools.contains(&name.to_owned()), "{name}");
    }

    let read = server(&public, None, AccessLevel::Read, push_toolset());
    assert!(
        !names(&read)
            .iter()
            .any(|n| [SEND, PUBLISH, SUBSCRIBE, UNSUBSCRIBE].contains(&n.as_str()))
    );

    // Подписка — запись, отправка — уровень удаления.
    let write = server(&public, None, AccessLevel::Write, push_toolset());
    let tools = names(&write);
    assert!(tools.contains(&SUBSCRIBE.to_owned()));
    assert!(tools.contains(&UNSUBSCRIBE.to_owned()));
    assert!(!tools.contains(&SEND.to_owned()));
    assert!(!tools.contains(&PUBLISH.to_owned()));
    let reason = write.registry().unavailable_reason(SEND).unwrap();
    assert!(reason.contains("allow-destructive"), "{reason}");

    let destructive = server(&public, None, AccessLevel::Destructive, push_toolset());
    let tools = destructive.registry().tools();
    for name in [SEND, PUBLISH] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.destructive_hint, Some(true), "{name}");
        assert_eq!(annotations.read_only_hint, Some(false), "{name}");
    }
}

#[tokio::test]
async fn send_goes_to_the_push_host_only() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/messages:send")))
        .and(header("authorization", format!("Bearer {SERVICE_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&push)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/topics-host/v1/projects/{PROJECT}/topics/news/batch_subscribe"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": "1 error(s)",
            "errors": [{"push_token": DEVICE_TOKEN, "error": {"code": 1001, "status": "FORBIDDEN", "message": "s2s token invalid"}}]
        })))
        .expect(1)
        .mount(&push)
        .await;
    let server = server(
        &public,
        Some(&push),
        AccessLevel::Destructive,
        push_toolset(),
    );

    let result = server.call(SEND, args(message())).await.unwrap();
    assert_ne!(result.is_error, Some(true), "{}", text(&result));

    let result = server
        .call(
            SUBSCRIBE,
            args(json!({"topic": "news", "body": {"push_tokens": [DEVICE_TOKEN]}})),
        )
        .await
        .unwrap();
    let body = text(&result);
    assert_ne!(result.is_error, Some(true), "{body}");
    assert!(body.contains("s2s token invalid"), "{body}");
    assert!(!body.contains(DEVICE_TOKEN), "{body}");
    assert!(!body.contains(SERVICE_TOKEN), "{body}");

    assert!(public.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn push_errors_are_managed_and_scrubbed() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/messages:send")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
            "code": 403,
            "message": format!("token {SERVICE_TOKEN} is not valid"),
            "status": "PERMISSION_DENIED"
        }})))
        .mount(&push)
        .await;
    let server = server(
        &public,
        Some(&push),
        AccessLevel::Destructive,
        push_toolset(),
    );

    let result = server.call(SEND, args(message())).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let body = text(&result);
    assert!(body.contains("PERMISSION_DENIED"), "{body}");
    assert!(body.contains("service token"), "{body}");
    assert!(!body.contains(SERVICE_TOKEN), "{body}");
    assert!(!body.contains(DEVICE_TOKEN), "{body}");
}

#[tokio::test]
async fn nothing_is_sent_without_a_push_project_or_with_a_large_message() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;

    let unconfigured = server(&public, None, AccessLevel::Destructive, push_toolset());
    let result = unconfigured.call(SEND, args(message())).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let body = text(&result);
    assert!(body.contains("auth push login"), "{body}");

    let configured = server(
        &public,
        Some(&push),
        AccessLevel::Destructive,
        push_toolset(),
    );
    let big = json!({"body": {"message": {
        "token": DEVICE_TOKEN,
        "data": {"text": "x".repeat(MAX_MESSAGE_BYTES)}
    }}});
    let result = configured.call(SEND, args(big)).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let body = text(&result);
    assert!(body.contains("4096"), "{body}");
    assert!(!body.contains(DEVICE_TOKEN), "{body}");

    // `api_request` ходит только в Public API: путь push туда не отправляется.
    let result = configured
        .call(
            "api_request",
            args(json!({"method": "POST", "path": "/messages:send", "body": {}})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true), "{}", text(&result));

    assert!(public.received_requests().await.unwrap().is_empty());
    assert!(push.received_requests().await.unwrap().is_empty());
}
