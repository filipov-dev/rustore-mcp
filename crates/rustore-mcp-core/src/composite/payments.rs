//! Составные инструменты группы: платежи, покупки и подписки (наборы `purchases` и `subscriptions`).
//!
//! - `payments_report` (набор `purchases`) — платежи приложения за период: `list_payments_by_date`
//!   по каждому дню (метод принимает одну дату) со всеми страницами, итоги по статусам и
//!   продуктам (`order.itemCode`), суммы по валютам, по желанию — CSV в новый локальный файл.
//!   `appId` берётся из аргумента или по имени пакета из `list_apps`.
//!
//! Суммы RuStore — в минимальных единицах валюты (копейки), `order.amountCurrent` (с учётом
//! скидок), без него — `order.amountCreate`. Статусы в примере справочника бывают в нижнем
//! регистре, поэтому сравниваются без учёта регистра. Один счёт относится к одной дате, но
//! на всякий случай повторы `invoiceId` (сдвиг страниц во время обхода) не считаются дважды.
//!
//! Как добавить инструмент — в описании модуля [`super`].

use std::collections::{BTreeMap, HashSet};

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::common::{
    bool_arg, check_new_file, csv_line, format_date, object_schema, optional_date, optional_str,
    optional_u64, package_name, package_name_property, string_list, text_field, write_new_file,
};
use super::{Collect, CompositeTool, Context, ToolFuture};
use crate::error::ToolError;

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[CompositeTool {
    name: "payments_report",
    toolset: "purchases",
    access: AccessLevel::Read,
    title: "Payments report for a period",
    description: "Reports the payments (invoices) of one app for a period of up to 92 days: \
        reads `list_payments_by_date` for every day from `from` to `to` (all pages) and \
        returns counts and sums by status (`by_status`), by product (`by_products`, \
        `order.itemCode`, most payments first) and totals (`totals`: all payments, the \
        amount of CONFIRMED payments and of REFUNDED ones). Amounts are integers in minor \
        units of the currency (kopecks for RUB), summed per currency, from \
        `order.amountCurrent`. `statuses` limits the invoice statuses (default all: CREATED, \
        EXECUTED, CONFIRMED, CANCELLED, REJECTED, EXPIRED, PAID, REVERSED, REFUNDED). The app \
        is `appId` or, without it, the package name resolved through `list_apps`. With \
        `save_to` the payments are also written to a new local CSV file (never overwritten; \
        invoiceId, invoiceDate, invoiceStatus, refundDate, paymentDate, itemCode, \
        visualName, amountCreate, amountCurrent, currency, purchaseId, \
        acknowledgementState; `appUserId` only with `include_user_id: true`; no card data). \
        Individual payments are not returned to the agent; use `get_payment` for one \
        invoice.",
    input_schema: report_schema,
    uses_local_files: false,
    run: payments_report,
}];

/// Статусы счёта по справочнику RuStore.
const STATUSES: &[&str] = &[
    "CREATED",
    "EXECUTED",
    "CONFIRMED",
    "CANCELLED",
    "REJECTED",
    "EXPIRED",
    "PAID",
    "REVERSED",
    "REFUNDED",
];
/// Самый длинный период отчёта, дней: по запросу на каждый день.
const MAX_DAYS: i64 = 92;
/// Размер страницы `list_payments_by_date` (максимум по справочнику).
const PAGE_SIZE: u64 = 500;
/// Предел платежей за один день.
const DAY_MAX_ITEMS: usize = 25_000;
/// Сколько продуктов показывать в отчёте.
const PRODUCTS_SHOWN: usize = 50;
/// Сколько приложений просматривается при поиске `appId` по имени пакета.
const APPS_SCAN_LIMIT: usize = 5000;

/// Колонки CSV-файла отчёта.
const CSV_COLUMNS: &[&str] = &[
    "invoiceId",
    "invoiceDate",
    "invoiceStatus",
    "refundDate",
    "paymentDate",
    "itemCode",
    "visualName",
    "amountCreate",
    "amountCurrent",
    "currency",
    "purchaseId",
    "acknowledgementState",
];

