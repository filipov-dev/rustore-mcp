//! Исполнитель и Push API: хост, префикс проекта, сервисный токен, ошибки и лимит сообщения.
//!
//! Два `wiremock`-сервера изображают Public API и Push API: сервисный токен не должен
//! уходить на хост Public API, а токен доступа Public API — на хост push.

use std::sync::Arc;
use std::time::Duration;

use rustore_api::StaticToken;
use rustore_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, RequestBody, RetryPolicy,
};
use rustore_api::push::{MAX_MESSAGE_BYTES, PushConfig, PushCredentials};
use rustore_api::spec::{ApiService, HttpMethod};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const PUBLIC_TOKEN: &str = "public-access-token-0123456789";
const SERVICE_TOKEN: &str = "push-service-token-abcdef0123456789";
const PROJECT: &str = "myproject-b5ae1";
const DEVICE_TOKEN: &str = "bk3RNwTe3H0:CI2k_HHwgIpoDKCIZvvDMExUdFQ3P1";

fn config(public: &MockServer, push: Option<&MockServer>) -> ClientConfig {
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
    config
}

fn send_request(message: Value) -> ApiRequest {
    let mut request = ApiRequest::new(HttpMethod::Post, "/messages:send");
    request.service = ApiService::Push;
    request.body = RequestBody::Json(json!({ "message": message }));
    request
}

fn notification() -> Value {
    json!({"token": DEVICE_TOKEN, "notification": {"title": "Hi", "body": "Text"}})
}

fn header_values(request: &Request) -> String {
    request
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn push_requests_go_to_the_push_host_with_the_service_token() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/messages:send")))
        .and(header("authorization", format!("Bearer {SERVICE_TOKEN}")))
        .and(body_json(json!({"message": notification()})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&push)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK", "message": null, "body": {"content": []}, "timestamp": "t"
        })))
        .mount(&public)
        .await;
    let client = ApiClient::new(config(&public, Some(&push))).unwrap();

    let response = client.execute(send_request(notification())).await.unwrap();
    assert_eq!(response.status, 200);
    client
        .execute(ApiRequest::new(HttpMethod::Get, "/public/v1/application"))
        .await
        .unwrap();

    // Сервисный токен — только на хосте push, токен доступа — только на хосте Public API.
    for request in public.received_requests().await.unwrap() {
        let headers = header_values(&request);
        assert!(!headers.contains(SERVICE_TOKEN), "{headers}");
        assert!(request.headers.get("authorization").is_none(), "{headers}");
        assert_eq!(
            request
                .headers
                .get("public-token")
                .and_then(|v| v.to_str().ok()),
            Some(PUBLIC_TOKEN)
        );
    }
    for request in push.received_requests().await.unwrap() {
        let headers = header_values(&request);
        assert!(!headers.contains(PUBLIC_TOKEN), "{headers}");
        assert!(request.headers.get("public-token").is_none(), "{headers}");
    }
}

