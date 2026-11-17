//! Тесты исполнителя запросов на `wiremock`, без реальной сети.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rustore_api::HttpMethod;
use rustore_api::StaticToken;
use rustore_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, FileSource, MultipartPart, PAGE_PARAM,
    RequestBody, ResponseBody, RetryPolicy,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Узнаваемое фиктивное значение, чтобы искать утечки в текстах ошибок.
const TOKEN: &str = "test-token-LEAKCHECK-7f3a9";

/// Успешный ответ RuStore в конверте.
fn ok(body: serde_json::Value) -> serde_json::Value {
    json!({"code": "OK", "message": null, "body": body, "timestamp": "2024-06-18T11:49:08.290+03:00"})
}

/// Ошибка RuStore в конверте.
fn failure(message: &str) -> serde_json::Value {
    json!({"code": "ERROR", "message": message, "body": null, "timestamp": "2024-06-18T11:49:08.290+03:00"})
}

fn config(base: &str) -> ClientConfig {
    ClientConfig {
        base_url: url::Url::parse(base).unwrap(),
        tokens: Some(Arc::new(StaticToken::new(TOKEN))),
        timeout: Duration::from_secs(5),
        transfer_timeout: Duration::from_secs(30),
        max_response_bytes: 64 * 1024,
        retry: RetryPolicy {
            max_retries: 2,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(5),
        },
        push: None,
    }
}

fn client(server: &MockServer) -> ApiClient {
    ApiClient::new(config(&server.uri())).unwrap()
}

fn get(path: &str) -> ApiRequest {
    ApiRequest::new(HttpMethod::Get, path)
}

fn http_error(err: ApiError) -> rustore_api::executor::HttpError {
    match err {
        ApiError::Http(e) => *e,
        other => panic!("ожидалась Http, получено {other:?}"),
    }
}

#[tokio::test]
async fn sends_public_token_and_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application"))
        .and(header("public-token", TOKEN))
        .and(header(
            "user-agent",
            concat!("rustore-mcp/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(|req: &Request| {
            // Других заголовков авторизации нет.
            assert!(req.headers.get("authorization").is_none());
            ResponseTemplate::new(200).set_body_json(ok(json!({"appName": "x"})))
        })
        .expect(1)
        .mount(&server)
        .await;

    let response = client(&server)
        .execute(get("/public/v1/application"))
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    // Конверт снят: возвращается `body`.
    assert_eq!(response.body, ResponseBody::Json(json!({"appName": "x"})));
    assert!(response.pagination.is_empty());
}

#[tokio::test]
async fn envelope_without_body_keeps_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/public/v1/application/pkg/version/1/commit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK", "message": "Draft committed", "body": null, "timestamp": "t"
        })))
        .mount(&server)
        .await;
    let request = ApiRequest::new(
        HttpMethod::Post,
        "/public/v1/application/pkg/version/1/commit",
    );
    let response = client(&server).execute(request).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::Json(json!({"code": "OK", "message": "Draft committed", "timestamp": "t"}))
    );
}

#[tokio::test]
async fn missing_token_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.tokens = None;
    let err = ApiClient::new(cfg)
        .unwrap()
        .execute(get("/public/v1/application"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::MissingToken), "{err:?}");

    // Токен, который нельзя положить в заголовок, не отправляется и не отражается.
    for bad in ["", "  ", "bad\ntoken"] {
        let mut cfg = config(&server.uri());
        cfg.tokens = Some(Arc::new(StaticToken::new(bad)));
        let err = ApiClient::new(cfg)
            .unwrap()
            .execute(get("/public/v1/application"))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{err:?}");
        assert!(!format!("{err:?}").contains("bad\ntoken"));
    }
}

#[tokio::test]
async fn encodes_query_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application"))
        .and(query_param("appName", "a b&c=d"))
        .and(query_param("pageSize", "10"))
        .and(query_param("page", "2"))
        .and(query_param("orderFields", "appName,appStatus"))
        .and(query_param("sortDirections", "ASC"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let mut request = get("/public/v1/application");
    request.query = vec![
        ("appName".into(), "a b&c=d".into()),
        ("pageSize".into(), "10".into()),
        ("page".into(), "2".into()),
        ("orderFields".into(), "appName,appStatus".into()),
        ("sortDirections".into(), "ASC".into()),
    ];
    let response = client(&server).execute(request).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!([])));
}

