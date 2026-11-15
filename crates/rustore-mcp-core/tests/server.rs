//! Сквозная проверка: сервер в процессе, клиент `rmcp` через in-memory транспорт,
//! RuStore API подменён `wiremock`. Реальный API, keychain и сеть не используются.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use rustore_api::executor::{ApiClient, ClientConfig, RetryPolicy};
use rustore_api::{AccessLevel, StaticToken};
use rustore_mcp_core::composite::{CompositeTool, common};
use rustore_mcp_core::{FilePolicy, RuStoreServer, ServerOptions};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, header_regex, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TEST_TOKEN: &str = "test-token-value";
const APP: &str = "/public/v1/application/com.example.app";

fn client(api: &MockServer) -> ApiClient {
    let mut config = ClientConfig::new(Some(Arc::new(StaticToken::new(TEST_TOKEN)))).unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.max_response_bytes = 64 * 1024;
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    ApiClient::new(config).unwrap()
}

async fn start(
    api: &MockServer,
    options: ServerOptions,
    files: FilePolicy,
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    let server = RuStoreServer::new(client(api), options, files).unwrap();
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let handle = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();
    (client, handle)
}

fn options(max_access: AccessLevel) -> ServerOptions {
    ServerOptions {
        max_access,
        toolsets: Some(vec!["all".into()]),
        ..ServerOptions::default()
    }
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

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    value: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args(value)))
        .await
        .unwrap()
}