#[tokio::test]
async fn topics_use_their_own_host_and_hide_device_tokens() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/topics-host/v1/projects/{PROJECT}/topics/news/batch_subscribe"
        )))
        .and(header("authorization", format!("Bearer {SERVICE_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": "1 error(s)",
            "errors": [{"push_token": DEVICE_TOKEN, "error": {"code": 1001, "status": "FORBIDDEN", "message": "s2s token invalid"}}]
        })))
        .expect(1)
        .mount(&push)
        .await;
    let client = ApiClient::new(config(&public, Some(&push))).unwrap();
    let mut request = ApiRequest::new(HttpMethod::Post, "/topics/news/batch_subscribe");
    request.service = ApiService::PushTopics;
    request.body = RequestBody::Json(json!({"push_tokens": [DEVICE_TOKEN]}));

    let response = client.execute(request).await.unwrap();
    let text = format!("{:?}", response.body);
    assert!(!text.contains(DEVICE_TOKEN), "{text}");
    assert!(text.contains("s2s token invalid"), "{text}");
    assert!(public.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn push_errors_are_parsed_and_scrubbed() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/messages:send")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
            "code": 403,
            "message": format!("token {SERVICE_TOKEN} is not valid for {DEVICE_TOKEN}"),
            "status": "PERMISSION_DENIED"
        }})))
        .mount(&push)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/topics-host/v1/projects/{PROJECT}/topics/news/publish"
        )))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 2000, "status": "BAD_REQUEST", "message": "failed to parse request"
        })))
        .mount(&push)
        .await;
    let client = ApiClient::new(config(&public, Some(&push))).unwrap();

    let err = client
        .execute(send_request(notification()))
        .await
        .unwrap_err();
    let ApiError::Http(http) = &err else {
        panic!("ожидалась Http, получено {err:?}");
    };
    assert_eq!(http.status, 403);
    assert_eq!(http.code.as_deref(), Some("PERMISSION_DENIED"));
    let text = format!("{http:?}");
    assert!(!text.contains(SERVICE_TOKEN), "{text}");
    assert!(!text.contains(DEVICE_TOKEN), "{text}");

    let mut request = ApiRequest::new(HttpMethod::Post, "/topics/news/publish");
    request.service = ApiService::PushTopics;
    request.body = RequestBody::Json(json!({"message": {"data": {"k": "v"}}}));
    let ApiError::Http(http) = client.execute(request).await.unwrap_err() else {
        panic!("ожидалась Http");
    };
    assert_eq!(http.status, 400);
    assert_eq!(http.code.as_deref(), Some("BAD_REQUEST"));
    assert_eq!(http.message.as_deref(), Some("failed to parse request"));
}

#[tokio::test]
async fn nothing_is_sent_without_push_credentials_or_with_a_large_message() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;

    // Без проекта push запрос не уходит никуда, в том числе на хост Public API.
    let client = ApiClient::new(config(&public, None)).unwrap();
    let err = client
        .execute(send_request(notification()))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::PushNotConfigured), "{err:?}");

    let client = ApiClient::new(config(&public, Some(&push))).unwrap();
    let big = json!({"token": DEVICE_TOKEN, "data": {"text": "x".repeat(MAX_MESSAGE_BYTES)}});
    let err = client.execute(send_request(big)).await.unwrap_err();
    assert!(
        matches!(err, ApiError::PushMessageTooLarge { limit, .. } if limit == MAX_MESSAGE_BYTES),
        "{err:?}"
    );

    // Путь Public API не уходит на хост push, путь push — на хост Public API.
    let mut wrong = ApiRequest::new(HttpMethod::Get, "/public/v1/application");
    wrong.service = ApiService::Push;
    assert!(matches!(
        client.execute(wrong).await.unwrap_err(),
        ApiError::InvalidPath(_)
    ));
    let wrong = ApiRequest::new(HttpMethod::Post, "/messages:send");
    assert!(matches!(
        client.execute(wrong).await.unwrap_err(),
        ApiError::InvalidPath(_)
    ));

    assert!(public.received_requests().await.unwrap().is_empty());
    assert!(push.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn push_redirects_to_other_hosts_are_not_followed() {
    let public = MockServer::start().await;
    let push = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT}/messages:send")))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/steal", public.uri()).as_str()),
        )
        .mount(&push)
        .await;
    let client = ApiClient::new(config(&public, Some(&push))).unwrap();
    let err = client
        .execute(send_request(notification()))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(307));
    assert!(public.received_requests().await.unwrap().is_empty());
}

#[test]
fn service_token_is_not_in_debug_output() {
    let credentials = PushCredentials::new(PROJECT, SERVICE_TOKEN).unwrap();
    let config = PushConfig::new(&credentials).unwrap();
    assert!(!format!("{config:?}").contains(SERVICE_TOKEN));
    assert!(!format!("{credentials:?}").contains(SERVICE_TOKEN));
    assert_eq!(config.base_url.as_str(), "https://vkpns.rustore.ru/");
    assert_eq!(
        config.topics_base_url.as_str(),
        "https://vkpns-topics.rustore.ru/"
    );
}