#[tokio::test]
async fn keeps_base_url_path_prefix() {
    let server = MockServer::start().await;
    Mock::given(path("/proxy/public/v1/application"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(2)
        .mount(&server)
        .await;

    for base in [
        format!("{}/proxy", server.uri()),
        format!("{}/proxy/", server.uri()),
    ] {
        let client = ApiClient::new(config(&base)).unwrap();
        let response = client.execute(get("/public/v1/application")).await.unwrap();
        assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    }
}

#[tokio::test]
async fn json_body_is_sent_as_object() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/public/v1/application/com.example.app/version"))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({"appName": "Test", "whatsNew": "s"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"versionId": 1})))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server);
    let mut request = ApiRequest::new(
        HttpMethod::Post,
        "/public/v1/application/com.example.app/version",
    );
    request.body = RequestBody::Json(json!({"appName": "Test", "whatsNew": "s"}));
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(response.status, 201);

    // Регрессия: тело, переданное строкой с JSON, уходит объектом, а не строкой.
    request.body = RequestBody::Json(json!(r#"{"appName": "Test", "whatsNew": "s"}"#));
    client.execute(request).await.unwrap();
}

#[tokio::test]
async fn multipart_sends_text_and_file_parts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/public/v1/application/com.example.app/version/1/apk"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "1"})))
        .expect(2)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("report.txt");
    std::fs::write(&file, "file-content-123").unwrap();

    let client = client(&server);
    let mut request = ApiRequest::new(
        HttpMethod::Post,
        "/public/v1/application/com.example.app/version/1/apk",
    );
    request.query = vec![("filename".into(), "отчёт.log".into())];
    request.body = RequestBody::Multipart(vec![
        MultipartPart::Text {
            name: "servicesType".into(),
            value: "Unknown".into(),
        },
        MultipartPart::File {
            name: "file".into(),
            source: FileSource::Path(file.clone()),
            file_name: Some("отчёт.log".into()),
            content_type: Some("text/x-log".into()),
        },
    ]);
    client.execute(request).await.unwrap();

    // Файл из памяти: тип угадывается по имени.
    let mut request = ApiRequest::new(
        HttpMethod::Post,
        "/public/v1/application/com.example.app/version/1/apk",
    );
    request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Bytes(b"\x89PNG-bytes".to_vec()),
        file_name: Some("pic.png".into()),
        content_type: None,
    }]);
    client.execute(request).await.unwrap();

    let received = server.received_requests().await.unwrap();
    let ct = received[0]
        .headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("multipart/form-data; boundary="), "{ct}");
    let body = String::from_utf8_lossy(&received[0].body);
    assert!(body.contains("name=\"servicesType\""));
    assert!(body.contains("Unknown"));
    assert!(
        body.contains("name=\"file\"; filename=\"отчёт.log\""),
        "{body}"
    );
    assert!(body.contains("Content-Type: text/x-log"));
    assert!(body.contains("file-content-123"));

    let body = String::from_utf8_lossy(&received[1].body);
    assert!(body.contains("filename=\"pic.png\""), "{body}");
    assert!(body.contains("Content-Type: image/png"), "{body}");
    assert!(body.contains("PNG-bytes"));

    // Ошибки до отправки: нет имени для файла из памяти, плохой тип, нет файла.
    for part in [
        MultipartPart::File {
            name: "file".into(),
            source: FileSource::Bytes(vec![1]),
            file_name: None,
            content_type: None,
        },
        MultipartPart::File {
            name: "file".into(),
            source: FileSource::Path(file),
            file_name: None,
            content_type: Some("not a mime".into()),
        },
    ] {
        let mut request = ApiRequest::new(
            HttpMethod::Post,
            "/public/v1/application/com.example.app/version/1/aab",
        );
        request.body = RequestBody::Multipart(vec![part]);
        let err = client.execute(request).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    }
    let mut request = ApiRequest::new(
        HttpMethod::Post,
        "/public/v1/application/com.example.app/version/1/aab",
    );
    request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Path(base.join("missing.bin")),
        file_name: None,
        content_type: None,
    }]);
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::Io(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn returns_pagination_from_body() {
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/application"))
        .and(query_param("page", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok(json!({
            "content": [{"packageName": "a"}, {"packageName": "b"}],
            "pageNumber": 0,
            "pageSize": 2,
            "totalElements": 3,
            "totalPages": 2
        }))))
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/application"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok(json!({
            "content": [{"packageName": "c"}],
            "pageNumber": 1,
            "pageSize": 2,
            "totalElements": 3,
            "totalPages": 2
        }))))
        .mount(&server)
        .await;

    let client = client(&server);
    let mut request = get("/public/v1/application");
    request.query = vec![("page".into(), "0".into()), ("size".into(), "2".into())];
    let first = client.execute(request.clone()).await.unwrap();
    assert_eq!(first.pagination.next_page, Some(1));
    assert_eq!(first.pagination.pass_as, Some(PAGE_PARAM));
    assert_eq!(first.pagination.total_elements, Some(3));
    let ResponseBody::Json(body) = &first.body else {
        panic!("{:?}", first.body);
    };
    assert_eq!(body["content"].as_array().unwrap().len(), 2);
    let serialized = serde_json::to_value(&first.pagination).unwrap();
    assert_eq!(serialized["next_page"], 1);
    assert_eq!(serialized["pass_as"], "page");

    request.query[0].1 = "1".into();
    let last = client.execute(request).await.unwrap();
    assert_eq!(last.pagination.next_page, None);
    let serialized = serde_json::to_value(&last.pagination).unwrap();
    assert!(serialized.get("next_page").is_none(), "{serialized}");
    assert_eq!(serialized["total_pages"], 2);
}

