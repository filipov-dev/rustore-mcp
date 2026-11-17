//! Проверка прав ключа RuStore API: какие разделы API и какие приложения ключу доступны.
//!
//! Права ключа задаются в консоли RuStore (Компания → API RuStore) отдельно по методам и по
//! приложениям, и новые методы и приложения в созданный ключ не добавляются. Без проверки
//! пользователь узнаёт об отказе только при вызове инструмента. [`check_access`] делает по
//! одному безопасному запросу чтения на раздел (набор инструментов) и сводит ответы в отчёт
//! [`AccessReport`]; его отдают инструмент `auth_check` и команда `rustore-mcp auth check`.
//!
//! Порядок:
//!
//! 1. `list_apps` — раздел «Приложения» и список приложений компании (все страницы, не больше
//!    [`MAX_APP_PAGES`]). Если список недоступен, проверяются пакеты профиля по умолчанию; у
//!    них нет `appId`, поэтому разделы платежей и каталога остаются непроверенными.
//! 2. Для приложений по порядку (не больше [`MAX_APPS_CHECKED`]) — пробные запросы разделов
//!    приложения ([`APP_SECTIONS`]). Сначала раздел, уже доступный на другом приложении: его
//!    отказ значит, что ключу не выдано само приложение, и остальные разделы на нём не
//!    проверяются. Затем — разделы, которые ещё нигде не ответили. Каждый запрос на
//!    приложении делается не больше одного раза.
//! 3. Итог раздела: `available` — ответил хотя бы на одном приложении; `denied` — 401 или 403
//!    на приложении, которое ключу доступно (не отмечен метод), или на всех проверенных
//!    приложениях (не отмечен метод или не выданы приложения — различить нельзя);
//!    `not_checked` — проверить нечем (нет приложения, `appId` или версии); `error` — другой
//!    ответ.
//!
//! Пробные запросы — только чтение, с наименьшей страницей. Раздел «Подписки» не имеет
//! списка: запрос данных подписки идёт с заведомо несуществующей покупкой, и любой ответ,
//! кроме 401 и 403, значит, что метод ключу разрешён. В отчёт не попадают тела ответов (в
//! них бывают персональные данные): только статусы, идентификаторы приложений и тексты
//! ошибок RuStore.

use rustore_api::executor::{ApiError, ResponseBody};
use rustore_api::spec::OperationDef;
use serde::Serialize;
use serde_json::{Value, json};

use crate::composite::common::{format_date, text_field, today_utc};
use crate::composite::{Context, operation};
use crate::error::{ToolError, describe_api_error};
use crate::redact::redact_api_error;
use crate::request::build_request;

/// Сколько приложений проверяется самое большее: у ключа без прав на разделы приложения
/// каждое стоит до [`APP_SECTIONS`] запросов.
pub const MAX_APPS_CHECKED: usize = 10;

/// Сколько страниц `list_apps` читается самое большее.
pub const MAX_APP_PAGES: usize = 20;

/// Размер страницы `list_apps`.
const APPS_PAGE_SIZE: u64 = 100;

/// Раздел «Приложения»: список приложений компании.
const APPS_SECTION: Section = Section {
    toolset: "apps",
    operation: "list_apps",
};

/// Разделы приложения в порядке проверки: «Иконка и скриншоты» — после версий, потому что
/// список скриншотов нужен для конкретной версии.
pub const APP_SECTIONS: &[Section] = &[
    Section {
        toolset: "releases",
        operation: "list_versions",
    },
    Section {
        toolset: "listing",
        operation: "list_screenshots",
    },
    Section {
        toolset: "reviews",
        operation: "get_app_rating",
    },
    Section {
        toolset: "purchases",
        operation: "list_payments_by_date",
    },
    Section {
        toolset: "subscriptions",
        operation: "get_subscription_v5",
    },
    Section {
        toolset: "catalog",
        operation: "list_catalog_products",
    },
    Section {
        toolset: "access",
        operation: "list_app_access",
    },
];

/// Порядок разделов для подтверждения доступа к приложению: сначала запросы с настоящими
/// данными, запрос подписки с несуществующей покупкой — последним.
const CONFIRM_ORDER: &[&str] = &[
    "releases",
    "reviews",
    "access",
    "catalog",
    "purchases",
    "listing",
    "subscriptions",
];

