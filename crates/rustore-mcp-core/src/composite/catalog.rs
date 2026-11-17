//! Составные инструменты группы: продуктовый каталог (набор `catalog`).
//!
//! - `catalog_summary` — продукты и подписки одного приложения или всех приложений ключа:
//!   цена, статус, период. Каталог отвечает по `appId`, поэтому имена пакетов сопоставляются
//!   с `appId` через `list_apps`. `list_catalog_products` требует `productType`: потребляемые
//!   и непотребляемые продукты читаются отдельными запросами (все страницы).
//!
//! У приложения без подключённой монетизации каталог отвечает 400 с текстом «Submit a
//! monetization application»: это не ошибка инструмента, а состояние приложения
//! `no_monetization`. Другие ошибки по приложению уходят в `warnings` и не теряют остальные
//! приложения. Полная выгрузка (все продукты и тарифы) — CSV-файл `save_to`.
//!
//! Как добавить инструмент — в описании модуля [`super`].

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::access::{AppRef, MAX_APPS, http_status, package_list, select_apps};
use super::common::{
    bounded_u64, check_new_file, csv_line, invalid_argument, optional_str, optional_u64,
    object_schema, text_field, write_new_file,
};
use super::{Collect, CompositeTool, Context, ToolFuture};
use crate::error::ToolError;

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[CompositeTool {
    name: "catalog_summary",
    toolset: "catalog",
    access: AccessLevel::Read,
    title: "Products and subscriptions of the apps",
    description: "Summarizes the product catalog (in-app products and subscriptions) of one \
        app (`appId`, or `packageNames` with one package) or of all apps of the key (at most \
        200 per call): reads `list_catalog_products` for consumable and non-consumable \
        products and `list_catalog_subscriptions`, all pages. Per app it returns `state` \
        (`ok`; `no_monetization` when monetization is not set up for the app, which is not an \
        error; `error`, also listed in `warnings`), counts, and compact `items` (productId, \
        kind, productType, name, status, tariffs with `price` in minor units of the currency, \
        kopecks for RUB, and for subscriptions `periodType` and `duration`). `status` limits \
        products to `active` or `inactive`. `max_items` limits the items returned per app; \
        with `save_to` every product and tariff is written to a new local CSV file (never \
        overwritten).",
    input_schema: summary_schema,
    uses_local_files: false,
    run: catalog_summary,
}];

/// Типы продуктов `list_catalog_products`: параметр обязателен, читаются оба.
const PRODUCT_TYPES: &[&str] = &["consumable", "non-consumable"];
/// Размер страницы каталога (максимум по справочнику).
const PAGE_SIZE: u64 = 500;
/// Предел продуктов одного вида в приложении.
const MAX_ITEMS: usize = 10_000;

/// Колонки CSV-файла: одна строка на тариф (продукт без тарифов — одна строка).
const CSV_COLUMNS: &[&str] = &[
    "appId",
    "packageName",
    "kind",
    "productId",
    "productType",
    "name",
    "description",
    "status",
    "tariffCode",
    "tariffName",
    "price",
    "currency",
    "periodType",
    "duration",
];

fn summary_schema() -> Value {
    object_schema(
        json!({
            "appId": {
                "type": "integer",
                "description": "Numeric app id from the RuStore console (`appId` of \
                    `list_apps` or `search_apps`): only this app, without `list_apps`."
            },
            "packageNames": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Package names of the apps, like [\"com.example.app\"]. Omit \
                    both `appId` and `packageNames` to read all apps of the key."
            },
            "status": {
                "type": "string",
                "enum": ["active", "inactive"],
                "description": "Only products and subscriptions with this publication status."
            },
            "max_items": {
                "type": "integer",
                "minimum": 0,
                "maximum": 500,
                "description": "Maximum number of items returned per app (default 50; 0 \
                    returns counts only). `save_to` gets all of them."
            },
            "save_to": {
                "type": "string",
                "description": "Optional path of a new local CSV file with every product and \
                    tariff (absolute, or relative to the server's working directory) inside \
                    the allowed directories; an existing file is never overwritten. Not \
                    available when the server runs over HTTP."
            }
        }),
        &[],
    )
}

/// Состояние каталога приложения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ok,
    NoMonetization,
    Error,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoMonetization => "no_monetization",
            Self::Error => "error",
        }
    }
}