#[tokio::test]
async fn envelope_errors_are_parsed() {
    let server = MockServer::start().await;
    // Ошибка в конверте при HTTP 200.
    Mock::given(path("/public/v1/application/pkg/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(failure("Application not found")))
        .mount(&server)
        .await;
    // Ошибка в конверте при HTTP 400.
    Mock::given(path("/public/v1/application/pkg/version/draft"))
        .respond_with(ResponseTemplate::new(400).set_body_json(failure("Draft already exists")))
        .mount(&server)
        .await;
    // Ошибка шлюза без конверта.
    Mock::given(path("/public/v1/gateway"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "timestamp": "t", "status": 404, "error": "Not Found", "path": "/public/v1/gateway"
        })))
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/huge-error"))
        .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(100_000)))
        .mount(&server)
        .await;

    let client = client(&server);
    let err = client
        .execute(get("/public/v1/application/pkg/version"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(200));
    let text = err.to_string();
    assert!(text.contains("ERROR: Application not found"), "{text}");
    let e = http_error(err);
    assert_eq!(e.code.as_deref(), Some("ERROR"));
    assert_eq!(e.message.as_deref(), Some("Application not found"));
    assert!(e.body.contains("Application not found"), "{}", e.body);

    let request = ApiRequest::new(HttpMethod::Post, "/public/v1/application/pkg/version/draft");
    let e = http_error(client.execute(request).await.unwrap_err());
    assert_eq!(e.status, 400);
    assert_eq!(e.message.as_deref(), Some("Draft already exists"));

    let e = http_error(client.execute(get("/public/v1/gateway")).await.unwrap_err());
    assert_eq!(e.status, 404);
    assert_eq!(e.code, None);
    assert_eq!(e.message.as_deref(), Some("Not Found"));

    let e = http_error(
        client
            .execute(get("/public/v1/huge-error"))
            .await
            .unwrap_err(),
    );
    assert_eq!(e.status, 500);
    assert!(
        e.body.len() < 9000,
        "тело ошибки должно обрезаться: {}",
        e.body.len()
    );
    assert!(e.body.ends_with("(обрезано)"));
}

