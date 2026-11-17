//! Проверка прав ключа на поддельном RuStore API (`wiremock`): ключ со всеми правами, ключ
//! без платежей и ключ на одно приложение из четырёх.

use rustore_api::spec::AccessLevel;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::composite::testing;
use crate::files::FilePolicy;

fn envelope(body: Value) -> Value {
    json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(envelope(body))
}

fn forbidden() -> ResponseTemplate {
    ResponseTemplate::new(403).set_body_json(json!({
        "code": "FORBIDDEN",
        "message": "Access denied",
        "body": null
    }))
}

/// Приложения компании: `com.example.app<N>` с `appId` 100 + N.
fn apps(count: usize) -> Value {
    let content: Vec<Value> = (1..=count)
        .map(|n| {
            json!({
                "packageName": format!("com.example.app{n}"),
                "appName": format!("App {n}"),
                "appId": 100 + n,
                "appStatus": "ACTIVE"
            })
        })
        .collect();
    json!({"content": content})
}

/// Все разделы одного приложения отвечают успехом (подписка — 404 несуществующей покупки).
async fn mount_app(server: &MockServer, n: usize) {
    let package = format!("com.example.app{n}");
    let app_id = 100 + n;
    let routes: Vec<(String, ResponseTemplate)> = vec![
        (
            format!("/public/v1/application/{package}/version"),
            ok(json!({"content": [{"versionId": 7000 + n}], "totalPages": 1})),
        ),
        (
            format!(
                "/public/v2/application/{package}/version/{}/image/screenshot",
                7000 + n
            ),
            ok(json!([])),
        ),
        (
            format!("/public/v1/application/{package}/comment/statistic"),
            ok(json!({"averageUserRating": 4.5})),
        ),
        (
            format!("/public/applications/{app_id}/invoices"),
            ok(json!({"content": [], "totalPages": 0})),
        ),
        (
            format!(
                "/public/v5/subscription/{package}/{PROBE_SUBSCRIPTION_ID}/{PROBE_PURCHASE_ID}"
            ),
            ResponseTemplate::new(404).set_body_json(json!({
                "code": "NOT_FOUND",
                "message": "Subscription not found"
            })),
        ),
        (
            format!("/public/applications/{app_id}/catalog/products"),
            ok(json!({"elements": []})),
        ),
        (
            format!("/public/v1/application/{package}/developer"),
            ok(json!({"content": [{"devVkId": "1", "firstName": "Private", "role": "OWNER"}]})),
        ),
    ];
    for (route, response) in routes {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(response)
            .mount(server)
            .await;
    }
}

async fn mount_apps(server: &MockServer, count: usize) {
    Mock::given(method("GET"))
        .and(path("/public/v1/application"))
        .respond_with(ok(apps(count)))
        .mount(server)
        .await;
}

/// Всё, что не подключено отдельно, — отказ по правам (подключается последним).
async fn deny_the_rest(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex("^/public/.*"))
        .respond_with(forbidden())
        .mount(server)
        .await;
}

fn context(server: &MockServer, packages: &[&str]) -> Context {
    testing::context(
        &server.uri(),
        AccessLevel::Read,
        packages,
        FilePolicy::disabled(),
    )
    .unwrap()
}

fn status(report: &AccessReport, toolset: &str) -> SectionStatus {
    report.section(toolset).unwrap().status
}

fn app_statuses(report: &AccessReport) -> Vec<(&str, AppStatus)> {
    report
        .apps
        .iter()
        .map(|app| (app.package_name.as_str(), app.status))
        .collect()
}

#[tokio::test]
async fn key_with_all_permissions() {
    let server = MockServer::start().await;
    mount_apps(&server, 2).await;
    mount_app(&server, 1).await;
    mount_app(&server, 2).await;
    deny_the_rest(&server).await;

    let report = check_access(&context(&server, &[])).await.unwrap();
    for section in &report.sections {
        assert_eq!(section.status, SectionStatus::Available, "{section:?}");
    }
    assert_eq!(report.sections.len(), 8);
    assert_eq!(
        report.section("listing").unwrap().app.as_deref(),
        Some("com.example.app1")
    );
    assert_eq!(
        app_statuses(&report),
        [
            ("com.example.app1", AppStatus::Accessible),
            ("com.example.app2", AppStatus::Accessible)
        ]
    );
    assert_eq!(report.apps[0].app_id.as_deref(), Some("101"));
    assert_eq!(report.apps_source, "list_apps");
    assert!(report.apps_complete);
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    // Второе приложение подтверждено одним запросом, разделы на нём не повторяются.
    let requests = server.received_requests().await.unwrap();
    let second: Vec<String> = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .filter(|path| path.contains("app2") || path.contains("/102/"))
        .collect();
    assert_eq!(second.len(), 1, "{second:?}");
    // Персональные данные из ответов в отчёт не попадают.
    let text = serde_json::to_string(&report).unwrap();
    assert!(!text.contains("Private"), "{text}");
}