fn report_schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "appId": {
                "type": "integer",
                "description": "Numeric app id from the RuStore console (`appId` of \
                    `list_apps` or `search_apps`). Without it the package name is resolved \
                    through `list_apps`."
            },
            "from": {
                "type": "string",
                "description": "First day of the period, `YYYY-MM-DD` (the invoice date)."
            },
            "to": {
                "type": "string",
                "description": "Last day of the period, inclusive, `YYYY-MM-DD`; at most 92 \
                    days after `from`. Default: `from`."
            },
            "statuses": {
                "type": "array",
                "items": {"type": "string", "enum": STATUSES},
                "description": "Invoice statuses to include (default all). CONFIRMED is a \
                    final successful payment, PAID a hold waiting for confirmation, REFUNDED \
                    money returned to the buyer."
            },
            "save_to": {
                "type": "string",
                "description": "Optional path of a new local CSV file with the payments \
                    (absolute, or relative to the server's working directory) inside the \
                    allowed directories; an existing file is never overwritten. Not \
                    available when the server runs over HTTP."
            },
            "include_user_id": {
                "type": "boolean",
                "description": "Add `appUserId` (the buyer's id in your app) to the CSV \
                    file (default false)."
            }
        }),
        &["from"],
    )
}

/// Сумма по валютам: валюта → сумма в минимальных единицах.
type Amounts = BTreeMap<String, i64>;

#[derive(Default)]
struct StatusTotal {
    count: u64,
    amount: Amounts,
}

#[derive(Default)]
struct ProductTotal {
    name: Option<String>,
    count: u64,
    by_status: BTreeMap<String, u64>,
    confirmed: Amounts,
    refunded: Amounts,
}

/// Сумма платежа и валюта из `order`.
fn amount(payment: &Value) -> Option<(String, i64)> {
    let order = payment.get("order")?;
    let value = ["amountCurrent", "amountCreate"].iter().find_map(|field| {
        text_field(order, field).and_then(|text| {
            text.parse::<i64>()
                .ok()
                .or_else(|| text.parse::<f64>().ok().map(|v| v.round() as i64))
        })
    })?;
    let currency = text_field(order, "currency").unwrap_or_else(|| "UNKNOWN".to_owned());
    Some((currency.to_uppercase(), value))
}

fn status_of(payment: &Value) -> String {
    text_field(payment, "invoiceStatus")
        .map(|status| status.to_uppercase())
        .unwrap_or_else(|| "UNKNOWN".to_owned())
}

/// Поле вложенного объекта как текст (`order.itemCode`).
fn nested(payment: &Value, object: &str, field: &str) -> String {
    payment
        .get(object)
        .and_then(|value| text_field(value, field))
        .unwrap_or_default()
}

fn add(amounts: &mut Amounts, value: Option<&(String, i64)>) {
    if let Some((currency, value)) = value {
        *amounts.entry(currency.clone()).or_default() += value;
    }
}

/// `appId` из аргумента или по имени пакета через `list_apps`.
async fn resolve_app(ctx: &Context, args: &JsonObject) -> Result<(u64, Option<String>), ToolError> {
    if let Some(app_id) = optional_u64(args, "appId")? {
        let package = optional_str(args, "packageName")?.map(str::to_owned);
        return Ok((app_id, package));
    }
    let package = package_name(ctx, args).map_err(|err| {
        ToolError::new(format!("pass `appId` or `packageName`: {}", err.message()))
    })?;
    let apps = ctx
        .collect(
            "list_apps",
            json!({}),
            Collect {
                page_size: 100,
                max_items: APPS_SCAN_LIMIT,
            },
        )
        .await?;
    let app_id = apps
        .items
        .iter()
        .find(|app| text_field(app, "packageName").as_deref() == Some(package.as_str()))
        .and_then(|app| text_field(app, "appId"))
        .and_then(|id| id.parse::<u64>().ok());
    match app_id {
        Some(id) => Ok((id, Some(package))),
        None => Err(ToolError::new(format!(
            "no app `{package}` among the {} apps of the key{}; pass `appId` (the number in \
             the app URL of the RuStore console)",
            apps.items.len(),
            if apps.complete { "" } else { " read so far" }
        ))),
    }
}