#[tokio::test]
async fn retries_429_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/application"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/application"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let started = Instant::now();
    let response = client(&server)
        .execute(get("/public/v1/application"))
        .await
        .unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "пауза Retry-After не выдержана: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn retry_limits() {
    let server = MockServer::start().await;
    // Без Retry-After: пауза по умолчанию, после исчерпания попыток — ошибка 429.
    Mock::given(path("/public/v1/always-429"))
        .respond_with(ResponseTemplate::new(429).set_body_json(failure("Too many requests")))
        .expect(3)
        .mount(&server)
        .await;
    // Retry-After больше max_delay: не ждём, сразу возвращаем ошибку с подсказкой.
    Mock::given(path("/public/v1/long-wait"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .expect(1)
        .mount(&server)
        .await;
    // 503 повторяется для GET, но не для POST.
    Mock::given(method("GET"))
        .and(path("/public/v1/unavailable"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/public/v1/unavailable"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    // Прочие ошибки не повторяются.
    Mock::given(path("/public/v1/server-error"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let e = http_error(
        client
            .execute(get("/public/v1/always-429"))
            .await
            .unwrap_err(),
    );
    assert_eq!(e.status, 429);
    assert_eq!(e.message.as_deref(), Some("Too many requests"));
    assert_eq!(e.retry_after, None);

    let err = client
        .execute(get("/public/v1/long-wait"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("3600"), "{err}");
    assert_eq!(http_error(err).retry_after, Some(3600));

    let e = http_error(
        client
            .execute(get("/public/v1/unavailable"))
            .await
            .unwrap_err(),
    );
    assert_eq!(e.status, 503);
    let post = ApiRequest::new(HttpMethod::Post, "/public/v1/unavailable");
    assert_eq!(client.execute(post).await.unwrap_err().status(), Some(503));
    assert_eq!(
        client
            .execute(get("/public/v1/server-error"))
            .await
            .unwrap_err()
            .status(),
        Some(500)
    );
}

#[tokio::test]
async fn retry_resends_multipart_file() {
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/application/com.example.app/version/1/aab"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/application/com.example.app/version/1/aab"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "9"})))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("a.txt");
    std::fs::write(&file, "retry-body-42").unwrap();
    let mut request = ApiRequest::new(
        HttpMethod::Post,
        "/public/v1/application/com.example.app/version/1/aab",
    );
    request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Path(file),
        file_name: None,
        content_type: None,
    }]);
    client(&server).execute(request).await.unwrap();

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    for req in &received {
        assert!(String::from_utf8_lossy(&req.body).contains("retry-body-42"));
    }
}