#[tokio::test]
async fn key_without_payments() {
    let server = MockServer::start().await;
    mount_apps(&server, 2).await;
    // Платежи отклонены раньше остальных маршрутов приложения.
    Mock::given(method("GET"))
        .and(path_regex("^/public/applications/[0-9]+/invoices$"))
        .respond_with(forbidden())
        .mount(&server)
        .await;
    mount_app(&server, 1).await;
    mount_app(&server, 2).await;
    deny_the_rest(&server).await;

    let report = check_access(&context(&server, &[])).await.unwrap();
    assert_eq!(status(&report, "purchases"), SectionStatus::Denied);
    let purchases = report.section("purchases").unwrap();
    let detail = purchases.detail.as_deref().unwrap();
    assert!(detail.contains("is not ticked for the key"), "{detail}");
    assert!(detail.contains(purchases.method_title), "{detail}");
    assert!(detail.contains("HTTP 403"), "{detail}");
    for toolset in [
        "apps",
        "releases",
        "listing",
        "reviews",
        "subscriptions",
        "catalog",
        "access",
    ] {
        assert_eq!(
            status(&report, toolset),
            SectionStatus::Available,
            "{toolset}"
        );
    }
    // Отказ метода не делает приложения недоступными, и на втором приложении платежи не
    // проверяются повторно.
    assert!(
        report
            .apps
            .iter()
            .all(|app| app.status == AppStatus::Accessible)
    );
    let invoices = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path().ends_with("/invoices"))
        .count();
    assert_eq!(invoices, 1);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("Denied sections")),
        "{:?}",
        report.notes
    );
}

#[tokio::test]
async fn key_for_one_app_of_four() {
    let server = MockServer::start().await;
    mount_apps(&server, 4).await;
    // Доступно только последнее приложение: первые три проверяются раньше, чем хоть один
    // раздел ответил.
    mount_app(&server, 4).await;
    deny_the_rest(&server).await;

    let report = check_access(&context(&server, &[])).await.unwrap();
    for section in &report.sections {
        assert_eq!(section.status, SectionStatus::Available, "{section:?}");
    }
    assert_eq!(
        app_statuses(&report),
        [
            ("com.example.app1", AppStatus::Denied),
            ("com.example.app2", AppStatus::Denied),
            ("com.example.app3", AppStatus::Denied),
            ("com.example.app4", AppStatus::Accessible)
        ]
    );
    let accessible: Vec<&str> = report
        .accessible_apps()
        .map(|app| app.package_name.as_str())
        .collect();
    assert_eq!(accessible, ["com.example.app4"]);
    assert!(
        report.apps[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("denied for this one")
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("Some apps are denied")),
        "{:?}",
        report.notes
    );
}

#[tokio::test]
async fn accessible_app_first_denies_later_apps_with_one_request() {
    let server = MockServer::start().await;
    mount_apps(&server, 3).await;
    mount_app(&server, 1).await;
    deny_the_rest(&server).await;

    let report = check_access(&context(&server, &[])).await.unwrap();
    assert_eq!(
        app_statuses(&report),
        [
            ("com.example.app1", AppStatus::Accessible),
            ("com.example.app2", AppStatus::Denied),
            ("com.example.app3", AppStatus::Denied)
        ]
    );
    let later = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| {
            let path = request.url.path();
            path.contains("app2") || path.contains("app3")
        })
        .count();
    assert_eq!(later, 2);
}

#[tokio::test]
async fn key_without_list_apps_checks_profile_packages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/public/v1/application"))
        .respond_with(forbidden())
        .mount(&server)
        .await;
    mount_app(&server, 1).await;
    deny_the_rest(&server).await;

    let report = check_access(&context(&server, &["com.example.app1"]))
        .await
        .unwrap();
    assert_eq!(status(&report, "apps"), SectionStatus::Denied);
    assert_eq!(status(&report, "releases"), SectionStatus::Available);
    assert_eq!(status(&report, "reviews"), SectionStatus::Available);
    // Без appId платежи и каталог не проверить.
    assert_eq!(status(&report, "purchases"), SectionStatus::NotChecked);
    assert_eq!(status(&report, "catalog"), SectionStatus::NotChecked);
    assert_eq!(report.apps_source, "profile");
    assert_eq!(
        app_statuses(&report),
        [("com.example.app1", AppStatus::Accessible)]
    );

    // Без пакетов профиля проверять разделы приложения не на чем.
    let report = check_access(&context(&server, &[])).await.unwrap();
    assert!(report.apps.is_empty());
    assert_eq!(status(&report, "releases"), SectionStatus::NotChecked);
    assert!(
        report.notes[0].contains("no default packages"),
        "{:?}",
        report.notes
    );
}

#[tokio::test]
async fn token_failure_stops_the_check() {
    let server = MockServer::start().await;
    let ctx = Context::new(
        rustore_api::executor::ApiClient::new(
            rustore_api::executor::ClientConfig::new(None).unwrap(),
        )
        .unwrap(),
        FilePolicy::disabled(),
        AccessLevel::Read,
        Vec::new(),
    );
    let err = check_access(&ctx).await.unwrap_err();
    assert!(err.message().contains("no RuStore API key"), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn unreachable_api_stops_the_check() {
    // Порт закрыт: соединение не устанавливается.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let ctx = testing::context(
        &format!("http://{address}"),
        AccessLevel::Read,
        &["com.example.app1"],
        FilePolicy::disabled(),
    )
    .unwrap();
    let err = check_access(&ctx).await.unwrap_err();
    assert!(err.message().contains("could not reach"), "{err}");
}