/// Каталог одного приложения.
struct AppCatalog {
    state: State,
    error: Option<String>,
    products: Vec<Value>,
    subscriptions: Vec<Value>,
    complete: bool,
}

/// Ответ 400 «Submit a monetization application»: монетизация приложения не подключена.
fn is_no_monetization(err: &ToolError) -> bool {
    http_status(err) == Some(400) && err.message().to_lowercase().contains("monetization")
}

async fn read_catalog(ctx: &Context, app_id: u64, status: Option<&str>) -> AppCatalog {
    let mut catalog = AppCatalog {
        state: State::Ok,
        error: None,
        products: Vec::new(),
        subscriptions: Vec::new(),
        complete: true,
    };
    let limits = Collect {
        page_size: PAGE_SIZE,
        max_items: MAX_ITEMS,
    };
    let with_status = |mut args: Value| {
        if let (Some(status), Value::Object(map)) = (status, &mut args) {
            map.insert("status".into(), json!(status));
        }
        args
    };
    let mut steps: Vec<(&str, Value)> = PRODUCT_TYPES
        .iter()
        .map(|kind| {
            (
                "list_catalog_products",
                with_status(json!({"appId": app_id, "productType": kind})),
            )
        })
        .collect();
    steps.push((
        "list_catalog_subscriptions",
        with_status(json!({"appId": app_id})),
    ));
    for (operation, args) in steps {
        match ctx.collect(operation, args, limits).await {
            Ok(collected) => {
                catalog.complete &= collected.complete;
                if operation == "list_catalog_subscriptions" {
                    catalog.subscriptions.extend(collected.items);
                } else {
                    catalog.products.extend(collected.items);
                }
            }
            Err(err) => {
                // Монетизации нет — у приложения нет и каталога: дальше не спрашиваем.
                if is_no_monetization(&err) {
                    catalog.state = State::NoMonetization;
                } else {
                    catalog.state = State::Error;
                    catalog.error = Some(err.message().to_owned());
                }
                catalog.products.clear();
                catalog.subscriptions.clear();
                return catalog;
            }
        }
    }
    catalog
}

/// Цена в минимальных единицах валюты числом; не число — как есть.
fn price(tariff: &Value) -> Option<Value> {
    let text = text_field(tariff, "priceValue")?;
    Some(match text.trim().parse::<i64>() {
        Ok(value) => json!(value),
        Err(_) => json!(text),
    })
}

/// Компактный тариф: код, название, цена, валюта, период подписки.
fn compact_tariff(tariff: &Value) -> Value {
    let mut out = Map::new();
    for (field, name) in [("tariffCode", "tariffCode"), ("name", "name")] {
        if let Some(value) = text_field(tariff, field) {
            out.insert(name.into(), json!(value));
        }
    }
    if let Some(price) = price(tariff) {
        out.insert("price".into(), price);
    }
    if let Some(currency) = text_field(tariff, "priceCurrency") {
        out.insert("currency".into(), json!(currency));
    }
    if let Some(period) = text_field(tariff, "periodType") {
        out.insert("periodType".into(), json!(period));
    }
    if let Some(duration) = tariff.get("duration").filter(|d| !d.is_null()) {
        out.insert("duration".into(), duration.clone());
    }
    Value::Object(out)
}

/// Код продукта: `productId` у продуктов, `productCode` у подписок.
fn product_id(item: &Value) -> Option<String> {
    text_field(item, "productId").or_else(|| text_field(item, "productCode"))
}