#[tokio::test]
async fn large_response_is_rejected_with_hint() {
    let server = MockServer::start().await;
    let big = format!("{{\"data\": \"{}\"}}", "a".repeat(10_000));
    Mock::given(path("/public/v1/big"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(big, "application/json"))
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.max_response_bytes = 1000;
    let client = ApiClient::new(cfg).unwrap();

    let err = client.execute(get("/public/v1/big")).await.unwrap_err();
    assert!(
        matches!(err, ApiError::ResponseTooLarge { limit: 1000 }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("size"), "{text}");

    // Лимит отдельного запроса заменяет лимит конфигурации.
    let response = client
        .execute_with_limit(get("/public/v1/big"), 64 * 1024)
        .await
        .unwrap();
    assert!(matches!(response.body, ResponseBody::Json(_)));
}

#[tokio::test]
async fn text_and_untyped_json_responses() {
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/text"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("hello", "text/plain"))
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/untyped"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"{\"a\":1}".to_vec()))
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/empty"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let client = client(&server);
    assert_eq!(
        client.execute(get("/public/v1/text")).await.unwrap().body,
        ResponseBody::Text("hello".into())
    );
    assert_eq!(
        client
            .execute(get("/public/v1/untyped"))
            .await
            .unwrap()
            .body,
        ResponseBody::Json(json!({"a": 1}))
    );
    assert_eq!(
        client.execute(get("/public/v1/empty")).await.unwrap().body,
        ResponseBody::Empty
    );
}

#[tokio::test]
async fn binary_response_without_save_to_returns_note() {
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/blob"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![0u8, 159, 146, 150], "application/octet-stream"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/sniffed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8, 159, 146, 150]))
        .mount(&server)
        .await;

    let client = client(&server);
    for p in ["/public/v1/blob", "/public/v1/sniffed"] {
        match client.execute(get(p)).await.unwrap().body {
            ResponseBody::Text(note) => {
                assert!(note.contains("двоичный"), "{note}");
                assert!(note.contains("save_to"), "{note}");
                assert!(note.contains("4 байт"), "{note}");
            }
            other => panic!("ожидалось пояснение, получено {other:?}"),
        }
    }
}

#[tokio::test]
async fn save_to_writes_file_and_refuses_overwrite() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    Mock::given(path(
        "/public/v1/application/com.example.app/comment/export",
    ))
    .respond_with(ResponseTemplate::new(200).set_body_raw(payload.clone(), "image/png"))
    .mount(&server)
    .await;
    Mock::given(path("/public/v1/fail"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let target = base.join("thumb.png");
    let mut cfg = config(&server.uri());
    // Лимит ответа не распространяется на сохранение в файл.
    cfg.max_response_bytes = 10;
    let client = ApiClient::new(cfg).unwrap();

    let request = ApiRequest {
        save_to: Some(target.clone()),
        ..get("/public/v1/application/com.example.app/comment/export")
    };
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::SavedToFile {
            path: target.clone(),
            bytes: payload.len() as u64,
            content_type: Some("image/png".into()),
        }
    );
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Повторная запись в существующий файл запрещена.
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Каталог должен существовать, путь — быть абсолютным.
    for bad in [
        base.join("no-such-dir").join("x.bin"),
        std::path::PathBuf::from("relative.bin"),
    ] {
        let err = client
            .execute(ApiRequest {
                save_to: Some(bad),
                ..get("/public/v1/application/com.example.app/comment/export")
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    }

    // При ошибке API файл не создаётся.
    let failed = base.join("failed.bin");
    let err = client
        .execute(ApiRequest {
            save_to: Some(failed.clone()),
            ..get("/public/v1/fail")
        })
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert!(!failed.exists());
}

#[tokio::test]
async fn invalid_paths_never_reach_server() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let client = client(&server);
    for p in [
        "https://evil.example/public/v1/application",
        "http://127.0.0.1/public/v1/application",
        "//evil.example/public/v1/application",
        "/public/v1/application/../../admin",
        "/public/v1/application/%2e%2e/admin",
        "/public/v1/application?x=1",
        "/v1/application",
        "/admin",
        "public/v1/application",
        "/public/auth/",
        "/public/auth",
        "/public/%61uth/",
    ] {
        let err = client.execute(get(p)).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{p}: {err:?}");
    }
}

#[tokio::test]
async fn cross_origin_redirect_is_not_followed() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&other)
        .await;
    Mock::given(path("/public/v1/external"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/steal?sig=secret", other.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/internal"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("location", "/public/v1/application"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/application"))
        .and(header("public-token", TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "me"})))
        .mount(&server)
        .await;

    let client = client(&server);
    let e = http_error(
        client
            .execute(get("/public/v1/external"))
            .await
            .unwrap_err(),
    );
    assert_eq!(e.status, 302);
    assert!(!e.body.contains("sig=secret"), "{}", e.body);
    let response = client.execute(get("/public/v1/internal")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"login": "me"})));
}

/// Источник токенов для проверки повтора после 401: выдаёт токены по очереди.
#[derive(Debug)]
struct Rotating {
    tokens: Vec<&'static str>,
    current: std::sync::Mutex<usize>,
    invalidated: std::sync::Mutex<Vec<String>>,
}

impl Rotating {
    fn new(tokens: Vec<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            tokens,
            current: std::sync::Mutex::new(0),
            invalidated: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl rustore_api::TokenSource for Rotating {
    fn token(
        &self,
    ) -> rustore_api::access::BoxFuture<
        '_,
        Result<rustore_api::AccessToken, rustore_api::access::TokenError>,
    > {
        let index = *self.current.lock().unwrap();
        let token = rustore_api::AccessToken::new(self.tokens[index.min(self.tokens.len() - 1)]);
        Box::pin(async move { Ok(token) })
    }

    fn invalidate<'a>(
        &'a self,
        rejected: &'a rustore_api::AccessToken,
    ) -> rustore_api::access::BoxFuture<'a, ()> {
        self.invalidated
            .lock()
            .unwrap()
            .push(rejected.expose().to_owned());
        *self.current.lock().unwrap() += 1;
        Box::pin(async {})
    }

