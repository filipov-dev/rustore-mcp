use std::path::PathBuf;

use wiremock::matchers::{
    body_json, body_string_contains, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::ServerOptions;
use crate::composite::testing;
use crate::files::FilePolicy;
use crate::registry::ToolRegistry;

const VERSIONS: &str = "/public/v1/application/com.example.app/version";
const V2: &str = "/public/v2/application/com.example.app/version";

fn envelope(body: Value) -> Value {
    json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(envelope(body))
}

fn ctx_with(server: &MockServer, access: AccessLevel, packages: &[&str], files: FilePolicy) -> Context {
    testing::context(&server.uri(), access, packages, files).unwrap()
}

fn ctx(server: &MockServer, packages: &[&str]) -> Context {
    ctx_with(server, AccessLevel::Read, packages, FilePolicy::disabled())
}

fn destructive(server: &MockServer) -> Context {
    ctx_with(
        server,
        AccessLevel::Destructive,
        &["com.example.app"],
        FilePolicy::disabled(),
    )
}

async fn run(name: &str, ctx: &Context, args: Value) -> Result<Value, crate::ToolError> {
    let tool = TOOLS.iter().find(|tool| tool.name == name).unwrap();
    let Value::Object(args) = args else {
        panic!("не объект")
    };
    super::super::check_arguments(&(tool.input_schema)(), &args)?;
    (tool.run)(ctx, args).await
}

/// Ни один изменяющий запрос не должен дойти до API.
async fn forbid_changes(server: &MockServer) {
    for verb in ["POST", "PATCH", "DELETE", "PUT"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .named(format!("запрещённый {verb}"))
            .mount(server)
            .await;
    }
}

fn version(id: u64, status: &str) -> Value {
    json!({
        "versionId": id, "versionName": format!("1.{id}"), "versionCode": id,
        "versionStatus": status, "publishType": "MANUAL", "appName": "Example",
        "appType": "MAIN", "paid": false, "priceValue": 0
    })
}

// --- release_summary ----------------------------------------------------------------------

#[tokio::test]
async fn release_summary_groups_versions() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(VERSIONS))
        .and(query_param("page", "0"))
        .and(query_param_is_missing("filterTestingType"))
        .respond_with(ok(json!({
            "content": [
                version(10, "ARCHIVED"),
                version(14, "DRAFT"),
                version(13, "REJECTED_BY_MODERATOR"),
            ],
            "pageNumber": 0, "pageSize": 3, "totalElements": 6, "totalPages": 2
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut partial = version(12, "PARTIAL_ACTIVE");
    partial["partialValue"] = json!(20);
    Mock::given(method("GET"))
        .and(path(VERSIONS))
        .and(query_param("page", "1"))
        .respond_with(ok(json!({
            "content": [version(11, "ACTIVE"), partial, version(9, "AUTO_CHECK_FAILED")],
            "pageNumber": 1, "pageSize": 3, "totalElements": 6, "totalPages": 2
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/13")))
        .respond_with(ok(json!({
            "versionId": 13, "versionStatus": "REJECTED_BY_MODERATOR",
            "statusComment": "Screenshots do not match the app"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/9")))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"code": "NOT_FOUND", "message": "Version not found", "body": null}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    // Пакет — из профиля.
    let ctx = ctx(&server, &["com.example.app"]);

    let summary = run("release_summary", &ctx, json!({})).await.unwrap();
    assert_eq!(summary["packageName"], "com.example.app");
    assert_eq!(summary["scanned"], 6);
    assert_eq!(summary["complete"], true);
    assert_eq!(summary["active"][0]["versionId"], 11);
    assert_eq!(summary["partial"][0]["partialValue"], 20);
    assert_eq!(summary["draft"][0]["versionId"], 14);
    assert_eq!(summary["in_moderation"], json!([]));
    // Отклонённые — новые первыми, с комментарием модерации.
    assert_eq!(summary["rejected"][0]["versionId"], 13);
    assert_eq!(
        summary["rejected"][0]["statusComment"],
        "Screenshots do not match the app"
    );
    assert_eq!(summary["rejected"][1]["versionId"], 9);
    assert!(summary["rejected"][1].get("statusComment").is_none());
    let warnings = summary["warnings"].as_array().unwrap();
    assert!(
        warnings[0].as_str().unwrap().contains("Version not found"),
        "{warnings:?}"
    );
    assert_eq!(summary["status_counts"]["ARCHIVED"], 1);
    // Лишних полей версии нет.
    assert!(summary["draft"][0].get("appType").is_none());
    assert!(summary["note"].as_str().unwrap().contains("include_alpha"));
}

#[tokio::test]
async fn release_summary_options() {
    let server = MockServer::start().await;
    let drafts: Vec<Value> = (1..=5).map(|id| version(id, "MODERATION")).collect();
    Mock::given(method("GET"))
        .and(path("/public/v1/application/com.example.other/version"))
        .and(query_param("filterTestingType", "ALL"))
        .and(query_param("size", "100"))
        .respond_with(ok(json!({
            "content": drafts,
            "pageNumber": 0, "pageSize": 100, "totalElements": 5, "totalPages": 1
        })))
        .expect(1)
        .mount(&server)
        .await;
    let ctx = ctx(&server, &["com.example.app", "com.example.second"]);

    let summary = run(
        "release_summary",
        &ctx,
        json!({"packageName": "com.example.other", "include_alpha": true, "per_status": 2}),
    )
    .await
    .unwrap();
    assert_eq!(summary["versions"], "ALL");
    let ids: Vec<u64> = summary["in_moderation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["versionId"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [5, 4]);
    assert_eq!(summary["in_moderation_total"], 5);
    assert!(summary.get("warnings").is_none());
    assert!(summary.get("note").is_none(), "{summary}");

    // В профиле несколько пакетов — агент выбирает сам.
    let err = run("release_summary", &ctx, json!({})).await.unwrap_err();
    assert!(err.message().contains("several default packages"), "{err}");
    let err = run("release_summary", &ctx, json!({"packageName": "../x"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("invalid `packageName`"), "{err}");
}

// --- регистрация ------------------------------------------------------------------------------

#[test]
fn tools_are_registered_by_access_level_and_files() {
    let tool = |name: &str| *TOOLS.iter().find(|tool| tool.name == name).unwrap();
    assert_eq!(tool("upload_build").access, AccessLevel::Write);
    assert!(tool("upload_build").uses_local_files);
    for name in ["release_submit", "release_rollout"] {
        assert_eq!(tool(name).access, AccessLevel::Destructive, "{name}");
        assert!(!tool(name).uses_local_files, "{name}");
        // Пробный запуск — аргумент того же инструмента, отдельного уровня у него нет.
        assert!((tool(name).input_schema)()["properties"].get("dry_run").is_some());
    }

    let registry = |access: AccessLevel, local: bool| {
        ToolRegistry::new(&ServerOptions {
            max_access: access,
            toolsets: Some(vec!["releases".into()]),
            local_machine: local,
            ..ServerOptions::default()
        })
        .unwrap()
    };
    let read = registry(AccessLevel::Read, true);
    assert!(read.get("release_summary").is_some());
    for name in ["upload_build", "release_submit", "release_rollout"] {
        assert!(read.get(name).is_none(), "{name}");
    }
    let reason = read.unavailable_reason("upload_build").unwrap();
    assert!(reason.contains("`write` access"), "{reason}");

    let write = registry(AccessLevel::Write, true);
    let upload = write.get("upload_build").unwrap();
    let annotations = upload.tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    for name in ["release_submit", "release_rollout"] {
        assert!(write.get(name).is_none(), "{name}");
        let reason = write.unavailable_reason(name).unwrap();
        assert!(reason.contains("`destructive` access"), "{name}: {reason}");
    }

    let all = registry(AccessLevel::Destructive, true);
    for name in ["release_submit", "release_rollout"] {
        let annotations = all.get(name).unwrap().tool.annotations.clone().unwrap();
        assert_eq!(annotations.destructive_hint, Some(true), "{name}");
    }

    // HTTP-режим: инструмент с локальными файлами не регистрируется.
    let remote = registry(AccessLevel::Destructive, false);
    assert!(remote.get("upload_build").is_none());
    assert!(remote.get("release_submit").is_some());
}

// --- upload_build -----------------------------------------------------------------------------

/// Каталог с файлами сборки и политика файлов с ним.
struct Builds {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Builds {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }

    fn file(&self, name: &str, content: &[u8]) -> String {
        let path = self.root.join(name);
        std::fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    fn ctx(&self, server: &MockServer, access: AccessLevel) -> Context {
        ctx_with(
            server,
            access,
            &["com.example.app"],
            FilePolicy::new(std::slice::from_ref(&self.root)).unwrap(),
        )
    }
}

async fn mount_drafts(server: &MockServer, drafts: Vec<Value>, times: Option<u64>) {
    let mock = Mock::given(method("GET"))
        .and(path(VERSIONS))
        .and(query_param("versionStatuses", "DRAFT"))
        .and(query_param("filterTestingType", "ALL"))
        .respond_with(ok(json!({
            "content": drafts, "pageNumber": 0, "pageSize": 20,
            "totalElements": drafts.len(), "totalPages": 1
        })));
    match times {
        Some(n) => mock.up_to_n_times(n).mount(server).await,
        None => mock.mount(server).await,
    }
}

fn draft_details(id: u64, packages: Value) -> Value {
    json!({
        "versionId": id, "versionStatus": "DRAFT", "versionName": "2.0",
        "publishType": "INSTANTLY", "whatsNew": "Fixes", "iconUrl": "https://example.invalid/i",
        "fileUids": [{"fileUrl": "https://example.invalid/f", "type": "APK"}],
        "packages": packages
    })
}

#[tokio::test]
async fn upload_build_creates_a_draft_and_uploads_apks() {
    let server = MockServer::start().await;
    let builds = Builds::new();
    let main = builds.file("app-release.apk", b"main-apk-bytes");
    let hms = builds.file("app-hms.apk", b"hms-apk-bytes");
    mount_drafts(&server, vec![], None).await;
    Mock::given(method("POST"))
        .and(path(VERSIONS))
        .and(body_json(json!({"whatsNew": "Bug fixes", "moderInfo": "Test account inside"})))
        .respond_with(ok(json!(77)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/77/apk")))
        .and(query_param("isMainApk", "true"))
        .and(query_param_is_missing("servicesType"))
        .and(body_string_contains("name=\"file\""))
        .and(body_string_contains("filename=\"app-release.apk\""))
        .and(body_string_contains("main-apk-bytes"))
        .respond_with(ok(json!({
            "permissions": {"permission": "android.permission.INTERNET"},
            "autoDetectedSdks": {"ads": []}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/77/apk")))
        .and(query_param("isMainApk", "false"))
        .and(query_param("servicesType", "HMS"))
        .and(body_string_contains("filename=\"app-hms.apk\""))
        .and(body_string_contains("hms-apk-bytes"))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/77")))
        .respond_with(ok(draft_details(
            77,
            json!([{"apkUid": "u1", "fileName": "app-release.apk", "verCode": 20,
                    "isMainApk": true, "servicesType": "Unknown"}]),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let ctx = builds.ctx(&server, AccessLevel::Write);

    let result = run(
        "upload_build",
        &ctx,
        json!({
            "apks": [main, {"file": hms, "servicesType": "hms"}],
            "whatsNew": "Bug fixes",
            "moderInfo": "Test account inside"
        }),
    )
    .await
    .unwrap();
    assert_eq!(result["versionId"], 77);
    assert_eq!(result["draft"], "created");
    assert_eq!(result["updated_fields"], json!(["moderInfo", "whatsNew"]));
    let uploaded = result["uploaded"].as_array().unwrap();
    assert_eq!(uploaded.len(), 2);
    assert_eq!(uploaded[0]["file"], main.as_str());
    assert_eq!(uploaded[0]["isMainApk"], true);
    assert_eq!(uploaded[0]["bytes"], 14);
    assert_eq!(
        uploaded[0]["permissions"]["permission"],
        "android.permission.INTERNET"
    );
    assert_eq!(uploaded[1]["servicesType"], "HMS");
    assert_eq!(uploaded[1]["isMainApk"], false);
    assert_eq!(result["version"]["versionStatus"], "DRAFT");
    assert_eq!(result["version"]["packages"][0]["fileName"], "app-release.apk");
    // Ссылок на файлы и иконки, идентификаторов сборок в ответе нет.
    let text = result.to_string();
    assert!(!text.contains("example.invalid") && !text.contains("apkUid"), "{text}");
    let note = result["note"].as_str().unwrap();
    assert!(note.contains("release_submit"), "{note}");
}

#[tokio::test]
async fn upload_build_reuses_the_existing_draft() {
    let server = MockServer::start().await;
    let builds = Builds::new();
    let aab = builds.file("app.AAB", b"bundle-bytes");
    mount_drafts(&server, vec![version(41, "DRAFT"), version(42, "DRAFT")], None).await;
    Mock::given(method("POST"))
        .and(path(VERSIONS))
        .respond_with(ok(json!(1)))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(format!("{V2}/42")))
        .and(body_json(json!({"whatsNew": "New", "appName": "Example"})))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/42/aab")))
        .and(body_string_contains("filename=\"app.AAB\""))
        .and(body_string_contains("bundle-bytes"))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/42")))
        .respond_with(ok(draft_details(42, json!([]))))
        .mount(&server)
        .await;
    let ctx = builds.ctx(&server, AccessLevel::Write);

    let result = run(
        "upload_build",
        &ctx,
        json!({"aab": aab, "whatsNew": "New", "draft_fields": {"appName": "Example"}}),
    )
    .await
    .unwrap();
    // Новый черновик не создаётся: берётся существующий (новейший).
    assert_eq!(result["versionId"], 42);
    assert_eq!(result["draft"], "reused");
    assert_eq!(result["uploaded"][0]["type"], "aab");
    assert!(result["uploaded"][0].get("isMainApk").is_none());
}

#[tokio::test]
async fn upload_build_falls_back_to_the_draft_when_create_fails() {
    let server = MockServer::start().await;
    let builds = Builds::new();
    let apk = builds.file("app.apk", b"apk");
    // Первый список пуст, после ошибки create черновик находится.
    mount_drafts(&server, vec![], Some(1)).await;
    mount_drafts(&server, vec![version(55, "DRAFT")], None).await;
    Mock::given(method("POST"))
        .and(path(VERSIONS))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "ERROR", "message": "Draft already exists", "body": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/55/apk")))
        .and(query_param("isMainApk", "true"))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/55")))
        .respond_with(ok(draft_details(55, json!([]))))
        .mount(&server)
        .await;
    let ctx = builds.ctx(&server, AccessLevel::Write);

    let result = run("upload_build", &ctx, json!({"apks": [apk]}))
        .await
        .unwrap();
    assert_eq!(result["versionId"], 55);
    assert_eq!(result["draft"], "reused");
    let note = result["note"].as_str().unwrap();
    assert!(note.contains("Draft already exists"), "{note}");
}

#[tokio::test]
async fn upload_build_with_version_id_checks_the_draft() {
    let server = MockServer::start().await;
    let builds = Builds::new();
    let apk = builds.file("app.apk", b"apk");
    Mock::given(method("GET"))
        .and(path(format!("{V2}/90")))
        .respond_with(ok(json!({"versionId": 90, "versionStatus": "ACTIVE"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/91")))
        .respond_with(ok(draft_details(91, json!([]))))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/91/apk")))
        .and(query_param("isMainApk", "false"))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    let ctx = builds.ctx(&server, AccessLevel::Write);

    let err = run(
        "upload_build",
        &ctx,
        json!({"versionId": 90, "apks": [apk.clone()]}),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("version 90 is ACTIVE"), "{err}");

    // Явный `isMainApk: false` — основной APK не назначается; список черновиков не читается.
    let result = run(
        "upload_build",
        &ctx,
        json!({"versionId": 91, "apks": [{"file": apk, "isMainApk": false}]}),
    )
    .await
    .unwrap();
    assert_eq!(result["draft"], "given");
    assert!(
        result["note"]
            .as_str()
            .unwrap()
            .contains("no main non-HMS APK"),
        "{result}"
    );
}

#[tokio::test]
async fn upload_build_checks_files_before_any_request() {
    let server = MockServer::start().await;
    forbid_changes(&server).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let builds = Builds::new();
    let apk = builds.file("app.apk", b"apk");
    let aab = builds.file("app.aab", b"aab");
    let apks = |n: usize| -> Vec<String> {
        (0..n)
            .map(|i| builds.file(&format!("split-{i}.apk"), b"apk"))
            .collect()
    };
    let ctx = builds.ctx(&server, AccessLevel::Write);
    let fail = |args: Value| {
        let ctx = &ctx;
        async move { run("upload_build", ctx, args).await.unwrap_err() }
    };

    let err = fail(json!({})).await;
    assert!(err.message().contains("pass `aab`"), "{err}");
    let err = fail(json!({"apks": apks(11)})).await;
    // `maxItems` схемы сервер не проверяет: предел проверяет сам инструмент.
    assert!(err.message().contains("too many APK files: 11"), "{err}");
    let err = fail(json!({"aab": aab, "apks": apks(9)})).await;
    assert!(err.message().contains("too many APK files: 9"), "{err}");
    assert!(err.message().contains("up to 8"), "{err}");
    let err = fail(json!({"aab": apk})).await;
    assert!(err.message().contains(".aab extension"), "{err}");
    let err = fail(json!({"apks": [aab]})).await;
    assert!(err.message().contains(".apk extension"), "{err}");
    let err = fail(json!({"apks": [apk, apk]})).await;
    assert!(err.message().contains("more than once"), "{err}");
    let err = fail(json!({"apks": [{"file": apk, "servicesType": "GMS"}]})).await;
    assert!(err.message().contains("HMS or Unknown"), "{err}");
    let err = fail(json!({"apks": [{"file": apk, "main": true}]})).await;
    assert!(err.message().contains("unknown field `main`"), "{err}");
    let empty = builds.file("empty.apk", b"");
    let err = fail(json!({"apks": [empty]})).await;
    assert!(err.message().contains("is empty"), "{err}");
    let missing = builds.root.join("missing.apk").display().to_string();
    let err = fail(json!({"apks": [missing]})).await;
    assert!(err.message().starts_with("`apk`:"), "{err}");
    let outside = tempfile::tempdir().unwrap();
    let foreign = outside.path().join("other.apk");
    std::fs::write(&foreign, b"apk").unwrap();
    let err = fail(json!({"apks": [foreign.display().to_string()]})).await;
    assert!(err.message().contains("outside the allowed directories"), "{err}");
    let err = fail(json!({"apks": [apk], "draft_fields": {"versionCode": 1}})).await;
    assert!(err.message().contains("unknown field `versionCode`"), "{err}");
    let err = fail(json!({"apks": [apk], "whatsNew": "x".repeat(5001)})).await;
    assert!(err.message().contains("longer than 5000"), "{err}");
    let err = fail(json!({"apks": [apk], "whatsNew": "a", "draft_fields": {"whatsNew": "b"}})).await;
    assert!(err.message().contains("both"), "{err}");
}

#[tokio::test]
async fn upload_build_reports_partial_uploads() {
    let server = MockServer::start().await;
    let builds = Builds::new();
    let first = builds.file("first.apk", b"one");
    let second = builds.file("second.apk", b"two");
    mount_drafts(&server, vec![version(60, "DRAFT")], None).await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/60/apk")))
        .and(body_string_contains("filename=\"first.apk\""))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/60/apk")))
        .and(body_string_contains("filename=\"second.apk\""))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "ERROR", "message": "Version code must be higher", "body": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    let ctx = builds.ctx(&server, AccessLevel::Write);

    let err = run("upload_build", &ctx, json!({"apks": [first, second]}))
        .await
        .unwrap_err();
    let message = err.message();
    assert!(message.starts_with("`upload_apk` failed"), "{message}");
    assert!(message.contains("Version code must be higher"), "{message}");
    assert!(message.contains("draft version 60 (reused)"), "{message}");
    assert!(message.contains(&format!("uploaded before the failure: {first}")), "{message}");
    assert!(message.contains("Nothing was submitted"), "{message}");
}

// --- release_submit ---------------------------------------------------------------------------

async fn mount_get(server: &MockServer, id: u64, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("{V2}/{id}")))
        .respond_with(ok(body))
        .mount(server)
        .await;
}

fn with_build(id: u64) -> Value {
    draft_details(
        id,
        json!([{"fileName": "app.apk", "isMainApk": true, "servicesType": "Unknown"}]),
    )
}

#[tokio::test]
async fn release_submit_dry_run_plans_without_changes() {
    let server = MockServer::start().await;
    forbid_changes(&server).await;
    mount_drafts(&server, vec![version(70, "DRAFT")], None).await;
    mount_get(&server, 70, with_build(70)).await;
    let ctx = destructive(&server);

    let result = run(
        "release_submit",
        &ctx,
        json!({"publishType": "manual", "partialValue": 25, "priorityUpdate": 3, "dry_run": true}),
    )
    .await
    .unwrap();
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["versionId"], 70);
    assert_eq!(
        result["planned"],
        json!([
            {"operation": "update_draft_version", "arguments": {
                "packageName": "com.example.app", "versionId": 70,
                "body": {"publishType": "MANUAL", "partialValue": 25}}},
            {"operation": "submit_draft_for_moderation", "arguments": {
                "packageName": "com.example.app", "versionId": 70, "priorityUpdate": 3}}
        ])
    );
    assert!(result.get("executed").is_none());
    assert!(result["after_moderation"].as_str().unwrap().contains("READY_FOR_PUBLICATION"));
    assert!(result["note"].as_str().unwrap().contains("nothing was changed"));
}

#[tokio::test]
async fn release_submit_sets_publication_and_submits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/70")))
        .respond_with(ok(with_build(70)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_get(
        &server,
        70,
        json!({"versionId": 70, "versionStatus": "MODERATION", "publishType": "DELAYED"}),
    )
    .await;
    Mock::given(method("PATCH"))
        .and(path(format!("{V2}/70")))
        .and(body_json(json!({
            "publishType": "DELAYED", "publishDateTime": "2026-12-01T10:00:00+03:00"
        })))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/70/commit")))
        .and(query_param_is_missing("priorityUpdate"))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    let ctx = destructive(&server);

    let result = run(
        "release_submit",
        &ctx,
        json!({
            "versionId": 70, "publishType": "DELAYED",
            "publishDateTime": "2026-12-01T10:00:00+03:00"
        }),
    )
    .await
    .unwrap();
    assert_eq!(result["dry_run"], false);
    assert_eq!(result["executed"].as_array().unwrap().len(), 2);
    assert_eq!(result["version"]["versionStatus"], "MODERATION");
    assert!(result["after_moderation"].as_str().unwrap().contains("publishDateTime"));
}

#[tokio::test]
async fn release_submit_without_settings_only_commits() {
    let server = MockServer::start().await;
    mount_get(&server, 71, with_build(71)).await;
    Mock::given(method("PATCH"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/71/commit")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": "ERROR", "message": "Version must have at least one main non-HMS apk-file",
            "body": null
        })))
        .expect(1)
        .mount(&server)
        .await;
    let ctx = destructive(&server);

    let err = run("release_submit", &ctx, json!({"versionId": 71}))
        .await
        .unwrap_err();
    assert!(err.message().contains("main non-HMS"), "{err}");
    assert!(err.message().contains("no earlier step"), "{err}");
}

#[tokio::test]
async fn release_submit_checks_the_draft_first() {
    let server = MockServer::start().await;
    forbid_changes(&server).await;
    mount_get(&server, 72, draft_details(72, json!([]))).await;
    mount_get(
        &server,
        73,
        draft_details(73, json!([{"fileName": "a.apk", "servicesType": "HMS"}])),
    )
    .await;
    mount_get(&server, 74, json!({"versionId": 74, "versionStatus": "MODERATION"})).await;
    let mut delayed = with_build(75);
    delayed["publishType"] = json!("DELAYED");
    mount_get(&server, 75, delayed).await;
    mount_drafts(&server, vec![], None).await;
    let ctx = destructive(&server);
    let fail = |args: Value| {
        let ctx = &ctx;
        async move { run("release_submit", ctx, args).await.unwrap_err() }
    };

    let err = fail(json!({"versionId": 72})).await;
    assert!(err.message().contains("no uploaded build"), "{err}");
    let err = fail(json!({"versionId": 73})).await;
    assert!(err.message().contains("only HMS"), "{err}");
    let err = fail(json!({"versionId": 74})).await;
    assert!(err.message().contains("is MODERATION, not a draft"), "{err}");
    let err = fail(json!({"versionId": 75, "partialValue": 50})).await;
    assert!(err.message().contains("DELAYED"), "{err}");
    let err = fail(json!({})).await;
    assert!(err.message().contains("has no draft version"), "{err}");

    // Аргументы проверяются до запросов.
    let err = fail(json!({"versionId": 70, "publishType": "DELAYED"})).await;
    assert!(err.message().contains("needs `publishDateTime`"), "{err}");
    let err = fail(json!({"versionId": 70, "publishDateTime": "2026-12-01T10:00:00Z"})).await;
    assert!(err.message().contains("only with publishType DELAYED"), "{err}");
    let err = fail(json!({
        "versionId": 70, "publishType": "DELAYED", "publishDateTime": "tomorrow"
    }))
    .await;
    assert!(err.message().contains("`publishDateTime`"), "{err}");
    let err = fail(json!({"versionId": 70, "partialValue": 30})).await;
    assert!(err.message().contains("5, 10, 25, 50, 75 or 100"), "{err}");
    let err = fail(json!({"versionId": 70, "priorityUpdate": 6})).await;
    assert!(err.message().contains("from 0 to 5"), "{err}");
    let err = fail(json!({"versionId": 70, "publishType": "LATER"})).await;
    assert!(err.message().contains("INSTANTLY, MANUAL or DELAYED"), "{err}");
}

#[tokio::test]
async fn release_submit_can_skip_the_check() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/76/commit")))
        .and(query_param("priorityUpdate", "0"))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    mount_get(&server, 76, json!({"versionId": 76, "versionStatus": "AUTO_CHECK"})).await;
    let ctx = destructive(&server);

    let result = run(
        "release_submit",
        &ctx,
        json!({"versionId": 76, "check_build": false, "priorityUpdate": 0}),
    )
    .await
    .unwrap();
    assert_eq!(result["version"]["versionStatus"], "AUTO_CHECK");
    // Тип публикации неизвестен: по умолчанию RuStore публикует сразу после модерации.
    assert!(result["after_moderation"].as_str().unwrap().contains("INSTANTLY"));
}

// --- release_rollout --------------------------------------------------------------------------

fn partial(id: u64, percent: u64) -> Value {
    json!({"versionId": id, "versionStatus": "PARTIAL_ACTIVE", "publishType": "MANUAL",
           "partialValue": percent, "versionName": "3.0"})
}

#[tokio::test]
async fn release_rollout_refuses_to_lower_the_percent() {
    let server = MockServer::start().await;
    forbid_changes(&server).await;
    mount_get(&server, 80, partial(80, 50)).await;
    let ctx = destructive(&server);

    let err = run(
        "release_rollout",
        &ctx,
        json!({"action": "set_percent", "versionId": 80, "partialValue": 25}),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("cannot lower the rollout percent"), "{err}");
    assert!(err.message().contains("already published to 50%"), "{err}");
    assert!(err.message().contains("75%, 100%"), "{err}");

    // Тот же процент — ничего не меняется.
    let result = run(
        "release_rollout",
        &ctx,
        json!({"action": "set_percent", "versionId": 80, "partialValue": 50}),
    )
    .await
    .unwrap();
    assert!(result["note"].as_str().unwrap().contains("already 50%"));

    let err = run(
        "release_rollout",
        &ctx,
        json!({"action": "set_percent", "versionId": 80}),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("needs `partialValue`"), "{err}");
    let err = run(
        "release_rollout",
        &ctx,
        json!({"action": "set_percent", "versionId": 80, "partialValue": 60}),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("5, 10, 25, 50, 75 or 100"), "{err}");
}

#[tokio::test]
async fn release_rollout_raises_the_percent() {
    let server = MockServer::start().await;
    // Версия находится сама: единственная в PARTIAL_ACTIVE или READY_FOR_PUBLICATION.
    Mock::given(method("GET"))
        .and(path(VERSIONS))
        .and(query_param("versionStatuses", "PARTIAL_ACTIVE,READY_FOR_PUBLICATION"))
        .respond_with(ok(json!({
            "content": [partial(81, 10), version(79, "ACTIVE")],
            "totalPages": 1
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{V2}/81")))
        .respond_with(ok(partial(81, 10)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_get(&server, 81, partial(81, 75)).await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/81/publish-settings")))
        .and(body_json(json!({"partialValue": 75})))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    let ctx = destructive(&server);

    let result = run(
        "release_rollout",
        &ctx,
        json!({"action": "set_percent", "partialValue": 75}),
    )
    .await
    .unwrap();
    assert_eq!(result["versionId"], 81);
    assert_eq!(result["executed"][0]["operation"], "update_publish_settings");
    assert_eq!(result["version"]["partialValue"], 75);
    assert!(result["effect"].as_str().unwrap().contains("75%"));
}

#[tokio::test]
async fn release_rollout_dry_run_and_status() {
    let server = MockServer::start().await;
    forbid_changes(&server).await;
    mount_get(&server, 82, partial(82, 25)).await;
    let ctx = destructive(&server);

    let result = run(
        "release_rollout",
        &ctx,
        json!({"action": "set_percent", "versionId": 82, "partialValue": 100, "dry_run": true}),
    )
    .await
    .unwrap();
    assert_eq!(result["dry_run"], true);
    assert_eq!(
        result["planned"],
        json!([{"operation": "update_publish_settings", "arguments": {
            "packageName": "com.example.app", "versionId": 82, "body": {"partialValue": 100}}}])
    );
    assert_eq!(result["version"]["partialValue"], 25);

    let result = run(
        "release_rollout",
        &ctx,
        json!({"action": "status", "versionId": 82}),
    )
    .await
    .unwrap();
    assert_eq!(result["version"]["versionStatus"], "PARTIAL_ACTIVE");
    assert!(result["next"].as_str().unwrap().contains("25%"));
    assert!(result.get("planned").is_none() && result.get("executed").is_none());

    let err = run(
        "release_rollout",
        &ctx,
        json!({"action": "publish", "versionId": 82}),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("already published"), "{err}");
}

#[tokio::test]
async fn release_rollout_publishes_an_approved_version() {
    let server = MockServer::start().await;
    let ready = json!({"versionId": 83, "versionStatus": "READY_FOR_PUBLICATION",
                       "publishType": "MANUAL"});
    Mock::given(method("GET"))
        .and(path(format!("{V2}/83")))
        .respond_with(ok(ready))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_get(&server, 83, partial(83, 10)).await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/83/publish-settings")))
        .and(body_json(json!({"partialValue": 10})))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{VERSIONS}/83/publish")))
        .respond_with(ok(Value::Null))
        .expect(1)
        .mount(&server)
        .await;
    let ctx = destructive(&server);

    let result = run(
        "release_rollout",
        &ctx,
        json!({"action": "publish", "versionId": 83, "partialValue": 10}),
    )
    .await
    .unwrap();
    let ops: Vec<&str> = result["executed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["operation"].as_str().unwrap())
        .collect();
    assert_eq!(ops, ["update_publish_settings", "publish_version"]);
    assert_eq!(result["version"]["versionStatus"], "PARTIAL_ACTIVE");
    assert!(result["effect"].as_str().unwrap().contains("10%"));
}

#[tokio::test]
async fn release_rollout_refuses_publish_without_manual_type() {
    let server = MockServer::start().await;
    forbid_changes(&server).await;
    mount_get(
        &server,
        84,
        json!({"versionId": 84, "versionStatus": "READY_FOR_PUBLICATION", "publishType": "DELAYED"}),
    )
    .await;
    mount_get(&server, 85, json!({"versionId": 85, "versionStatus": "ACTIVE"})).await;
    Mock::given(method("GET"))
        .and(path(VERSIONS))
        .respond_with(ok(json!({
            "content": [partial(86, 5), {"versionId": 87, "versionStatus": "READY_FOR_PUBLICATION"}],
            "totalPages": 1
        })))
        .mount(&server)
        .await;
    let ctx = destructive(&server);

    let err = run(
        "release_rollout",
        &ctx,
        json!({"action": "publish", "versionId": 84}),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("uses DELAYED publication"), "{err}");
    let err = run(
        "release_rollout",
        &ctx,
        json!({"action": "set_percent", "versionId": 85, "partialValue": 50}),
    )
    .await
    .unwrap_err();
    assert!(err.message().contains("already published to all users"), "{err}");
    // Несколько подходящих версий — агент выбирает сам.
    let err = run("release_rollout", &ctx, json!({"action": "status"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("86 (PARTIAL_ACTIVE)"), "{err}");
    assert!(err.message().contains("87 (READY_FOR_PUBLICATION)"), "{err}");
    let err = run("release_rollout", &ctx, json!({"action": "halt"}))
        .await
        .unwrap_err();
    assert!(err.message().contains("status, publish or set_percent"), "{err}");
    let err = run("release_rollout", &ctx, json!({}))
        .await
        .unwrap_err();
    assert!(err.message().contains("`action`"), "{err}");
}