fn tariffs(item: &Value) -> &[Value] {
    item.get("tariffs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Компактная запись продукта или подписки для агента.
fn compact_item(item: &Value, kind: &str) -> Value {
    let mut out = Map::new();
    if let Some(id) = product_id(item) {
        out.insert("productId".into(), json!(id));
    }
    out.insert("kind".into(), json!(kind));
    for field in ["productType", "name", "status"] {
        if let Some(value) = text_field(item, field) {
            out.insert(field.into(), json!(value));
        }
    }
    out.insert(
        "tariffs".into(),
        Value::Array(tariffs(item).iter().map(compact_tariff).collect()),
    );
    Value::Object(out)
}

/// Строки CSV продукта: по одной на тариф.
fn csv_rows(app: &AppRef, app_id: u64, item: &Value, kind: &str) -> String {
    let field = |name: &str| text_field(item, name).unwrap_or_default();
    let head = [
        app_id.to_string(),
        app.package.clone(),
        kind.to_owned(),
        product_id(item).unwrap_or_default(),
        field("productType"),
        field("name"),
        field("description"),
        field("status"),
    ];
    let list = tariffs(item);
    if list.is_empty() {
        let mut row: Vec<String> = head.to_vec();
        row.extend(std::iter::repeat_n(String::new(), 6));
        return csv_line(&row);
    }
    list.iter()
        .map(|tariff| {
            let tariff_field = |name: &str| text_field(tariff, name).unwrap_or_default();
            let mut row: Vec<String> = head.to_vec();
            row.extend([
                tariff_field("tariffCode"),
                tariff_field("name"),
                tariff_field("priceValue"),
                tariff_field("priceCurrency"),
                tariff_field("periodType"),
                tariff_field("duration"),
            ]);
            csv_line(&row)
        })
        .collect()
}

fn count_status(items: &[Value], status: &str) -> usize {
    items
        .iter()
        .filter(|item| text_field(item, "status").as_deref() == Some(status))
        .count()
}

fn catalog_summary<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let app_id = optional_u64(&args, "appId")?;
        let packages = package_list(&args)?;
        if app_id.is_some() && !packages.is_empty() {
            return Err(ToolError::new(
                "pass either `appId` (one app) or `packageNames`, not both",
            ));
        }
        let status = match optional_str(&args, "status")?.map(str::to_lowercase) {
            None => None,
            Some(status) if status == "active" || status == "inactive" => Some(status),
            Some(_) => return Err(invalid_argument("status", "`active` or `inactive`")),
        };
        let max_items = bounded_u64(&args, "max_items", 50, 0, 500)? as usize;
        let save_to = optional_str(&args, "save_to")?;
        if let Some(save_to) = save_to {
            check_new_file(ctx, "save_to", save_to)?;
        }

        let (apps, mut warnings, mut notes) = match app_id {
            Some(id) => (
                vec![(
                    AppRef {
                        package: String::new(),
                        app_id: Some(id),
                        name: None,
                    },
                    id,
                )],
                Vec::new(),
                Vec::new(),
            ),
            None => {
                let selection = select_apps(ctx, &packages, true).await?;
                let mut warnings = selection.warnings;
                let mut apps = Vec::new();
                for app in selection.apps {
                    match app.app_id {
                        Some(id) => apps.push((app, id)),
                        None => warnings.push(json!({
                            "packageName": app.package,
                            "error": "`list_apps` returned no `appId` for the app"
                        })),
                    }
                }
                (apps, warnings, selection.notes)
            }
        };

        let mut csv = csv_line(CSV_COLUMNS);
        let mut rows = 0usize;
        let mut records = Vec::new();
        let (mut products_total, mut subscriptions_total) = (0usize, 0usize);
        let (mut no_monetization, mut failed) = (0usize, 0usize);
        for (app, id) in &apps {
            let catalog = read_catalog(ctx, *id, status.as_deref()).await;
            let mut record = Map::new();
            if !app.package.is_empty() {
                record.insert("packageName".into(), json!(app.package));
            }
            if let Some(name) = &app.name {
                record.insert("appName".into(), json!(name));
            }
            record.insert("appId".into(), json!(id));
            record.insert("state".into(), json!(catalog.state.name()));
            match catalog.state {
                State::NoMonetization => no_monetization += 1,
                State::Error => {
                    failed += 1;
                    let error = catalog.error.clone().unwrap_or_default();
                    record.insert("error".into(), json!(error));
                    let mut warning = Map::new();
                    if !app.package.is_empty() {
                        warning.insert("packageName".into(), json!(app.package));
                    }
                    warning.insert("appId".into(), json!(id));
                    warning.insert("error".into(), json!(error));
                    warnings.push(Value::Object(warning));
                }
                State::Ok => {}
            }
            if catalog.state == State::Ok {
                let all: Vec<(&Value, &str)> = catalog
                    .products
                    .iter()
                    .map(|item| (item, "product"))
                    .chain(catalog.subscriptions.iter().map(|item| (item, "subscription")))
                    .collect();
                products_total += catalog.products.len();
                subscriptions_total += catalog.subscriptions.len();
                let both: Vec<Value> = catalog
                    .products
                    .iter()
                    .chain(&catalog.subscriptions)
                    .cloned()
                    .collect();
                record.insert("products".into(), json!(catalog.products.len()));
                record.insert("subscriptions".into(), json!(catalog.subscriptions.len()));
                record.insert("active".into(), json!(count_status(&both, "active")));
                record.insert("inactive".into(), json!(count_status(&both, "inactive")));
                if !catalog.complete {
                    record.insert("complete".into(), json!(false));
                    warnings.push(json!({
                        "appId": id,
                        "error": format!(
                            "only the first {MAX_ITEMS} products of a kind were read"
                        )
                    }));
                }
                if max_items > 0 {
                    record.insert(
                        "items".into(),
                        Value::Array(
                            all.iter()
                                .take(max_items)
                                .map(|(item, kind)| compact_item(item, kind))
                                .collect(),
                        ),
                    );
                }
                if all.len() > max_items {
                    record.insert("items_omitted".into(), json!(all.len() - max_items));
                }
                if save_to.is_some() {
                    for (item, kind) in &all {
                        csv.push_str(&csv_rows(app, *id, item, kind));
                        rows += tariffs(item).len().max(1);
                    }
                }
            }
            records.push(Value::Object(record));
        }

        let mut result = Map::new();
        result.insert(
            "totals".into(),
            json!({
                "apps": records.len(),
                "products": products_total,
                "subscriptions": subscriptions_total,
                "no_monetization": no_monetization,
                "errors": failed,
            }),
        );
        if let Some(status) = &status {
            result.insert("status".into(), json!(status));
        }
        result.insert("apps".into(), Value::Array(records));
        if let Some(save_to) = save_to {
            let path = write_new_file(ctx, "save_to", save_to, csv.as_bytes())?;
            result.insert("saved_to".into(), json!(path.display().to_string()));
            result.insert("rows".into(), json!(rows));
        }
        notes.push("prices are in minor units of the currency (kopecks for RUB)".to_owned());
        if no_monetization > 0 {
            notes.push(format!(
                "{no_monetization} app(s) have no monetization set up (`no_monetization`): \
                 they have no catalog until a monetization application is approved in the \
                 RuStore console"
            ));
        }
        if !warnings.is_empty() {
            notes.push(format!("see `warnings` ({})", warnings.len()));
            result.insert("warnings".into(), Value::Array(warnings));
        }
        if apps.len() >= MAX_APPS && app_id.is_none() && packages.is_empty() {
            notes.push("pass `packageNames` to read other apps".to_owned());
        }
        result.insert("note".into(), json!(notes.join("; ")));
        Ok(Value::Object(result))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
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

    fn ctx(server: &MockServer, files: FilePolicy) -> Context {
        testing::context(&server.uri(), AccessLevel::Read, &[], files).unwrap()
    }

    async fn run(ctx: &Context, args: Value) -> Result<Value, ToolError> {
        let tool = &TOOLS[0];
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        super::super::check_arguments(&(tool.input_schema)(), &args)?;
        (tool.run)(ctx, args).await
    }

    fn products(app: u64) -> String {
        format!("/public/applications/{app}/catalog/products")
    }

    fn subscriptions(app: u64) -> String {
        format!("/public/applications/{app}/catalog/subscriptions")
    }

    fn page(elements: Value) -> Value {
        let count = elements.as_array().map_or(0, Vec::len);
        json!({"totalElementsCount": count, "elementsCount": count, "elements": elements})
    }

    async fn mount_apps(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .respond_with(ok(json!({"content": [
                {"packageName": "com.example.game", "appName": "Game", "appId": 11},
                {"packageName": "com.example.empty", "appName": "Empty", "appId": 12},
                {"packageName": "com.example.free", "appName": "Free", "appId": 13},
                {"packageName": "com.example.broken", "appName": "Broken", "appId": 14},
            ]})))
            .mount(server)
            .await;
    }

    /// Каталоги: у Game продукты на двух страницах и подписка, у Empty пусто, у Free нет
    /// монетизации, у Broken 403.
    async fn mount_catalogs(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(products(11)))
            .and(query_param("productType", "consumable"))
            .and(query_param("limit", "500"))
            .and(query_param_is_missing("continuationToken"))
            .respond_with(ok(json!({
                "elements": [{
                    "productId": "gems_100", "productType": "consumable", "name": "100 gems",
                    "description": "Gems, \"shiny\"", "status": "active",
                    "image": "https://example.invalid/gems.png",
                    "tariffs": [{"tariffCode": "standard", "name": "Base",
                                 "priceCurrency": "RUB", "priceValue": "9900"}]
                }],
                "continuationToken": "next"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(products(11)))
            .and(query_param("productType", "consumable"))
            .and(query_param("continuationToken", "next"))
            .respond_with(ok(page(json!([{
                "productId": "gems_500", "productType": "consumable", "name": "500 gems",
                "status": "inactive", "tariffs": []
            }]))))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(products(11)))
            .and(query_param("productType", "non-consumable"))
            .respond_with(ok(page(json!([{
                "productId": "no_ads", "productType": "non-consumable", "name": "No ads",
                "status": "active",
                "tariffs": [{"tariffCode": "standard", "priceCurrency": "RUB", "priceValue": "29900"}]
            }]))))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(subscriptions(11)))
            .respond_with(ok(page(json!([{
                "productCode": "premium_month", "productType": "subscription",
                "name": "Premium", "status": "active",
                "tariffs": [
                    {"tariffCode": "promo", "priceCurrency": "RUB", "priceValue": "0",
                     "periodType": "day", "duration": 7},
                    {"tariffCode": "main", "priceCurrency": "RUB", "priceValue": "19900",
                     "periodType": "month", "duration": 1}
                ]
            }]))))
            .mount(server)
            .await;
        for path_of in [products(12), subscriptions(12)] {
            Mock::given(method("GET"))
                .and(path(path_of))
                .respond_with(ok(json!({"totalElementsCount": 0, "elementsCount": 0})))
                .mount(server)
                .await;
        }
        // Без монетизации: на первый же запрос — 400, остальные не делаются.
        Mock::given(method("GET"))
            .and(path(products(13)))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "code": "ERROR",
                "message": "Submit a monetization application",
                "body": null
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(subscriptions(13)))
            .respond_with(ok(page(json!([]))))
            .expect(0)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(products(14)))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({"code": "FORBIDDEN", "message": "Access denied", "body": null}),
            ))
            .mount(server)
            .await;
    }

    #[test]
    fn tool_is_read_only_in_the_catalog_toolset() {
        assert_eq!(TOOLS[0].access, AccessLevel::Read);
        assert_eq!(TOOLS[0].toolset, "catalog");
        for op in ["list_catalog_products", "list_catalog_subscriptions", "list_apps"] {
            assert_eq!(
                super::super::operation(op).unwrap().access,
                AccessLevel::Read
            );
        }
    }

    #[tokio::test]
    async fn summary_reads_all_apps_and_keeps_states() {
        let server = MockServer::start().await;
        mount_apps(&server).await;
        mount_catalogs(&server).await;
        let ctx = ctx(&server, FilePolicy::disabled());

        let summary = run(&ctx, json!({})).await.unwrap();
        // После ответа «нет монетизации» каталог этого приложения больше не запрашивается.
        let requests = server.received_requests().await.unwrap();
        let free = requests
            .iter()
            .filter(|request| request.url.path() == products(13))
            .count();
        assert_eq!(free, 1);
        assert_eq!(
            summary["totals"],
            json!({"apps": 4, "products": 3, "subscriptions": 1, "no_monetization": 1,
                   "errors": 1})
        );
        let apps = summary["apps"].as_array().unwrap();
        let game = &apps[0];
        assert_eq!(game["packageName"], "com.example.game");
        assert_eq!(game["state"], "ok");
        assert_eq!(game["products"], 3);
        assert_eq!(game["subscriptions"], 1);
        assert_eq!(game["active"], 3);
        assert_eq!(game["inactive"], 1);
        let items = game["items"].as_array().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(
            items[0],
            json!({"productId": "gems_100", "kind": "product", "productType": "consumable",
                   "name": "100 gems", "status": "active",
                   "tariffs": [{"tariffCode": "standard", "name": "Base", "price": 9900,
                                "currency": "RUB"}]})
        );
        assert_eq!(items[3]["productId"], "premium_month");
        assert_eq!(items[3]["kind"], "subscription");
        assert_eq!(
            items[3]["tariffs"][1],
            json!({"tariffCode": "main", "price": 19900, "currency": "RUB",
                   "periodType": "month", "duration": 1})
        );
        // Пустой каталог — состояние `ok` с нулями.
        assert_eq!(apps[1]["state"], "ok");
        assert_eq!(apps[1]["products"], 0);
        assert_eq!(apps[1]["items"], json!([]));
        // Без монетизации — отдельное состояние, не ошибка.
        assert_eq!(apps[2]["state"], "no_monetization");
        assert!(apps[2].get("error").is_none());
        // Ошибка одного приложения не теряет остальные.
        assert_eq!(apps[3]["state"], "error");
        assert!(apps[3]["error"].as_str().unwrap().contains("HTTP 403"));
        let warnings = summary["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["packageName"], "com.example.broken");
        let note = summary["note"].as_str().unwrap();
        assert!(note.contains("kopecks") && note.contains("no monetization"), "{note}");
        assert!(summary.get("saved_to").is_none());
    }

    #[tokio::test]
    async fn summary_of_one_app_limits_items() {
        let server = MockServer::start().await;
        mount_catalogs(&server).await;
        // По `appId` список приложений не читается.
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .respond_with(ok(json!({"content": []})))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = ctx(&server, FilePolicy::disabled());

        let summary = run(&ctx, json!({"appId": 11, "max_items": 1})).await.unwrap();
        let app = &summary["apps"][0];
        assert_eq!(app["appId"], 11);
        assert!(app.get("packageName").is_none());
        assert_eq!(app["items"].as_array().unwrap().len(), 1);
        assert_eq!(app["items_omitted"], 3);
        assert!(summary.get("warnings").is_none());

        let summary = run(&ctx, json!({"appId": 12})).await.unwrap();
        assert_eq!(
            summary["totals"],
            json!({"apps": 1, "products": 0, "subscriptions": 0, "no_monetization": 0,
                   "errors": 0})
        );

        for (args, expected) in [
            (json!({"appId": 11, "packageNames": ["com.example.game"]}), "not both"),
            (json!({"status": "deleted"}), "`status`"),
            (json!({"max_items": 501}), "`max_items`"),
            (json!({"appId": 11, "save_to": "/tmp/x.csv"}), "local files are not available"),
        ] {
            let err = run(&ctx, args.clone()).await.unwrap_err();
            assert!(err.message().contains(expected), "{args}: {err}");
        }
    }

    #[tokio::test]
    async fn summary_of_listed_packages_reports_unknown_ones() {
        let server = MockServer::start().await;
        mount_apps(&server).await;
        mount_catalogs(&server).await;
        let ctx = ctx(&server, FilePolicy::disabled());

        let summary = run(
            &ctx,
            json!({"packageNames": ["com.example.free", "com.example.missing"], "max_items": 0}),
        )
        .await
        .unwrap();
        assert_eq!(summary["apps"].as_array().unwrap().len(), 1);
        assert_eq!(summary["apps"][0]["state"], "no_monetization");
        assert_eq!(summary["warnings"][0]["packageName"], "com.example.missing");
    }

    #[tokio::test]
    async fn summary_writes_every_tariff_to_csv() {
        let server = MockServer::start().await;
        mount_catalogs(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let root: PathBuf = dir.path().canonicalize().unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root))
            .unwrap()
            .with_base_dir(&root);
        let ctx = ctx(&server, policy);

        let summary = run(
            &ctx,
            json!({"appId": 11, "max_items": 0, "save_to": "catalog.csv"}),
        )
        .await
        .unwrap();
        let file = root.join("catalog.csv");
        assert_eq!(summary["saved_to"], file.display().to_string());
        assert_eq!(summary["rows"], 5);
        assert!(summary["apps"][0].get("items").is_none());
        let csv = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], CSV_COLUMNS.join(","));
        assert_eq!(
            lines[1],
            "11,,product,gems_100,consumable,100 gems,\"Gems, \"\"shiny\"\"\",active,standard,\
             Base,9900,RUB,,"
        );
        assert_eq!(
            lines[2],
            "11,,product,gems_500,consumable,500 gems,,inactive,,,,,,"
        );
        assert_eq!(
            lines[5],
            "11,,subscription,premium_month,subscription,Premium,,active,main,,19900,RUB,month,1"
        );

        // Существующий файл не перезаписывается, ошибка — до запросов.
        let err = run(&ctx, json!({"appId": 11, "save_to": "catalog.csv"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
    }
}