    fn known_tokens(&self) -> Vec<rustore_api::AccessToken> {
        self.tokens
            .iter()
            .map(|t| rustore_api::AccessToken::new(*t))
            .collect()
    }
}

#[tokio::test]
async fn retries_401_once_with_a_fresh_token() {
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/application"))
        .and(header("public-token", "token-old"))
        .respond_with(ResponseTemplate::new(401).set_body_json(failure("Token expired")))
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/application"))
        .and(header("public-token", "token-new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok(json!({"ok": true}))))
        .expect(2)
        .mount(&server)
        .await;
    // POST с редиректом, после которого пришёл 401: исходный запрос мог выполниться.
    Mock::given(method("POST"))
        .and(path("/public/v1/redirected"))
        .respond_with(
            ResponseTemplate::new(303).insert_header("location", "/public/v1/after-redirect"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/after-redirect"))
        .respond_with(ResponseTemplate::new(401).set_body_json(failure("Unauthorized")))
        .mount(&server)
        .await;

    let tokens = Rotating::new(vec!["token-old", "token-new"]);
    let mut cfg = config(&server.uri());
    cfg.tokens = Some(tokens.clone());
    let client = ApiClient::new(cfg).unwrap();
    let response = client.execute(get("/public/v1/application")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    assert_eq!(*tokens.invalidated.lock().unwrap(), vec!["token-old"]);

    // POST без редиректа тоже повторяется: 401 приходит до выполнения операции.
    let tokens = Rotating::new(vec!["token-old", "token-new"]);
    let mut cfg = config(&server.uri());
    cfg.tokens = Some(tokens.clone());
    let client = ApiClient::new(cfg).unwrap();
    client
        .execute(ApiRequest::new(HttpMethod::Post, "/public/v1/application"))
        .await
        .unwrap();
    assert_eq!(*tokens.invalidated.lock().unwrap(), vec!["token-old"]);

    let tokens = Rotating::new(vec!["token-old", "token-new", "token-third"]);
    let mut cfg = config(&server.uri());
    cfg.tokens = Some(tokens.clone());
    let client = ApiClient::new(cfg).unwrap();
    let err = client
        .execute(ApiRequest::new(HttpMethod::Post, "/public/v1/redirected"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(401));
    // После редиректа POST не повторяется: токен не сброшен.
    assert!(tokens.invalidated.lock().unwrap().is_empty());

    // Повтор только один: новый токен тоже отвергнут — ошибка 401.
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/application"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&server)
        .await;
    let tokens = Rotating::new(vec!["token-a", "token-b", "token-c"]);
    let mut cfg = config(&server.uri());
    cfg.tokens = Some(tokens.clone());
    let err = ApiClient::new(cfg)
        .unwrap()
        .execute(get("/public/v1/application"))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert_eq!(tokens.invalidated.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn token_never_leaks() {
    // Конфигурация и клиент в Debug не показывают токен.
    let cfg = config("https://api.example");
    let debug = format!("{cfg:?}");
    assert!(!debug.contains(TOKEN), "{debug}");
    let client = ApiClient::new(cfg).unwrap();
    let debug = format!("{client:?}");
    assert!(!debug.contains(TOKEN), "{debug}");
    assert_eq!(client.scrub_token(format!("x {TOKEN} y")), "x *** y");

    // Сервер отражает заголовок Public-Token в теле ошибки (в том числе в message) и в
    // успешном ответе.
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/echo-error"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("public-token").unwrap().to_str().unwrap();
            ResponseTemplate::new(403).set_body_json(failure(&format!("bad token: {auth}")))
        })
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/echo-text"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("public-token").unwrap().to_str().unwrap();
            ResponseTemplate::new(200).set_body_raw(format!("you sent {auth}"), "text/plain")
        })
        .mount(&server)
        .await;

    let client = ApiClient::new(config(&server.uri())).unwrap();
    let mut outputs = Vec::new();

    let err = client
        .execute(get("/public/v1/echo-error"))
        .await
        .unwrap_err();
    outputs.push(format!("{err}"));
    outputs.push(format!("{err:?}"));
    let ok = client.execute(get("/public/v1/echo-text")).await.unwrap();
    outputs.push(format!("{ok:?}"));

    // Ошибки пути, параметров, ввода-вывода.
    let mut file_request = ApiRequest::new(
        HttpMethod::Post,
        "/public/v1/application/com.example.app/version/1/aab",
    );
    file_request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Path("/nonexistent/file".into()),
        file_name: None,
        content_type: None,
    }]);
    for e in [
        client.execute(get("/public/v1/../x")).await.unwrap_err(),
        client.execute(file_request).await.unwrap_err(),
    ] {
        outputs.push(format!("{e}"));
        outputs.push(format!("{e:?}"));
    }

    // Порт, на котором никто не слушает: соединение отклоняется.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let unreachable = ApiClient::new(config(&format!("http://127.0.0.1:{closed}"))).unwrap();
    let transport = unreachable
        .execute(get("/public/v1/application"))
        .await
        .unwrap_err();
    assert!(matches!(transport, ApiError::Transport(_)), "{transport:?}");
    outputs.push(format!("{transport}"));
    outputs.push(format!("{transport:?}"));

    for out in &outputs {
        assert!(!out.contains(TOKEN), "токен утёк: {out}");
    }
    assert!(outputs[0].contains("***"), "{}", outputs[0]);
}

#[tokio::test]
async fn timeout_maps_to_transport_error() {
    let server = MockServer::start().await;
    Mock::given(path("/public/v1/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.timeout = Duration::from_millis(200);
    let client = ApiClient::new(cfg).unwrap();
    match client.execute(get("/public/v1/slow")).await.unwrap_err() {
        ApiError::Transport(text) => assert!(text.contains("таймаут"), "{text}"),
        other => panic!("ожидалась Transport, получено {other:?}"),
    }
}

#[test]
fn default_config_targets_rustore() {
    let cfg = ClientConfig::new(None).unwrap();
    assert_eq!(cfg.base_url.as_str(), "https://public-api-m.rustore.ru/");
    assert!(ApiClient::new(cfg).is_ok());
}

#[test]
fn rejects_insecure_or_invalid_config() {
    for base in [
        "http://public-api-m.rustore.ru",
        "https://user:pw@api.example",
        "ftp://api.example",
    ] {
        let err = ApiClient::new(config(base)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{base}: {err:?}");
    }
}

/// Большой файл уходит потоком с диска с длиной части и действует таймаут передачи, а не
/// обычный таймаут запроса.
#[tokio::test]
async fn large_file_upload_is_streamed_from_disk() {
    const SIZE: usize = 24 * 1024 * 1024;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/public/v1/application/pkg/version/1/apk"))
        .and(header("public-token", TOKEN))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok(json!(null)))
                // Ответ дольше обычного таймаута: для загрузки файла действует transfer_timeout.
                .set_delay(Duration::from_millis(400)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("app-release.apk");
    {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
        let chunk: Vec<u8> = (0..=255u8).cycle().take(1024 * 1024).collect();
        for _ in 0..SIZE / chunk.len() {
            out.write_all(&chunk).unwrap();
        }
    }

    let mut cfg = config(&server.uri());
    cfg.timeout = Duration::from_millis(200);
    cfg.transfer_timeout = Duration::from_secs(60);
    let client = ApiClient::new(cfg).unwrap();
    let mut request = ApiRequest::new(HttpMethod::Post, "/public/v1/application/pkg/version/1/apk");
    request.query = vec![("isMainApk".into(), "true".into())];
    request.body = RequestBody::Multipart(vec![MultipartPart::File {
        name: "file".into(),
        source: FileSource::Path(file),
        file_name: None,
        content_type: Some("application/vnd.android.package-archive".into()),
    }]);
    let response = client.execute(request).await.unwrap();
    assert_eq!(response.status, 200);

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let body = &received[0].body;
    assert!(body.len() > SIZE, "{}", body.len());
    let text = String::from_utf8_lossy(&body[..512]);
    assert!(text.contains("filename=\"app-release.apk\""), "{text}");
    assert!(
        text.contains("Content-Type: application/vnd.android.package-archive"),
        "{text}"
    );
}
