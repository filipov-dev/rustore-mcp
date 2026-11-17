//! VK ID и имена пользователей не попадают в логи инструментов доступов.
//!
//! Отдельный тестовый бинарник с одним тестом: подписчик tracing ставится глобально.
//! Подписчик на поток (`set_default`) среди параллельных тестов ненадёжен: tracing кеширует
//! интерес точек логирования, и часть событий до него не доходит.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::JsonObject;
use rustore_api::executor::{ApiClient, ClientConfig, RetryPolicy};
use rustore_api::{AccessLevel, StaticToken};
use rustore_mcp_core::{FilePolicy, RuStoreServer, ServerOptions};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP: &str = "com.example.alpha";

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(
        json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"}),
    )
}

fn user(id: &str, role: &str, first: &str, last: &str) -> Value {
    json!({"devVkId": id, "role": role, "firstName": first, "lastName": last})
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => panic!("не объект"),
    }
}

#[tokio::test]
async fn user_ids_and_names_stay_out_of_logs() {
    let logs = Logs::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let api = MockServer::start().await;
    let access = format!("/public/v1/application/{APP}/developer");
    Mock::given(method("GET"))
        .and(path("/public/v1/application"))
        .respond_with(ok(json!({"content": [
            {"packageName": APP, "appName": "Alpha", "appId": 1}
        ]})))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path(access.as_str()))
        .and(query_param("pageSize", "100"))
        .respond_with(ok(json!({"content": [
            user("100", "OWNER", "Ivan", "Petrov"),
            user("200", "ADMIN", "Anna", "Smirnova")
        ]})))
        .mount(&api)
        .await;
    Mock::given(method("PUT"))
        .and(path(format!("{access}/200/role/SUPPORT")))
        .respond_with(ok(
            json!({"content": [user("200", "SUPPORT", "Anna", "Smirnova")]}),
        ))
        .mount(&api)
        .await;

    let mut config = ClientConfig::new(Some(Arc::new(StaticToken::new("test-token")))).unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    let options = ServerOptions {
        max_access: AccessLevel::Destructive,
        toolsets: Some(vec!["access".into(), "apps".into()]),
        ..ServerOptions::default()
    };
    let server = RuStoreServer::new(
        ApiClient::new(config).unwrap(),
        options,
        FilePolicy::disabled(),
    )
    .unwrap();

    let summary = server
        .call("access_summary", object(json!({})))
        .await
        .unwrap();
    assert_ne!(summary.is_error, Some(true), "{summary:?}");
    let applied = server
        .call(
            "access_apply",
            object(
                json!({"devVkId": "200", "action": "grant", "role": "SUPPORT",
                          "packageNames": [APP]}),
            ),
        )
        .await
        .unwrap();
    assert_ne!(applied.is_error, Some(true), "{applied:?}");

    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("developer/{devVkId}/role"), "{text}");
    for leaked in ["/200/", "Anna", "Smirnova", "Petrov"] {
        assert!(!text.contains(leaked), "{leaked} в логах: {text}");
    }
}