/// Заведомо несуществующая покупка и подписка для пробного запроса раздела «Подписки».
const PROBE_SUBSCRIPTION_ID: &str = "rustore-mcp-auth-check";
const PROBE_PURCHASE_ID: &str = "00000000-0000-0000-0000-000000000000";

/// Раздел API (набор инструментов) и операция его пробного запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    pub toolset: &'static str,
    pub operation: &'static str,
}

/// Отчёт о правах ключа.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessReport {
    /// Разделы API в порядке спецификации.
    pub sections: Vec<SectionReport>,
    /// Приложения: из `list_apps` или пакеты профиля.
    pub apps: Vec<AppReport>,
    /// Откуда список приложений: `list_apps` или `profile` (пакеты по умолчанию).
    pub apps_source: &'static str,
    /// Список `list_apps` прочитан до конца.
    pub apps_complete: bool,
    /// Пояснения для пользователя и агента.
    pub notes: Vec<String>,
}

impl AccessReport {
    /// Приложения, к которым у ключа есть доступ.
    pub fn accessible_apps(&self) -> impl Iterator<Item = &AppReport> {
        self.apps
            .iter()
            .filter(|app| app.status == AppStatus::Accessible)
    }

    /// Раздел по slug набора.
    pub fn section(&self, toolset: &str) -> Option<&SectionReport> {
        self.sections
            .iter()
            .find(|section| section.toolset == toolset)
    }
}

/// Итог раздела.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SectionReport {
    /// Slug набора инструментов (`--toolsets`).
    pub toolset: &'static str,
    /// Раздел справочника RuStore API.
    pub section: &'static str,
    pub status: SectionStatus,
    /// Инструмент пробного запроса.
    pub method: &'static str,
    /// Название метода в справочнике (так он называется в консоли RuStore).
    pub method_title: &'static str,
    /// Приложение, на котором раздел проверен.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionStatus {
    Available,
    Denied,
    NotChecked,
    Error,
}

impl SectionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Denied => "denied",
            Self::NotChecked => "not checked",
            Self::Error => "error",
        }
    }
}

/// Приложение и доступ к нему.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppReport {
    pub package_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub status: AppStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppStatus {
    /// Хотя бы один запрос по приложению прошёл.
    Accessible,
    /// Отказ в разделе, доступном на другом приложении: приложение ключу не выдано.
    Denied,
    /// Все запросы отклонены, но ни один раздел нигде не ответил: не выдано приложение или
    /// не отмечен ни один метод.
    Unknown,
    /// Не проверялось (больше [`MAX_APPS_CHECKED`]).
    NotChecked,
}

impl AppStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accessible => "accessible",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
            Self::NotChecked => "not checked",
        }
    }
}

/// Итог пробного запроса.
#[derive(Debug, Clone)]
enum Outcome {
    /// Запрос прошёл; тело — для идентификатора версии.
    Allowed(Value),
    /// 401 или 403.
    Denied(String),
    /// Проверить нечем (нет `appId` или версии).
    Skipped(String),
    /// Другая ошибка; HTTP-статус, если ответил RuStore.
    Failed(String, Option<u16>),
}

/// Приложение для проверки.
#[derive(Debug, Clone)]
struct AppRef {
    package: String,
    name: Option<String>,
    app_id: Option<Value>,
}

/// Состояние раздела во время проверки.
#[derive(Debug, Default)]
struct SectionState {
    /// Приложение, на котором раздел ответил.
    available: Option<String>,
    /// Отказы: приложение и подробности.
    denied: Vec<(String, String)>,
    failed: Option<(String, String)>,
    skipped: Option<String>,
    /// Отклонён на доступном приложении: метод не отмечен, на других приложениях не
    /// проверяется.
    method_denied: bool,
}

