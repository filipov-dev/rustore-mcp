//! Составные инструменты группы: приложения ключа (набор `apps`).
//!
//! - `search_apps` — приложения ключа со всех страниц `list_apps` с поиском по подстроке
//!   названия или имени пакета (в API поиск только по одному из полей сразу).
//!
//! Как добавить инструмент — в описании модуля [`super`].

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::common::{bounded_u64, object_schema, optional_str, pick, text_field};
use super::{Collect, CompositeTool, Context, ToolFuture};

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[
    CompositeTool {
        name: "search_apps",
        toolset: "apps",
        access: AccessLevel::Read,
        title: "Search the apps of the key",
        description: "Lists the apps of the RuStore developer account of the server's key, \
            reading all pages of `list_apps`, and finds apps by a case-insensitive substring of \
            the app name or the package name. Use it to find the package name (`packageName`) \
            or `appId` of an app; without `query` it lists all apps. Returns compact records \
            (packageName, appName, appId, appStatus, versionName, versionCode, \
            appVerUpdatedAt), best matches first: an exact package or name match, then a \
            prefix match.",
        input_schema: search_apps_schema,
        uses_local_files: false,
        run: search_apps,
    },
];

/// Поля приложения в ответе `search_apps`.
const APP_FIELDS: &[&str] = &[
    "packageName",
    "appName",
    "appId",
    "appStatus",
    "versionName",
    "versionCode",
    "appVerUpdatedAt",
];

/// Сколько приложений просматривает `search_apps`: [`super::MAX_PAGES`] страниц по 100.
const APPS_SCAN_LIMIT: usize = 5000;
const APPS_PAGE_SIZE: u64 = 100;

fn search_apps_schema() -> Value {
    object_schema(
        json!({
            "query": {
                "type": "string",
                "description": "Case-insensitive substring of the app name or the package name, \
                    like `example` or `com.example`. Omit to list all apps."
            },
            "status": {
                "type": "string",
                "description": "Only apps with this `appStatus`, like `ACTIVE`, \
                    `PARTIAL_ACTIVE`, `MODERATION` or `DRAFT`."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 500,
                "description": "Maximum number of apps to return (default 50). `matched` tells \
                    how many apps matched in all."
            }
        }),
        &[],
    )
}

fn search_apps<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let query = optional_str(&args, "query")?.map(str::to_lowercase);
        let status = optional_str(&args, "status")?.map(str::to_uppercase);
        let limit = bounded_u64(&args, "limit", 50, 1, 500)? as usize;
        let collected = ctx
            .collect(
                "list_apps",
                json!({}),
                Collect {
                    page_size: APPS_PAGE_SIZE,
                    max_items: APPS_SCAN_LIMIT,
                },
            )
            .await?;
        let mut matched: Vec<(u8, &Value)> = collected
            .items
            .iter()
            .filter(|app| {
                status.as_deref().is_none_or(|wanted| {
                    text_field(app, "appStatus").is_some_and(|s| s.eq_ignore_ascii_case(wanted))
                })
            })
            .filter_map(|app| match &query {
                None => Some((0, app)),
                Some(query) => match_rank(app, query).map(|rank| (rank, app)),
            })
            .collect();
        // Стабильная сортировка: при равном ранге остаётся порядок API.
        matched.sort_by_key(|(rank, _)| *rank);
        let apps: Vec<Value> = matched
            .iter()
            .take(limit)
            .map(|(_, app)| pick(app, APP_FIELDS))
            .collect();
        let mut result = Map::new();
        result.insert("matched".into(), json!(matched.len()));
        result.insert("returned".into(), json!(apps.len()));
        result.insert("scanned".into(), json!(collected.items.len()));
        result.insert("complete".into(), json!(collected.complete));
        result.insert("apps".into(), Value::Array(apps));
        let mut notes = Vec::new();
        if matched.len() > limit {
            notes.push(format!(
                "{} more apps matched; narrow `query` or raise `limit`",
                matched.len() - limit
            ));
        }
        if !collected.complete {
            notes.push(format!(
                "only the first {} apps of the account were searched",
                collected.items.len()
            ));
        }
        if !notes.is_empty() {
            result.insert("note".into(), json!(notes.join("; ")));
        }
        Ok(Value::Object(result))
    })
}