/// Статусы из аргумента (без учёта регистра); без аргумента — все.
fn statuses(args: &JsonObject) -> Result<Vec<String>, ToolError> {
    let given = string_list(args, "statuses")?;
    if given.is_empty() {
        return Ok(STATUSES.iter().map(|s| (*s).to_owned()).collect());
    }
    let mut out = Vec::new();
    for status in given {
        let upper = status.to_uppercase();
        if !STATUSES.contains(&upper.as_str()) {
            return Err(ToolError::new(format!(
                "unknown invoice status `{status}`; allowed: {}",
                STATUSES.join(", ")
            )));
        }
        if !out.contains(&upper) {
            out.push(upper);
        }
    }
    Ok(out)
}

fn csv_row(payment: &Value, include_user_id: bool) -> String {
    let mut fields: Vec<String> = vec![
        text_field(payment, "invoiceId").unwrap_or_default(),
        text_field(payment, "invoiceDate").unwrap_or_default(),
        status_of(payment),
        text_field(payment, "refundDate").unwrap_or_default(),
        nested(payment, "paymentInfo", "paymentDate"),
        nested(payment, "order", "itemCode"),
        nested(payment, "order", "visualName"),
        nested(payment, "order", "amountCreate"),
        nested(payment, "order", "amountCurrent"),
        nested(payment, "order", "currency"),
        text_field(payment, "purchaseId").unwrap_or_default(),
        text_field(payment, "acknowledgementState").unwrap_or_default(),
    ];
    if include_user_id {
        fields.push(text_field(payment, "appUserId").unwrap_or_default());
    }
    csv_line(&fields)
}