/// Проверяет права ключа сервера (см. описание модуля). Ошибка — только когда проверить
/// нельзя совсем: нет токена или RuStore API недоступен.
pub async fn check_access(ctx: &Context) -> Result<AccessReport, ToolError> {
    let mut notes = Vec::new();
    let (apps_state, apps, apps_complete, apps_source) = match list_apps(ctx).await? {
        Listed::Apps { apps, complete } => {
            if !complete {
                notes.push(format!(
                    "`list_apps` has more than {} pages; only the first ones were read",
                    MAX_APP_PAGES
                ));
            }
            let state = SectionState {
                available: Some(String::new()),
                ..SectionState::default()
            };
            (state, apps, complete, "list_apps")
        }
        Listed::Refused(outcome) => {
            let mut state = SectionState::default();
            record(&mut state, "", outcome);
            let apps: Vec<AppRef> = ctx
                .default_packages()
                .iter()
                .map(|package| AppRef {
                    package: package.clone(),
                    name: None,
                    app_id: None,
                })
                .collect();
            notes.push(if apps.is_empty() {
                "`list_apps` is not available and the profile has no default packages, so the \
                 sections of an app were not checked"
                    .to_owned()
            } else {
                "`list_apps` is not available: the default packages of the profile were checked \
                 instead; without the app ID (`appId`) payments and the product catalog were \
                 not checked"
                    .to_owned()
            });
            (state, apps, false, "profile")
        }
    };

    let mut states: Vec<SectionState> = APP_SECTIONS
        .iter()
        .map(|_| SectionState::default())
        .collect();
    let mut reports = Vec::new();
    // Разделы, отклонённые на приложениях с неясным доступом: индекс отчёта и разделы.
    let mut unclear: Vec<(usize, Vec<usize>)> = Vec::new();
    for (index, app) in apps.iter().enumerate() {
        if index >= MAX_APPS_CHECKED {
            reports.push(app_report(
                app,
                AppStatus::NotChecked,
                Some(format!(
                    "only the first {MAX_APPS_CHECKED} apps are checked; the key works the same \
                     way for apps it has access to"
                )),
            ));
            continue;
        }
        let checked = check_app(ctx, app, &mut states).await?;
        if checked.status == AppStatus::Unknown {
            unclear.push((reports.len(), checked.denied));
        }
        reports.push(app_report(app, checked.status, checked.detail));
    }
    // Приложение проверено раньше, чем раздел ответил на другом приложении: отказ в
    // доступном разделе — отказ в приложении.
    for (report, denied) in unclear {
        if let Some(index) = denied
            .into_iter()
            .find(|index| states[*index].available.is_some())
        {
            reports[report].status = AppStatus::Denied;
            reports[report].detail = Some(format!(
                "`{}` works for other apps but is denied for this one",
                APP_SECTIONS[index].operation
            ));
        }
    }

    let mut sections = vec![section_report(APPS_SECTION, &apps_state, &reports)];
    for (section, state) in APP_SECTIONS.iter().zip(&states) {
        sections.push(section_report(*section, state, &reports));
    }
    if reports.iter().any(|app| app.status == AppStatus::Denied) {
        notes.push(
            "Some apps are denied: the key has no access to them. Apps are ticked per key in \
             the RuStore console (Company > RuStore API); apps added after the key was created \
             are not added to it."
                .to_owned(),
        );
    }
    if sections
        .iter()
        .any(|section| section.status == SectionStatus::Denied)
    {
        notes.push(
            "Denied sections: tick their methods for the key in the RuStore console (Company > \
             RuStore API) or create a new key with them; methods added to RuStore after the key \
             was created are not added to it."
                .to_owned(),
        );
    }
    Ok(AccessReport {
        sections,
        apps: reports,
        apps_source,
        apps_complete,
        notes,
    })
}

/// Список приложений или итог отказа.
enum Listed {
    Apps { apps: Vec<AppRef>, complete: bool },
    Refused(Outcome),
}

/// Все страницы `list_apps` (токен продолжения).
async fn list_apps(ctx: &Context) -> Result<Listed, ToolError> {
    let mut apps = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_APP_PAGES {
        let mut args = json!({"pagination": true, "pageSize": APPS_PAGE_SIZE});
        if let Some(token) = &token {
            args["continuationToken"] = Value::String(token.clone());
        }
        let body = match probe(ctx, APPS_SECTION.operation, args).await? {
            Outcome::Allowed(body) => body,
            // Сеть или таймаут на первом же запросе: проверять дальше бессмысленно.
            Outcome::Failed(detail, None) => return Err(ToolError::new(detail)),
            other => return Ok(Listed::Refused(other)),
        };
        let items = body
            .get("content")
            .or(Some(&body))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in &items {
            let Some(package) = text_field(item, "packageName").filter(|p| !p.is_empty()) else {
                continue;
            };
            apps.push(AppRef {
                package,
                name: text_field(item, "appName"),
                app_id: item.get("appId").filter(|id| !id.is_null()).cloned(),
            });
        }
        let next = text_field(&body, "continuationToken")
            .map(|next| next.trim().to_owned())
            .filter(|next| !next.is_empty() && token.as_deref() != Some(next.as_str()));
        match next {
            Some(next) if !items.is_empty() => token = Some(next),
            _ => {
                return Ok(Listed::Apps {
                    apps,
                    complete: true,
                });
            }
        }
    }
    Ok(Listed::Apps {
        apps,
        complete: false,
    })
}