#[tokio::test]
async fn read_only_server_end_to_end() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{APP}/comment")))
        .and(query_param("size", "1"))
        .and(header("public-token", TEST_TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": [{"commentId": 1}]
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application/com.missing.app/comment"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errors": {},
            "errorMessages": ["Приложение не найдено."],
            "statusCode": 404
        })))
        .mount(&api)
        .await;

    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;

    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "rustore-mcp");
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("read-only"), "{instructions}");

    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "list_reviews"));
    assert!(tools.iter().all(|t| {
        t.annotations
            .as_ref()
            .is_some_and(|a| a.read_only_hint == Some(true))
    }));
    assert!(!tools.iter().any(|t| t.name == "create_draft_version"));
    assert!(!tools.iter().any(|t| t.name == "delete_review_answer"));

    // Успешный вызов: путь, query и `Public-Token` проверяет mock.
    let result = call(
        &client,
        "list_reviews",
        json!({"packageName": "com.example.app", "size": 1}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    // Конверт RuStore снят: агент получает `body`.
    assert_eq!(text(&result), r#"[{"commentId":1}]"#);

    // 404 RuStore API — управляемая ошибка, не ошибка протокола.
    let result = call(
        &client,
        "list_reviews",
        json!({"packageName": "com.missing.app"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 404"), "{message}");
    assert!(message.contains("Приложение не найдено."), "{message}");
    assert!(!message.contains(TEST_TOKEN));

    // Инструмент записи не зарегистрирован, вызов объясняет, какой флаг нужен.
    let result = call(
        &client,
        "create_draft_version",
        json!({"packageName": "com.example.app", "body": {"appName": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"), "{}", text(&result));

    // Ошибка аргументов не доходит до API.
    let result = call(
        &client,
        "list_reviews",
        json!({"package": "com.example.app"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("unknown argument(s) `package`"),
        "{}",
        text(&result)
    );
    let result = call(&client, "list_reviews", json!({})).await;
    assert!(text(&result).contains("missing required argument(s): `packageName`"));

    // Неизвестный инструмент — ошибка протокола, сервер продолжает работать.
    assert!(
        client
            .call_tool(CallToolRequestParams::new("no_such_tool"))
            .await
            .is_err()
    );
    assert!(!client.list_all_tools().await.unwrap().is_empty());

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn pagination_is_returned() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{APP}/comment")))
        .and(query_param("size", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": {
                "content": [{"commentId": 1}, {"commentId": 2}],
                "pageNumber": 0,
                "pageSize": 2,
                "totalPages": 3,
                "totalElements": 5
            }
        })))
        .expect(1)
        .mount(&api)
        .await;

    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(
        &client,
        "list_reviews",
        json!({"packageName": "com.example.app", "size": 2}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["result"]["content"][1]["commentId"], 2);
    assert_eq!(
        value["pagination"],
        json!({
            "page": 0,
            "page_size": 2,
            "total_pages": 3,
            "total_elements": 5,
            "next_page": 1,
            "pass_as": "page"
        })
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn credentials_are_redacted() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{APP}/developer")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": 1,
            "actions": [{"type": "Webhook", "authContext": {"type": "basic", "password": "p@ss"}}],
            "echo": TEST_TOKEN
        }])))
        .mount(&api)
        .await;
    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(
        &client,
        "list_app_access",
        json!({"packageName": "com.example.app"}),
    )
    .await;
    let body = text(&result);
    assert_eq!(result.is_error, Some(false), "{body}");
    assert!(!body.contains("p@ss"), "{body}");
    assert!(!body.contains(TEST_TOKEN), "{body}");
    assert!(
        body.contains("[redacted]") && body.contains("password"),
        "{body}"
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

/// Путь `get_version` для версии 5 приложения `com.example.app`.
const VERSION_5: &str = "/public/v2/application/com.example.app/version/5";

/// Значение, похожее на JWE-токен RuStore API по форме (не настоящее): пять частей base64url.
fn looks_like_token() -> String {
    [
        "eyJhbGciOiJSU0EtT0FFUC0yNTYiLCJlbmMiOiJBMjU2R0NNIn0",
        "OKOawDo13gRp2ojaHV7LFpZcgV7T6DVZKTyKOMTYUmKoTCVJRgckCL9kiMT03JGe",
        "48V1_ALb6US04U3b",
        "5eym8TW_c8SuK0ltJ3rpYIzOeDQz7TALvtu6UG9oMo4vpzs9tX_EFShS8iB7j6ji",
        "XFBoMYUZodetZdvTiFvSkQ",
    ]
    .join(".")
}

#[tokio::test]
async fn secrets_are_redacted_in_responses_and_errors() {
    let token = looks_like_token();
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(VERSION_5))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": [{
                "jwe": "jwe-value-1",
                "privateKey": "key-value-1",
                "purchaseToken": "purchase-token-visible",
                "contentType": "application/json"
            }]
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{APP}/comment/statistic")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": {"note": format!("bot uses Public-Token: {token}")}
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{APP}/comment")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "BAD_REQUEST",
            "message": format!("bad filter near {token}"),
            "password": "pw-in-error"
        })))
        .mount(&api)
        .await;

    let (client, server) = start(
        &api,
        options(AccessLevel::Read),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;

    let result = call(
        &client,
        "get_version",
        json!({"packageName": "com.example.app", "versionId": 5}),
    )
    .await;
    let body = text(&result);
    assert_eq!(result.is_error, Some(false), "{body}");
    assert!(
        !body.contains("jwe-value-1") && !body.contains("key-value-1"),
        "{body}"
    );
    // Токены покупок — идентификаторы, а не учётные данные: агенту они нужны.
    assert!(body.contains("purchase-token-visible"), "{body}");
    assert!(body.contains("application/json"), "{body}");
    assert!(body.contains("jwe, privateKey"), "{body}");

    // Тот же ответ через `api_request` скрывается так же.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": VERSION_5}),
    )
    .await;
    assert!(!text(&result).contains("jwe-value-1"), "{}", text(&result));

    let result = call(
        &client,
        "get_app_rating",
        json!({"packageName": "com.example.app"}),
    )
    .await;
    let body = text(&result);
    assert!(!body.contains(&token), "{body}");
    assert!(body.contains("bot uses Public-Token: [redacted]"), "{body}");
    assert!(body.contains("1 value(s) that look like"), "{body}");

    // Ошибка API: значения скрыты и в сообщении, и в теле, с пометкой.
    let result = call(
        &client,
        "list_reviews",
        json!({"packageName": "com.example.app"}),
    )
    .await;
    let body = text(&result);
    assert_eq!(result.is_error, Some(true));
    assert!(body.contains("HTTP 400"), "{body}");
    assert!(
        !body.contains("pw-in-error") && !body.contains(&token),
        "{body}"
    );
    assert!(body.contains("replaced with \"[redacted]\""), "{body}");

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn files_outside_allowed_directories_are_never_used() {
    let api = MockServer::start().await;
    // Ни загрузка, ни скачивание до API не доходят.
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"content".to_vec()))
        .expect(0)
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
    let files = FilePolicy::new(std::slice::from_ref(&root))
        .unwrap()
        .with_base_dir(&root);
    let (client, server) = start(&api, options(AccessLevel::Destructive), files).await;

    // Загрузка: файл вне корня, ссылка на него и путь через ссылку на каталог.
    for file in [
        outside.join("secret.txt").display().to_string(),
        root.join("link.txt").display().to_string(),
        "out/secret.txt".to_owned(),
        "../outside/secret.txt".to_owned(),
    ] {
        let result = call(
            &client,
            "upload_apk",
            json!({"packageName": "com.example.app", "versionId": 7, "isMainApk": true, "file": file}),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{file}");
        assert!(
            text(&result).contains("outside the allowed directories"),
            "{file}: {}",
            text(&result)
        );
    }

    // Скачивание: каталог назначения через ссылку наружу и существующая ссылка.
    for target in [
        root.join("out").join("a.csv").display().to_string(),
        outside.join("a.csv").display().to_string(),
        "out/a.csv".to_owned(),
    ] {
        for (tool, args) in [
            (
                "export_reviews_csv",
                json!({"packageName": "com.example.app", "from": "2024-01-01", "to": "2024-01-31", "save_to": target}),
            ),
            (
                "api_request",
                json!({"method": "GET", "path": format!("{APP}/comment/export"), "save_to": target}),
            ),
        ] {
            let result = call(&client, tool, args).await;
            assert_eq!(result.is_error, Some(true), "{tool} {target}");
            assert!(
                text(&result).contains("outside the allowed directories"),
                "{tool} {target}: {}",
                text(&result)
            );
        }
    }
    let result = call(
        &client,
        "export_reviews_csv",
        json!({"packageName": "com.example.app", "from": "2024-01-01", "to": "2024-01-31", "save_to": "link.txt"}),
    )
    .await;
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );
    assert!(!outside.join("a.csv").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "secret"
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn binary_download_is_saved_to_file() {
    let api = MockServer::start().await;
    let content: Vec<u8> = b"commentId,text\n1,Bug\n".to_vec();
    Mock::given(method("GET"))
        .and(path(format!("{APP}/comment/export")))
        .and(query_param("from", "2024-01-01"))
        .and(query_param("to", "2024-01-31"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/csv")
                .set_body_bytes(content.clone()),
        )
        .expect(1)
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
    let (client, server) = start(&api, options(AccessLevel::Read), files).await;

    let tools = client.list_all_tools().await.unwrap();
    let download = tools
        .iter()
        .find(|t| t.name == "export_reviews_csv")
        .unwrap();
    let required = download.input_schema["required"].as_array().unwrap();
    assert!(required.contains(&json!("save_to")));

    let target = root.join("reviews.csv");
    let result = call(
        &client,
        "export_reviews_csv",
        json!({
            "packageName": "com.example.app", "from": "2024-01-01", "to": "2024-01-31",
            "save_to": target.display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["saved_to"], target.display().to_string());
    assert_eq!(value["bytes"], content.len());
    assert_eq!(value["content_type"], "text/csv");
    assert_eq!(std::fs::read(&target).unwrap(), content);

    // Повтор с тем же путём: файл не перезаписывается, запрос не отправляется (expect(1)).
    let result = call(
        &client,
        "export_reviews_csv",
        json!({
            "packageName": "com.example.app", "from": "2024-01-01", "to": "2024-01-31",
            "save_to": target.display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn multipart_upload_from_allowed_file() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{APP}/version/7/apk")))
        .and(query_param("isMainApk", "true"))
        .and(header_regex(
            "content-type",
            "^multipart/form-data; boundary=",
        ))
        .and(|request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains("name=\"file\"")
                && body.contains("filename=\"app.apk\"")
                && body.contains("apk content")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": "OK"})))
        .expect(1)
        .mount(&api)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = root.join("app.apk");
    std::fs::write(&file, "apk content").unwrap();
    let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
    let (client, server) = start(&api, options(AccessLevel::Write), files).await;

    let result = call(
        &client,
        "upload_apk",
        json!({
            "packageName": "com.example.app", "versionId": 7, "isMainApk": true,
            "file": file.display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"code":"OK"}"#);

    // Файл вне разрешённых каталогов не читается.
    let other = tempfile::tempdir().unwrap();
    let outside = other.path().join("secret.txt");
    std::fs::write(&outside, "secret").unwrap();
    let result = call(
        &client,
        "upload_apk",
        json!({
            "packageName": "com.example.app", "versionId": 7, "isMainApk": true,
            "file": outside.display().to_string()
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("outside the allowed directories"),
        "{}",
        text(&result)
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn access_levels_change_the_tool_list() {
    let api = MockServer::start().await;
    let files = || FilePolicy::new(&[]).unwrap();

    let (write, server) = start(&api, options(AccessLevel::Write), files()).await;
    let tools = write.list_all_tools().await.unwrap();
    let create = tools
        .iter()
        .find(|t| t.name == "create_draft_version")
        .unwrap();
    let annotations = create.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert!(!tools.iter().any(|t| t.name == "delete_review_answer"));
    write.cancel().await.unwrap();
    server.await.unwrap();

    let (all, server) = start(&api, options(AccessLevel::Destructive), files()).await;
    let tools = all.list_all_tools().await.unwrap();
    let delete = tools
        .iter()
        .find(|t| t.name == "delete_review_answer")
        .unwrap();
    assert_eq!(
        delete.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    // Все операции, все составные инструменты, `api_request` и `api_search`.
    assert_eq!(
        tools.len(),
        rustore_api::spec::operations().len() + rustore_mcp_core::composite::all().len() + 2
    );
    for name in ["search_apps", "release_summary"] {
        assert!(tools.iter().any(|t| t.name == name), "{name}");
    }
    let request = tools.iter().find(|t| t.name == "api_request").unwrap();
    assert_eq!(
        request.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    all.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn api_request_respects_the_access_level() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{APP}/comment")))
        .and(query_param("size", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": {"content": [{"commentId": 1}], "totalElements": 7}
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v2/application/com.example.app/version/7"))
        .and(query_param("expand", "a"))
        .and(query_param("expand", "b"))
        .and(header("public-token", TEST_TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 1}])))
        .expect(1)
        .mount(&api)
        .await;
    // Изменяющие запросы до API не доходят.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&api)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&api)
        .await;

    // Набор `catalog` не мешает `api_request` и `api_search`.
    let (client, server) = start(
        &api,
        ServerOptions {
            toolsets: Some(vec!["catalog".into()]),
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let tools = client.list_all_tools().await.unwrap();
    let request = tools.iter().find(|t| t.name == "api_request").unwrap();
    assert_eq!(
        request.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tools.iter().any(|t| t.name == "api_search"));

    // Чтение операции из выключенного набора.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": format!("{APP}/comment"), "query": {"size": 1}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["result"]["content"][0]["commentId"], 1);
    assert_eq!(value["pagination"]["total_elements"], 7);

    // Путь v2 и повтор ключа в query.
    let result = call(
        &client,
        "api_request",
        json!({
            "method": "GET",
            "path": "/public/v2/application/com.example.app/version/7",
            "query": {"expand": ["a", "b"]}
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));

    // Запись и удаление: понятный отказ с нужным флагом.
    let result = call(
        &client,
        "api_request",
        json!({"method": "POST", "path": format!("{APP}/version"), "body": {"appName": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("needs write access"), "{message}");
    assert!(message.contains("--allow-write"), "{message}");
    // Удаление черновика — уровень записи операции, а не уровень метода DELETE.
    let result = call(
        &client,
        "api_request",
        json!({"method": "DELETE", "path": format!("{APP}/version/7")}),
    )
    .await;
    assert!(
        text(&result).contains("needs write access"),
        "{}",
        text(&result)
    );
    for path in [
        "/public/v1/application/com.example.app/feedback/1",
        "/public/v1/some/unknown/thing",
    ] {
        let result = call(
            &client,
            "api_request",
            json!({"method": "DELETE", "path": path}),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let message = text(&result);
        assert!(message.contains("needs destructive access"), "{message}");
        assert!(message.contains("--allow-destructive"), "{message}");
    }
    // Заголовки и полные URL не принимаются.
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "/public/v1/application", "headers": {"Authorization": "OAuth x"}}),
    )
    .await;
    assert!(text(&result).contains("headers cannot be set"));
    let result = call(
        &client,
        "api_request",
        json!({"method": "GET", "path": "https://example.test/public/v1/application"}),
    )
    .await;
    assert!(text(&result).contains("full URLs"));

    // `api_search` находит выключенные операции и называет флаги.
    let result = call(&client, "api_search", json!({"query": "черновик"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    let names: Vec<&str> = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["tool_name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"create_draft_version") && names.contains(&"update_draft_version"),
        "{names:?}"
    );
    let post = value["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool_name"] == "create_draft_version")
        .unwrap();
    assert_eq!(post["enable_with"], "--allow-write --toolsets releases");

    let result = call(&client, "api_search", json!({"query": "rating"})).await;
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    let first = &value["results"][0];
    assert!(
        first["tool_name"].as_str().unwrap().contains("rating"),
        "{first}"
    );
    assert!(
        first["reference"].as_str().unwrap().starts_with("https://"),
        "{first}"
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn api_request_writes_with_flags() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{APP}/version")))
        .and(body_json(json!({"appName": "x"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": "OK", "body": 9})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("{APP}/feedback/9")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&api)
        .await;

    let (write, server) = start(
        &api,
        options(AccessLevel::Write),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(
        &write,
        "api_request",
        json!({"method": "POST", "path": format!("{APP}/version"), "body": {"appName": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), "9");
    let result = call(
        &write,
        "api_request",
        json!({"method": "DELETE", "path": format!("{APP}/feedback/9")}),
    )
    .await;
    assert!(text(&result).contains("--allow-destructive"));
    write.cancel().await.unwrap();
    server.await.unwrap();

    let (all, server) = start(
        &api,
        options(AccessLevel::Destructive),
        FilePolicy::new(&[]).unwrap(),
    )
    .await;
    let result = call(
        &all,
        "api_request",
        json!({"method": "DELETE", "path": format!("{APP}/feedback/9")}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"status":204}"#);
    all.cancel().await.unwrap();
    server.await.unwrap();
}

/// Составной инструмент для теста: рейтинг приложения через `get_app_rating` и поле с
/// токеном, которое сервер должен вырезать.
fn rating_tool() -> CompositeTool {
    CompositeTool {
        name: "fake_rating_summary",
        toolset: "reviews",
        access: AccessLevel::Read,
        title: "Fake rating summary",
        description: "Rating of one app.",
        input_schema: || {
            common::object_schema(json!({"packageName": common::package_name_property()}), &[])
        },
        uses_local_files: false,
        run: |ctx, args| {
            Box::pin(async move {
                let package = common::package_name(ctx, &args)?;
                let rating = ctx
                    .call("get_app_rating", json!({"packageName": package}))
                    .await?;
                Ok(json!({
                    "packageName": package,
                    "rating": rating,
                    "access_token": "fake-secret-value"
                }))
            })
        },
    }
}

#[tokio::test]
async fn composite_tools_end_to_end() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{APP}/comment/statistic")))
        .and(header("public-token", TEST_TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK",
            "body": {"averageUserRating": 4.5, "totalRatings": 10}
        })))
        .mount(&api)
        .await;
    let mut write_tool = rating_tool();
    write_tool.name = "fake_write_summary";
    write_tool.access = AccessLevel::Write;
    let options = ServerOptions {
        composite_tools: vec![rating_tool(), write_tool],
        packages: vec!["com.example.app".into()],
        ..options(AccessLevel::Read)
    };
    let (client, server) = start(&api, options, FilePolicy::new(&[]).unwrap()).await;

    let tools = client.list_all_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == "fake_rating_summary")
        .unwrap();
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    assert!(tool.input_schema["properties"].get("jq").is_some());
    assert!(!tools.iter().any(|t| t.name == "fake_write_summary"));

    // Пакет по умолчанию из профиля, учётные данные вырезаны.
    let result = call(&client, "fake_rating_summary", json!({})).await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let body = text(&result);
    assert!(body.contains("4.5"), "{body}");
    assert!(body.contains("com.example.app"), "{body}");
    assert!(!body.contains("fake-secret-value"), "{body}");

    // Фильтр `jq` применяется к результату.
    let result = call(
        &client,
        "fake_rating_summary",
        json!({"jq": ".rating.totalRatings"}),
    )
    .await;
    let filtered = text(&result);
    assert!(filtered.starts_with("10\n"), "{filtered}");
    // Пометка о вырезанных полях остаётся и после фильтра.
    assert!(filtered.contains("access_token"), "{filtered}");

    // Неизвестный аргумент отклоняется до запросов.
    let result = call(&client, "fake_rating_summary", json!({"size": 1})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("size"), "{}", text(&result));

    // Инструмент выше уровня сервера объясняет, какой флаг нужен.
    let result = call(&client, "fake_write_summary", json!({})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"), "{}", text(&result));

    client.cancel().await.unwrap();
    server.abort();
}