fn payments_report<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let from = optional_date(&args, "from")?.ok_or_else(|| {
            ToolError::new("missing required argument `from` (a date like 2026-01-31)")
        })?;
        let to = optional_date(&args, "to")?.unwrap_or(from);
        if from > to {
            return Err(ToolError::new("`from` must not be later than `to`"));
        }
        if to - from >= MAX_DAYS {
            return Err(ToolError::new(format!(
                "the period is longer than {MAX_DAYS} days; split it into several reports"
            )));
        }
        let statuses = statuses(&args)?;
        // API принимает статусы только строчными (на `CONFIRMED` — 400), агенту и в отчёте
        // они показываются заглавными, как в ответах.
        let query_statuses: Vec<String> = statuses.iter().map(|s| s.to_lowercase()).collect();
        let save_to = optional_str(&args, "save_to")?;
        let include_user_id = bool_arg(&args, "include_user_id", false)?;
        if let Some(save_to) = save_to {
            check_new_file(ctx, "save_to", save_to)?;
        }
        let (app_id, package) = resolve_app(ctx, &args).await?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut payments: Vec<Value> = Vec::new();
        let mut incomplete_days = Vec::new();
        let mut requests = 0usize;
        for day in from..=to {
            let collected = ctx
                .collect(
                    "list_payments_by_date",
                    json!({
                        "appId": app_id,
                        "invoiceDate": format_date(day),
                        "invoiceStatuses": query_statuses,
                    }),
                    Collect {
                        page_size: PAGE_SIZE,
                        max_items: DAY_MAX_ITEMS,
                    },
                )
                .await?;
            requests += collected.pages;
            if !collected.complete {
                incomplete_days.push(format_date(day));
            }
            for payment in collected.items {
                if let Some(id) = text_field(&payment, "invoiceId")
                    && !seen.insert(id)
                {
                    continue;
                }
                payments.push(payment);
            }
        }

        let mut by_status: BTreeMap<String, StatusTotal> = BTreeMap::new();
        let mut by_product: BTreeMap<String, ProductTotal> = BTreeMap::new();
        let mut confirmed = Amounts::new();
        let mut refunded = Amounts::new();
        for payment in &payments {
            let status = status_of(payment);
            let value = amount(payment);
            let entry = by_status.entry(status.clone()).or_default();
            entry.count += 1;
            add(&mut entry.amount, value.as_ref());
            let item = nested(payment, "order", "itemCode");
            let product = by_product
                .entry(if item.is_empty() {
                    "(unknown)".to_owned()
                } else {
                    item
                })
                .or_default();
            product.count += 1;
            if product.name.is_none() {
                product.name = payment
                    .get("order")
                    .and_then(|order| text_field(order, "visualName"));
            }
            *product.by_status.entry(status.clone()).or_default() += 1;
            match status.as_str() {
                "CONFIRMED" => {
                    add(&mut product.confirmed, value.as_ref());
                    add(&mut confirmed, value.as_ref());
                }
                "REFUNDED" => {
                    add(&mut product.refunded, value.as_ref());
                    add(&mut refunded, value.as_ref());
                }
                _ => {}
            }
        }

        let mut products: Vec<(String, ProductTotal)> = by_product.into_iter().collect();
        // Больше платежей — выше; при равенстве — по коду продукта.
        products.sort_by(|(a_code, a), (b_code, b)| b.count.cmp(&a.count).then(a_code.cmp(b_code)));
        let products_total = products.len();
        let products: Vec<Value> = products
            .into_iter()
            .take(PRODUCTS_SHOWN)
            .map(|(code, total)| {
                let mut record = Map::new();
                record.insert("itemCode".into(), json!(code));
                if let Some(name) = total.name {
                    record.insert("visualName".into(), json!(name));
                }
                record.insert("count".into(), json!(total.count));
                record.insert("by_status".into(), json!(total.by_status));
                record.insert("confirmed_amount".into(), json!(total.confirmed));
                if !total.refunded.is_empty() {
                    record.insert("refunded_amount".into(), json!(total.refunded));
                }
                Value::Object(record)
            })
            .collect();
        let by_status: Map<String, Value> = by_status
            .into_iter()
            .map(|(status, total)| {
                (
                    status,
                    json!({"count": total.count, "amount": total.amount}),
                )
            })
            .collect();

        let mut result = Map::new();
        result.insert("appId".into(), json!(app_id));
        if let Some(package) = package {
            result.insert("packageName".into(), json!(package));
        }
        result.insert(
            "period".into(),
            json!({"from": format_date(from), "to": format_date(to), "days": to - from + 1}),
        );
        result.insert("statuses".into(), json!(statuses));
        result.insert("requests".into(), json!(requests));
        result.insert("complete".into(), json!(incomplete_days.is_empty()));
        result.insert(
            "totals".into(),
            json!({
                "payments": payments.len(),
                "confirmed_amount": confirmed,
                "refunded_amount": refunded,
            }),
        );
        result.insert("by_status".into(), Value::Object(by_status));
        result.insert("by_products".into(), Value::Array(products));
        if products_total > PRODUCTS_SHOWN {
            result.insert("products_total".into(), json!(products_total));
        }
        if let Some(save_to) = save_to {
            let mut header: Vec<&str> = CSV_COLUMNS.to_vec();
            if include_user_id {
                header.push("appUserId");
            }
            let mut csv = csv_line(&header);
            for payment in &payments {
                csv.push_str(&csv_row(payment, include_user_id));
            }
            let path = write_new_file(ctx, "save_to", save_to, csv.as_bytes())?;
            result.insert("saved_to".into(), json!(path.display().to_string()));
            result.insert("rows".into(), json!(payments.len()));
        }
        let mut notes =
            vec!["amounts are in minor units of the currency (kopecks for RUB)".to_owned()];
        if !incomplete_days.is_empty() {
            notes.push(format!(
                "not all payments were read for {}; the totals are incomplete",
                incomplete_days.join(", ")
            ));
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

    const INVOICES: &str = "/public/applications/4242/invoices";

    fn envelope(body: Value) -> Value {
        json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
    }

    fn ok(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(envelope(body))
    }

    async fn run(ctx: &Context, args: Value) -> Result<Value, ToolError> {
        let tool = &TOOLS[0];
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        super::super::check_arguments(&(tool.input_schema)(), &args)?;
        (tool.run)(ctx, args).await
    }

    fn payment(id: u64, status: &str, item: &str, amount: i64, currency: &str) -> Value {
        json!({
            "invoiceId": id, "invoiceDate": "2026-01-10T14:31:33+03:00",
            "invoiceStatus": status, "developerPayload": "payload", "appId": 4242,
            "ownerCode": 1,
            "paymentInfo": {"paymentDate": "2026-01-10T14:32:00+03:00", "maskedPan": "**1111",
                            "paymentSystem": "visa", "bankName": "Example Bank"},
            "order": {"orderId": "o-1", "visualName": format!("Buy {item}"),
                      "amountCreate": amount + 100, "amountCurrent": amount,
                      "currency": currency, "itemCode": item, "description": "x"},
            "purchaseId": format!("p-{id}"), "appUserId": format!("user-{id}"),
            "acknowledgementState": "ACKNOWLEDGED"
        })
    }

    fn page(content: Vec<Value>, number: u64, total_pages: u64, total: u64) -> Value {
        json!({"content": content, "pageNumber": number, "pageSize": 500,
               "totalElements": total, "totalPages": total_pages})
    }

    async fn mount_payments(server: &MockServer) {
        // День 1 — две страницы.
        Mock::given(method("GET"))
            .and(path(INVOICES))
            .and(query_param("invoiceDate", "2026-01-10"))
            .and(query_param("pageNumber", "0"))
            .and(query_param("pageSize", "500"))
            .and(query_param("invoiceStatuses", "confirmed"))
            .and(query_param("invoiceStatuses", "refunded"))
            .respond_with(ok(page(
                vec![
                    payment(1, "CONFIRMED", "gems_100", 9900, "RUB"),
                    payment(2, "confirmed", "gems_100", 9900, "RUB"),
                ],
                0,
                2,
                3,
            )))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(INVOICES))
            .and(query_param("invoiceDate", "2026-01-10"))
            .and(query_param("pageNumber", "1"))
            .respond_with(ok(page(
                vec![
                    payment(3, "REFUNDED", "premium", 29900, "RUB"),
                    // Повтор счёта на сдвинутой странице не считается.
                    payment(2, "CONFIRMED", "gems_100", 9900, "RUB"),
                ],
                1,
                2,
                3,
            )))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(INVOICES))
            .and(query_param("invoiceDate", "2026-01-11"))
            .respond_with(ok(page(
                vec![payment(4, "CONFIRMED", "premium", 500, "USD")],
                0,
                1,
                1,
            )))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_apps(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .and(query_param_is_missing("continuationToken"))
            .respond_with(ok(json!({
                "content": [
                    {"packageName": "com.example.other", "appId": 1},
                    {"packageName": "com.example.app", "appId": 4242}
                ]
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    struct Dir {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn dir() -> Dir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Dir { _dir: dir, root }
    }

    fn ctx(server: &MockServer, files: FilePolicy) -> Context {
        testing::context(&server.uri(), AccessLevel::Read, &["com.example.app"], files).unwrap()
    }

    #[tokio::test]
    async fn report_resolves_app_and_aggregates() {
        let server = MockServer::start().await;
        mount_apps(&server).await;
        mount_payments(&server).await;
        let ctx = ctx(&server, FilePolicy::disabled());

        let report = run(
            &ctx,
            json!({"from": "2026-01-10", "to": "2026-01-11",
                   "statuses": ["confirmed", "REFUNDED"]}),
        )
        .await
        .unwrap();
        assert_eq!(report["appId"], 4242);
        assert_eq!(report["packageName"], "com.example.app");
        assert_eq!(
            report["period"],
            json!({"from": "2026-01-10", "to": "2026-01-11", "days": 2})
        );
        assert_eq!(report["statuses"], json!(["CONFIRMED", "REFUNDED"]));
        assert_eq!(report["requests"], 3);
        assert_eq!(report["complete"], true);
        assert_eq!(report["totals"]["payments"], 4);
        assert_eq!(
            report["totals"]["confirmed_amount"],
            json!({"RUB": 19800, "USD": 500})
        );
        assert_eq!(report["totals"]["refunded_amount"], json!({"RUB": 29900}));
        assert_eq!(
            report["by_status"],
            json!({
                "CONFIRMED": {"count": 3, "amount": {"RUB": 19800, "USD": 500}},
                "REFUNDED": {"count": 1, "amount": {"RUB": 29900}}
            })
        );
        let products = report["by_products"].as_array().unwrap();
        assert_eq!(products.len(), 2);
        assert_eq!(products[0]["itemCode"], "gems_100");
        assert_eq!(products[0]["visualName"], "Buy gems_100");
        assert_eq!(products[0]["count"], 2);
        assert_eq!(products[0]["confirmed_amount"], json!({"RUB": 19800}));
        assert!(products[0].get("refunded_amount").is_none());
        assert_eq!(products[1]["itemCode"], "premium");
        assert_eq!(
            products[1]["by_status"],
            json!({"CONFIRMED": 1, "REFUNDED": 1})
        );
        assert_eq!(products[1]["refunded_amount"], json!({"RUB": 29900}));
        assert!(report["note"].as_str().unwrap().contains("kopecks"));
        // Отдельные платежи и данные покупателей агенту не отдаются.
        let text = report.to_string();
        for leaked in ["user-1", "**1111", "Example Bank", "payload"] {
            assert!(!text.contains(leaked), "{leaked}");
        }
        assert!(report.get("saved_to").is_none());
    }

    #[tokio::test]
    async fn report_writes_csv_and_never_overwrites() {
        let server = MockServer::start().await;
        mount_payments(&server).await;
        let dir = dir();
        let policy = FilePolicy::new(std::slice::from_ref(&dir.root))
            .unwrap()
            .with_base_dir(&dir.root);
        let ctx = ctx(&server, policy);

        let report = run(
            &ctx,
            json!({"appId": 4242, "from": "2026-01-10", "to": "2026-01-11",
                   "statuses": "CONFIRMED,REFUNDED", "save_to": "payments.csv",
                   "include_user_id": true}),
        )
        .await
        .unwrap();
        let file = dir.root.join("payments.csv");
        assert_eq!(report["saved_to"], file.display().to_string());
        assert_eq!(report["rows"], 4);
        assert!(report.get("packageName").is_none());
        let csv = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(
            lines[0],
            "invoiceId,invoiceDate,invoiceStatus,refundDate,paymentDate,itemCode,visualName,\
             amountCreate,amountCurrent,currency,purchaseId,acknowledgementState,appUserId"
        );
        assert_eq!(
            lines[2],
            "2,2026-01-10T14:31:33+03:00,CONFIRMED,,2026-01-10T14:32:00+03:00,gems_100,\
             Buy gems_100,10000,9900,RUB,p-2,ACKNOWLEDGED,user-2"
        );
        assert!(!csv.contains("**1111") && !csv.contains("payload"));

        // Существующий файл — ошибка до запросов (у моков expect(1)), файл не меняется.
        let err = run(
            &ctx,
            json!({"appId": 4242, "from": "2026-01-10", "save_to": "payments.csv"}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), csv);
    }

    #[tokio::test]
    async fn report_checks_arguments() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .respond_with(ok(json!({"content": [{"packageName": "com.example.other", "appId": 1}]})))
            .mount(&server)
            .await;
        let ctx = ctx(&server, FilePolicy::disabled());
        for (args, expected) in [
            (json!({"appId": 1, "from": "2026-02-01", "to": "2026-01-01"}), "later than"),
            (json!({"appId": 1, "from": "2026-01-01", "to": "2026-06-01"}), "92 days"),
            (json!({"appId": 1, "from": "2026-01-01", "statuses": ["PAYED"]}), "`PAYED`"),
            (json!({"appId": 1}), "`from`"),
            (json!({"from": "2026-01-01"}), "no app `com.example.app`"),
            (
                json!({"appId": 1, "from": "2026-01-01", "save_to": "/tmp/x.csv"}),
                "local files are not available",
            ),
        ] {
            let err = run(&ctx, args.clone()).await.unwrap_err();
            assert!(err.message().contains(expected), "{args}: {err}");
        }
    }

    #[test]
    fn amounts_and_statuses() {
        let mut p = payment(1, "paid", "x", 100, "rub");
        assert_eq!(amount(&p), Some(("RUB".to_owned(), 100)));
        assert_eq!(status_of(&p), "PAID");
        p["order"]["amountCurrent"] = Value::Null;
        assert_eq!(amount(&p), Some(("RUB".to_owned(), 200)));
        p["order"]["amountCreate"] = json!("150.4");
        assert_eq!(amount(&p), Some(("RUB".to_owned(), 150)));
        assert_eq!(amount(&json!({"invoiceId": 1})), None);
        let all = statuses(&JsonObject::new()).unwrap();
        assert_eq!(all.len(), STATUSES.len());
    }
}