/// Итог проверки приложения.
struct CheckedApp {
    status: AppStatus,
    detail: Option<String>,
    /// Разделы, отклонённые на этом приложении.
    denied: Vec<usize>,
}

/// Проверяет разделы на одном приложении; возвращает доступ к приложению.
async fn check_app(
    ctx: &Context,
    app: &AppRef,
    states: &mut [SectionState],
) -> Result<CheckedApp, ToolError> {
    // Итоги запросов на этом приложении: каждый раздел — не больше одного раза.
    let mut done: Vec<Option<Outcome>> = vec![None; APP_SECTIONS.len()];
    let mut accessible = false;

    // 1. Раздел, уже доступный на другом приложении: его отказ — отказ в приложении.
    let confirm = CONFIRM_ORDER.iter().find_map(|toolset| {
        let index = section_index(toolset)?;
        (states[index].available.is_some() && can_probe(index, app)).then_some(index)
    });
    if let Some(index) = confirm {
        let outcome = run_section(ctx, index, app, &mut done).await?;
        match &outcome {
            Outcome::Allowed(_) => accessible = true,
            Outcome::Denied(detail) => {
                return Ok(CheckedApp {
                    status: AppStatus::Denied,
                    detail: Some(format!(
                        "`{}` works for other apps but is denied for this one: {detail}",
                        APP_SECTIONS[index].operation
                    )),
                    denied: vec![index],
                });
            }
            Outcome::Skipped(_) | Outcome::Failed(..) => {}
        }
    }

    // 2. Разделы, которые ещё нигде не ответили и не отклонены на доступном приложении.
    for index in 0..APP_SECTIONS.len() {
        let state = &states[index];
        if state.available.is_some() || state.method_denied || done[index].is_some() {
            continue;
        }
        // Скриншоты проверяются на версии этого приложения: версии нужны и тогда, когда
        // раздел версий уже доступен на другом приложении.
        if APP_SECTIONS[index].toolset == "listing"
            && let Some(releases) = section_index("releases")
                .filter(|r| done[*r].is_none() && states[*r].available.is_some())
            && let Outcome::Allowed(_) = run_section(ctx, releases, app, &mut done).await?
        {
            accessible = true;
        }
        let outcome = run_section(ctx, index, app, &mut done).await?;
        if let Outcome::Allowed(_) = outcome {
            accessible = true;
        }
        record(&mut states[index], &app.package, outcome);
    }

    if accessible {
        // Приложение доступно: отказ в разделе на нём — не отмеченный метод.
        for (state, outcome) in states.iter_mut().zip(&done) {
            if let Some(Outcome::Denied(_)) = outcome {
                state.method_denied = true;
            }
        }
        return Ok(CheckedApp {
            status: AppStatus::Accessible,
            detail: None,
            denied: Vec::new(),
        });
    }
    let denied: Vec<usize> = done
        .iter()
        .enumerate()
        .filter(|(_, outcome)| matches!(outcome, Some(Outcome::Denied(_))))
        .map(|(index, _)| index)
        .collect();
    let detail = if !denied.is_empty() {
        "every request for this app was denied: the key has no access to the app, or none of \
         the checked methods is ticked for the key"
    } else {
        "no request for this app succeeded"
    };
    Ok(CheckedApp {
        status: AppStatus::Unknown,
        detail: Some(detail.to_owned()),
        denied,
    })
}