/// Насколько приложение подходит к запросу (в нижнем регистре): 0 — точное совпадение
/// пакета или названия, 1 — начало, 2 — подстрока, `None` — не подходит.
fn match_rank(app: &Value, query: &str) -> Option<u8> {
    [text_field(app, "packageName"), text_field(app, "appName")]
        .into_iter()
        .flatten()
        .map(|field| field.to_lowercase())
        .filter_map(|field| {
            if field == query {
                Some(0)
            } else if field.starts_with(query) {
                Some(1)
            } else if field.contains(query) {
                Some(2)
            } else {
                None
            }
        })
        .min()
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::testing;
    use crate::files::FilePolicy;

    fn envelope(body: Value) -> Value {
        json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
    }

    fn ctx(server: &MockServer, packages: &[&str]) -> Context {
        testing::context(
            &server.uri(),
            AccessLevel::Read,
            packages,
            FilePolicy::disabled(),
        )
        .unwrap()
    }

    async fn run(name: &str, ctx: &Context, args: Value) -> Result<Value, crate::ToolError> {
        let tool = TOOLS.iter().find(|tool| tool.name == name).unwrap();
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        super::super::check_arguments(&(tool.input_schema)(), &args)?;
        (tool.run)(ctx, args).await
    }

    fn app(package: &str, name: &str, status: &str) -> Value {
        json!({
            "packageName": package, "appName": name, "appId": 1, "appStatus": status,
            "versionName": "1.0", "versionCode": 1, "appVerUpdatedAt": "2026-01-01T00:00:00Z",
            "companyName": "Example", "iconUrl": "https://example.invalid/icon.png",
            "shortDescription": "Example app"
        })
    }

    async fn mount_apps(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .and(query_param("pagination", "true"))
            .and(query_param_is_missing("continuationToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "content": [
                    app("com.example.notes", "Заметки", "ACTIVE"),
                    app("com.example.player", "Example Player", "DRAFT"),
                ],
                "continuationToken": "next-1"
            }))))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .and(query_param("continuationToken", "next-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "content": [
                    app("org.sample.example", "Sample", "ACTIVE"),
                    app("com.other.tool", "Другое ЗАМЕТКИ", "ACTIVE"),
                ]
            }))))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn search_apps_reads_all_pages_and_filters() {
        let server = MockServer::start().await;
        mount_apps(&server).await;
        let ctx = ctx(&server, &[]);

        let all = run("search_apps", &ctx, json!({})).await.unwrap();
        assert_eq!(all["matched"], 4);
        assert_eq!(all["scanned"], 4);
        assert_eq!(all["complete"], true);
        let first = &all["apps"][0];
        assert_eq!(first["packageName"], "com.example.notes");
        // Компактная запись: лишних полей нет.
        assert!(first.get("iconUrl").is_none() && first.get("companyName").is_none());
        assert_eq!(first["appVerUpdatedAt"], "2026-01-01T00:00:00Z");
        assert!(all.get("note").is_none());
    }

    #[tokio::test]
    async fn search_apps_ranks_matches() {
        let server = MockServer::start().await;
        mount_apps(&server).await;
        let ctx = ctx(&server, &[]);

        // Регистр не важен (и для кириллицы), совпадение по имени и по пакету.
        let found = run("search_apps", &ctx, json!({"query": "заметки"}))
            .await
            .unwrap();
        let names: Vec<&str> = found["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| app["packageName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["com.example.notes", "com.other.tool"]);
    }

    #[tokio::test]
    async fn search_apps_orders_by_match_and_limits() {
        let server = MockServer::start().await;
        mount_apps(&server).await;
        let ctx = ctx(&server, &[]);

        // Фильтр по статусу без учёта регистра; при равном ранге — порядок API.
        let found = run(
            "search_apps",
            &ctx,
            json!({"query": "Example", "limit": 2, "status": "active"}),
        )
        .await
        .unwrap();
        assert_eq!(found["matched"], 2, "{found}");
        assert_eq!(found["apps"][0]["packageName"], "com.example.notes");
        assert_eq!(found["apps"][1]["packageName"], "org.sample.example");

        let found = run("search_apps", &ctx, json!({"query": "example", "limit": 1}))
            .await
            .unwrap();
        assert_eq!(found["returned"], 1);
        assert_eq!(found["matched"], 3);
        // Начало названия `Example Player` раньше подстроки в именах пакетов.
        assert_eq!(found["apps"][0]["packageName"], "com.example.player");
        assert!(found["note"].as_str().unwrap().contains("2 more apps"));

        let err = run("search_apps", &ctx, json!({"limit": 0}))
            .await
            .unwrap_err();
        assert!(err.message().contains("`limit`"), "{err}");
        let err = run("search_apps", &ctx, json!({"name": "x"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("unknown argument"), "{err}");
    }
}