/// Пробный запрос раздела `index` на приложении (с учётом уже сделанных).
async fn run_section(
    ctx: &Context,
    index: usize,
    app: &AppRef,
    done: &mut [Option<Outcome>],
) -> Result<Outcome, ToolError> {
    if let Some(outcome) = &done[index] {
        return Ok(outcome.clone());
    }
    let section = APP_SECTIONS[index];
    let outcome = match section_args(section, app, done) {
        Err(reason) => Outcome::Skipped(reason),
        Ok(args) => {
            let outcome = probe(ctx, section.operation, args).await?;
            match outcome {
                // Несуществующая покупка: любой ответ, кроме отказа по правам, значит, что
                // метод ключу разрешён.
                Outcome::Failed(_, Some(400..=499)) if section.toolset == "subscriptions" => {
                    Outcome::Allowed(Value::Null)
                }
                other => other,
            }
        }
    };
    done[index] = Some(outcome.clone());
    Ok(outcome)
}

/// Аргументы пробного запроса раздела; `Err` — проверить нечем.
fn section_args(section: Section, app: &AppRef, done: &[Option<Outcome>]) -> Result<Value, String> {
    let package = app.package.as_str();
    let app_id = || {
        app.app_id.clone().ok_or_else(|| {
            "the app ID (`appId`) is unknown: `list_apps` is not available".to_owned()
        })
    };
    Ok(match section.toolset {
        "releases" => json!({"packageName": package, "filterTestingType": "ALL", "size": 1}),
        "listing" => {
            let version = section_index("releases")
                .and_then(|index| done.get(index))
                .and_then(Option::as_ref)
                .and_then(|outcome| match outcome {
                    Outcome::Allowed(body) => first_version_id(body),
                    _ => None,
                })
                .ok_or_else(|| "no version of the app was found to check it on".to_owned())?;
            json!({"packageName": package, "versionId": version, "size": 1})
        }
        "reviews" => json!({"packageName": package}),
        "purchases" => json!({
            "appId": app_id()?,
            "invoiceStatuses": ["confirmed"],
            "invoiceDate": format_date(today_utc()),
            "pageSize": 1
        }),
        "subscriptions" => json!({
            "packageName": package,
            "subscriptionId": PROBE_SUBSCRIPTION_ID,
            "purchaseId": PROBE_PURCHASE_ID
        }),
        "catalog" => json!({"appId": app_id()?, "productType": ["consumable"], "limit": 1}),
        "access" => json!({"packageName": package, "pageSize": 1}),
        other => {
            return Err(format!(
                "internal error: no check for the section `{other}`"
            ));
        }
    })
}

/// Можно ли проверить раздел на приложении без других запросов.
fn can_probe(index: usize, app: &AppRef) -> bool {
    match APP_SECTIONS[index].toolset {
        "purchases" | "catalog" => app.app_id.is_some(),
        // Скриншоты нужны для версии, а версия — из другого запроса.
        "listing" => false,
        _ => true,
    }
}

fn section_index(toolset: &str) -> Option<usize> {
    APP_SECTIONS
        .iter()
        .position(|section| section.toolset == toolset)
}

/// Первый `versionId` из ответа `list_versions`.
fn first_version_id(body: &Value) -> Option<Value> {
    body.get("content")
        .or(Some(body))
        .and_then(Value::as_array)?
        .iter()
        .find_map(|version| version.get("versionId").filter(|id| !id.is_null()))
        .cloned()
}

/// Один пробный запрос. Ошибка — только если проверять дальше бессмысленно: нет ключа или
/// токена (запрос не отправлялся).
async fn probe(ctx: &Context, operation_id: &str, args: Value) -> Result<Outcome, ToolError> {
    let op = operation(operation_id)?;
    let Value::Object(args) = args else {
        return Err(ToolError::new(format!(
            "internal error: arguments of `{operation_id}` must be a JSON object"
        )));
    };
    let request = build_request(op, &args, ctx.files())
        .map_err(|err| ToolError::new(format!("internal error: `{operation_id}`: {err}")))?;
    let path = request.path.clone();
    tracing::debug!(operation = operation_id, "проверка прав ключа");
    match ctx.client().execute(request).await {
        Ok(response) => Ok(Outcome::Allowed(match response.body {
            ResponseBody::Json(value) => value,
            _ => Value::Null,
        })),
        Err(err @ (ApiError::MissingToken | ApiError::TokenUnavailable(_))) => {
            Err(ToolError::from(err))
        }
        Err(err) => {
            let (err, _) = redact_api_error(&path, err);
            let detail = short_detail(&err, op);
            Ok(match err.status() {
                Some(401 | 403) => Outcome::Denied(detail),
                status => Outcome::Failed(detail, status),
            })
        }
    }
}

/// Короткое описание ошибки для отчёта: статус и ответ RuStore без общих подсказок.
fn short_detail(err: &ApiError, op: &OperationDef) -> String {
    let ApiError::Http(http) = err else {
        return describe_api_error(err);
    };
    let mut text = format!("HTTP {}", http.status);
    let code = http
        .code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let message = http
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match (code, message) {
        (Some(code), Some(message)) => text.push_str(&format!(" {code}: {message}")),
        (None, Some(message)) | (Some(message), None) => text.push_str(&format!(": {message}")),
        (None, None) => {}
    }
    if matches!(http.status, 401 | 403) {
        text.push_str(&format!(
            " (method \"{}\", tool `{}`)",
            op.summary, op.tool_name
        ));
    }
    text
}

/// Записывает итог запроса в состояние раздела.
fn record(state: &mut SectionState, app: &str, outcome: Outcome) {
    match outcome {
        Outcome::Allowed(_) => {
            if state.available.is_none() {
                state.available = Some(app.to_owned());
            }
        }
        Outcome::Denied(detail) => state.denied.push((app.to_owned(), detail)),
        Outcome::Failed(detail, _) => {
            if state.failed.is_none() {
                state.failed = Some((app.to_owned(), detail));
            }
        }
        Outcome::Skipped(reason) => {
            if state.skipped.is_none() {
                state.skipped = Some(reason);
            }
        }
    }
}

fn app_report(app: &AppRef, status: AppStatus, detail: Option<String>) -> AppReport {
    AppReport {
        package_name: app.package.clone(),
        app_name: app.name.clone(),
        app_id: app.app_id.as_ref().map(|id| match id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
        status,
        detail,
    }
}

/// Итог раздела по состоянию и доступу к приложениям.
fn section_report(section: Section, state: &SectionState, apps: &[AppReport]) -> SectionReport {
    let op = operation(section.operation).ok();
    let mut report = SectionReport {
        toolset: section.toolset,
        section: rustore_api::spec::find_toolset(section.toolset)
            .map(|toolset| toolset.tag)
            .unwrap_or(section.toolset),
        status: SectionStatus::NotChecked,
        method: section.operation,
        method_title: op.map(|op| op.summary).unwrap_or_default(),
        app: None,
        detail: None,
    };
    let non_empty = |app: &str| (!app.is_empty()).then(|| app.to_owned());
    if let Some(app) = &state.available {
        report.status = SectionStatus::Available;
        report.app = non_empty(app);
        return report;
    }
    let accessible = |package: &str| {
        apps.iter()
            .any(|app| app.package_name == package && app.status == AppStatus::Accessible)
    };
    // Отказ на доступном приложении (или в списке приложений) — не отмечен метод.
    if let Some((app, detail)) = state
        .denied
        .iter()
        .find(|(app, _)| app.is_empty() || accessible(app))
    {
        report.status = SectionStatus::Denied;
        report.app = non_empty(app);
        report.detail = Some(format!(
            "the method \"{}\" is not ticked for the key: {detail}",
            report.method_title
        ));
        return report;
    }
    if let Some((app, detail)) = state.denied.first() {
        let apps: Vec<&str> = state.denied.iter().map(|(app, _)| app.as_str()).collect();
        report.status = SectionStatus::Denied;
        report.app = non_empty(app);
        report.detail = Some(format!(
            "denied for every app checked ({}): the method \"{}\" is not ticked for the key, or \
             the key has no access to these apps: {detail}",
            apps.join(", "),
            report.method_title
        ));
        return report;
    }
    if let Some((app, detail)) = &state.failed {
        report.status = SectionStatus::Error;
        report.app = non_empty(app);
        report.detail = Some(detail.clone());
        return report;
    }
    report.detail = Some(
        state
            .skipped
            .clone()
            .unwrap_or_else(|| "no accessible app to check it on".to_owned()),
    );
    report
}

#[cfg(test)]
mod tests;
