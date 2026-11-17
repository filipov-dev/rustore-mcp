//! Составные инструменты: несколько запросов к RuStore API за один вызов MCP-инструмента
//! (поиск приложения по всем страницам, сводка релиза, загрузка сборки в черновик и т. п.).
//!
//! Модуль задаёт контракт между составными инструментами и ядром сервера:
//!
//! - группа инструментов — отдельный файл `composite/<группа>.rs` с константой
//!   `pub const TOOLS: &[CompositeTool]`; группы перечислены в макросе `groups!` ниже, и
//!   [`all`] собирает их в один список;
//! - сервер регистрирует составные инструменты так же, как инструменты операций: по уровню
//!   доступа ([`CompositeTool::access`]), набору ([`CompositeTool::toolset`]) и признаку
//!   локальных файлов ([`CompositeTool::uses_local_files`]); `api_search` находит их наравне
//!   с операциями, `list_toolsets` и `enable_toolset` учитывают их в своих наборах;
//! - аргументы проверяются по схеме инструмента до вызова ([`check_arguments`]: неизвестные
//!   и пропущенные обязательные — ошибка), аргумент `jq` добавляет и снимает сервер;
//! - результат (JSON) проходит ту же постобработку, что и ответы операций: вырезание учётных
//!   данных ([`crate::redact`]) и токена, фильтр `jq`, лимит размера.
//!
//! Инструменты обращаются к API только через [`Context`]: операции вызываются по
//! `operationId` из спецификации ([`Context::call`], [`Context::collect`]) с аргументами
//! инструмента операции — запрос собирает тот же `build_request`, что и у инструментов
//! операций, поэтому файлы проходят [`FilePolicy`], а уровень операции проверяется по уровню
//! сервера. Пути не пишутся строками, а заголовок `Public-Token` недоступен.
//!
//! Файлы хранилища RuStore по ссылкам из ответов API (иконка и скриншоты версии) скачиваются
//! только через [`Context::external`]: клиент без токена, только https и только хосты из
//! белого списка ([`rustore_api::external`]).
//!
//! Мелкие помощники (аргументы, имя пакета, ошибки) — в [`common`].
//!
//! Как добавить инструмент:
//!
//! 1. В файле своей группы (`apps.rs`, `releases.rs`, `listing.rs`, `reviews.rs`,
//!    `payments.rs`, `access.rs`) напишите функцию схемы (`fn schema() -> Value`, удобно через
//!    [`common::object_schema`]), обработчик `fn run<'a>(ctx: &'a Context, args: JsonObject)
//!    -> ToolFuture<'a> { Box::pin(async move { ... }) }` и добавьте [`CompositeTool`] в
//!    `TOOLS` группы. Новая группа — новый файл и одна строка в `groups!`.
//! 2. Операции вызывайте через [`Context::call`] (один запрос) или [`Context::collect`] (все
//!    страницы списка), имя пакета берите [`common::package_name`]. Уровень инструмента —
//!    максимальный из уровней вызываемых операций.
//! 3. Проверьте инструмент тестами с `wiremock` и [`testing::context`]; общий тест
//!    `composite_tools_are_consistent` проверит имя, набор и схему.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use rmcp::model::JsonObject;
use rustore_api::executor::{ApiClient, ApiResponse, Pagination, ResponseBody};
use rustore_api::external::ExternalClient;
use rustore_api::spec::{AccessLevel, OperationDef, operations};
use serde_json::Value;

use crate::ServerOptions;
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::progress::{Clock, Progress, ProgressUpdate};
use crate::redact::redact_api_error;
use crate::request::build_request;
use crate::response::page_argument;

pub mod common;

/// Объявляет модули групп и собирает их инструменты в [`all`]. Новая группа — одна строка
/// здесь и свой файл `composite/<группа>.rs` с `pub const TOOLS: &[CompositeTool]`.
macro_rules! groups {
    ($($group:ident),* $(,)?) => {
        $(mod $group;)*

        /// Все составные инструменты сервера в порядке групп.
        pub fn all() -> Vec<CompositeTool> {
            let mut tools = Vec::new();
            $(tools.extend_from_slice($group::TOOLS);)*
            tools
        }
    };
}

groups! {
    apps,
    releases,
    listing,
    reviews,
    payments,
    access,
    catalog,
    auth,
}

/// Результат составного инструмента: JSON для агента.
pub type ToolResult = Result<Value, ToolError>;

/// Будущее значение результата; инструменты асинхронные.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Обработчик составного инструмента. Аргументы уже проверены [`check_arguments`], `jq`
/// снят сервером.
pub type RunFn = for<'a> fn(&'a Context, JsonObject) -> ToolFuture<'a>;

/// Описание составного инструмента.
#[derive(Clone, Copy)]
pub struct CompositeTool {
    /// Имя MCP-инструмента: не длиннее 40 символов, не совпадает с операциями и встроенными
    /// инструментами.
    pub name: &'static str,
    /// Slug набора инструментов (один из наборов спецификации).
    pub toolset: &'static str,
    /// Уровень доступа, задаётся явно: не ниже уровня всех операций, которые вызывает
    /// инструмент (иначе вызов операции вернёт ошибку уровня).
    pub access: AccessLevel,
    /// Короткий заголовок (`title` в MCP), на английском.
    pub title: &'static str,
    /// Описание для агента (английский): что делает, когда использовать, что вернёт.
    pub description: &'static str,
    /// JSON Schema аргументов: `type: object`, `additionalProperties: false`, без `jq`.
    pub input_schema: fn() -> Value,
    /// Читает или пишет локальные файлы: в HTTP-режиме не регистрируется.
    pub uses_local_files: bool,
    pub run: RunFn,
}

impl fmt::Debug for CompositeTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompositeTool")
            .field("name", &self.name)
            .field("toolset", &self.toolset)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// Окружение вызова: клиент API, политика файлов, уровень сервера и пакеты профиля.
#[derive(Debug, Clone)]
pub struct Context {
    client: ApiClient,
    files: FilePolicy,
    max_access: AccessLevel,
    packages: Vec<String>,
    /// Получатель сообщений о ходе вызова, если клиент передал `progressToken`.
    progress: Option<Progress>,
    /// Часы инструментов с ожиданием; в тестах — ручные.
    clock: Clock,
    /// Клиент хранилища RuStore; `None` — его не удалось создать (ошибка — при скачивании).
    external: Option<ExternalClient>,
}

/// Ответ операции для составного инструмента: тело (без конверта RuStore) и постраничная
/// выдача. Пустое тело — `Value::Null`.
#[derive(Debug, Clone, PartialEq)]
pub struct OperationResponse {
    pub status: u16,
    pub body: Value,
    pub pagination: Pagination,
}

/// Сколько собирать при обходе страниц ([`Context::collect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collect {
    /// Размер страницы в запросе (`size`, `pageSize`).
    pub page_size: u64,
    /// Сколько элементов собрать самое большее; дальше обход останавливается.
    pub max_items: usize,
}

impl Default for Collect {
    fn default() -> Self {
        Self {
            page_size: 100,
            max_items: 1000,
        }
    }
}

/// Элементы всех страниц списка.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Collected {
    pub items: Vec<Value>,
    /// Список прочитан до конца; `false` — остановились на `max_items` или на
    /// [`MAX_PAGES`].
    pub complete: bool,
    /// Сколько запросов сделано.
    pub pages: usize,
    /// Всего элементов по данным RuStore (`totalElements`), если API их сообщает.
    pub total: Option<u64>,
}

/// Предел страниц за один обход: защита от зацикливания на ответах, которые всегда
/// сообщают следующую страницу.
pub const MAX_PAGES: usize = 50;

/// Аргументы постраничной выдачи с токеном продолжения: имя в запросе и поле в ответе.
const CONTINUATION_ARGS: &[&str] = &["continuationToken", "pageToken", "continuation"];
/// Поля ответа с токеном следующей страницы.
const CONTINUATION_FIELDS: &[&str] = &[
    "continuationToken",
    "pageToken",
    "nextPageToken",
    "continuation",
];
/// Аргументы размера страницы в порядке предпочтения.
const SIZE_ARGS: &[&str] = &["size", "pageSize", "limit"];
/// Поля ответа с элементами страницы.
const ITEM_FIELDS: &[&str] = &["content", "elements"];

impl Context {
    pub fn new(
        client: ApiClient,
        files: FilePolicy,
        max_access: AccessLevel,
        packages: Vec<String>,
    ) -> Self {
        Self {
            client,
            files,
            max_access,
            packages,
            external: ExternalClient::rustore_static().ok(),
            progress: None,
            clock: Clock::system(),
        }
    }

    /// Тот же контекст с получателем сообщений о ходе (сервер ставит его на один вызов).
    pub fn with_progress(mut self, progress: Progress) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Тот же контекст с другими часами (ручные часы в тестах).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Часы инструментов с ожиданием.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Сообщает клиенту о ходе вызова (`notifications/progress`); без `progressToken` у
    /// клиента сообщение отбрасывается.
    pub async fn report_progress(&self, progress: f64, total: Option<f64>, message: String) {
        if let Some(sink) = &self.progress {
            sink.report(ProgressUpdate {
                progress,
                total,
                message,
            })
            .await;
        }
    }

    /// Тот же контекст с другим клиентом хранилища (тесты подставляют клиент с http на
    /// loopback).
    pub fn with_external(mut self, external: ExternalClient) -> Self {
        self.external = Some(external);
        self
    }

    /// Клиент скачивания файлов хранилища RuStore (без токена, белый список хостов).
    pub fn external(&self) -> Result<&ExternalClient, ToolError> {
        self.external.as_ref().ok_or_else(|| {
            ToolError::new("the download client for the RuStore storage could not be created")
        })
    }

    /// Контекст сервера с настройками `options`.
    pub(crate) fn for_server(
        client: ApiClient,
        files: FilePolicy,
        options: &ServerOptions,
    ) -> Self {
        Self::new(client, files, options.max_access, options.packages.clone())
    }

    /// Политика локальных файлов сервера (для `file` и `save_to` составных инструментов).
    pub fn files(&self) -> &FilePolicy {
        &self.files
    }

    /// Уровень доступа сервера. Инструмент проверяет его сам, если отдельный режим вызова
    /// требует уровня выше, чем у самого инструмента.
    pub fn max_access(&self) -> AccessLevel {
        self.max_access
    }

    /// Имена пакетов по умолчанию из профиля.
    pub fn default_packages(&self) -> &[String] {
        &self.packages
    }

    /// Клиент API: для проверки прав ключа ([`crate::auth_check`]), которой нужен
    /// HTTP-статус отказа, а не только текст ошибки.
    pub(crate) fn client(&self) -> &ApiClient {
        &self.client
    }

    /// Вызывает операцию `operation_id` с аргументами её инструмента (`args` — JSON-объект,
    /// как у инструмента операции) и возвращает тело ответа.
    pub async fn call(&self, operation_id: &str, args: Value) -> Result<Value, ToolError> {
        Ok(self.request(operation_id, args).await?.body)
    }

    /// Как [`Self::call`], но с HTTP-статусом и постраничной выдачей.
    pub async fn request(
        &self,
        operation_id: &str,
        args: Value,
    ) -> Result<OperationResponse, ToolError> {
        let op = operation(operation_id)?;
        if !self.max_access.allows(op.access) {
            return Err(ToolError::new(format!(
                "`{operation_id}` needs {} access, and the server runs with {} access",
                crate::registry::access_name(op.access),
                crate::registry::access_name(self.max_access),
            )));
        }
        let Value::Object(args) = args else {
            return Err(ToolError::new(format!(
                "internal error: arguments of `{operation_id}` must be a JSON object"
            )));
        };
        let request = build_request(op, &args, &self.files)
            .map_err(|err| step_error(operation_id, err.message()))?;
        let path = request.path.clone();
        tracing::debug!(
            operation = operation_id,
            path = %rustore_api::path::path_for_log(&path),
            "шаг составного инструмента"
        );
        let response = self.client.execute(request).await.map_err(|err| {
            let (err, _) = redact_api_error(&path, err);
            step_error(
                operation_id,
                &crate::error::describe_api_error_with_hint(&err, op.service, Some(op), &path),
            )
        })?;
        Ok(into_operation_response(response))
    }

    /// Обходит все страницы списка `operation_id` и собирает элементы.
    ///
    /// Вид выдачи определяется по параметрам операции: номер страницы (`page` или
    /// `pageNumber`, с нуля) или токен продолжения (`continuationToken`, `pageToken`,
    /// `continuation`; у `list_apps` ещё `pagination: true`). Элементы — массив `content`
    /// или `elements` тела, либо само тело-массив. Аргументы страниц в `args` передавать не
    /// нужно: их ставит обход. Операция без постраничной выдачи вызывается один раз.
    pub async fn collect(
        &self,
        operation_id: &str,
        args: Value,
        limits: Collect,
    ) -> Result<Collected, ToolError> {
        let op = operation(operation_id)?;
        let Value::Object(base) = args else {
            return Err(ToolError::new(format!(
                "internal error: arguments of `{operation_id}` must be a JSON object"
            )));
        };
        let size_arg = SIZE_ARGS
            .iter()
            .copied()
            .find(|name| op.param(name).is_some());
        let page_arg = page_argument(op);
        let token_arg = CONTINUATION_ARGS
            .iter()
            .copied()
            .find(|name| op.param(name).is_some());

        let mut collected = Collected::default();
        let mut page: u64 = 0;
        let mut token: Option<String> = None;
        loop {
            let mut args = base.clone();
            if let Some(name) = size_arg {
                args.insert(name.to_owned(), Value::from(limits.page_size));
            }
            if let Some(name) = page_arg {
                args.insert(name.to_owned(), Value::from(page));
            } else if let Some(name) = token_arg {
                if op.param("pagination").is_some() {
                    args.insert("pagination".to_owned(), Value::Bool(true));
                }
                if let Some(value) = &token {
                    args.insert(name.to_owned(), Value::String(value.clone()));
                }
            }
            let response = self.request(operation_id, Value::Object(args)).await?;
            collected.pages += 1;
            if collected.total.is_none() {
                collected.total = response.pagination.total_elements;
            }
            let items = page_items(&response.body);
            let received = items.len();
            for item in items {
                if collected.items.len() >= limits.max_items {
                    return Ok(collected);
                }
                collected.items.push(item);
            }
            let next = if page_arg.is_some() {
                let more = match (
                    response.pagination.next_page,
                    response.pagination.total_pages,
                ) {
                    (Some(_), _) => true,
                    // Итоги выдачи есть, следующей страницы нет — это последняя.
                    (None, Some(_)) => false,
                    // Итогов нет (тело-массив): полная страница — возможно, не последняя.
                    (None, None) => received as u64 >= limits.page_size && received > 0,
                };
                more.then(|| {
                    page += 1;
                    None
                })
            } else if token_arg.is_some() {
                continuation_token(&response.body)
                    .filter(|next| token.as_deref() != Some(next.as_str()))
                    .map(Some)
            } else {
                None
            };
            let Some(next_token) = next else {
                collected.complete = true;
                return Ok(collected);
            };
            if collected.items.len() >= limits.max_items || collected.pages >= MAX_PAGES {
                return Ok(collected);
            }
            if next_token.is_some() {
                token = next_token;
            }
        }
    }
}

/// Операция по `operationId`. Ошибка значит, что код инструмента разошёлся со спецификацией.
pub fn operation(operation_id: &str) -> Result<&'static OperationDef, ToolError> {
    operations()
        .iter()
        .find(|op| op.operation_id == operation_id)
        .ok_or_else(|| {
            ToolError::new(format!(
                "internal error: the API operation `{operation_id}` is missing from the server's \
                 API description"
            ))
        })
}

/// Проверяет аргументы по схеме инструмента: неизвестные (схема с `additionalProperties:
/// false`) и пропущенные обязательные — ошибка. Типы значений проверяют помощники
/// [`common`] при разборе.
pub fn check_arguments(schema: &Value, args: &JsonObject) -> Result<(), ToolError> {
    let properties = schema.get("properties").and_then(Value::as_object);
    let known = |name: &str| properties.is_some_and(|props| props.contains_key(name));
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !known(key))
        .map(|key| format!("`{key}`"))
        .collect();
    if !unknown.is_empty() {
        let allowed: Vec<String> = properties
            .map(|props| props.keys().map(|key| format!("`{key}`")).collect())
            .unwrap_or_default();
        return Err(ToolError::new(format!(
            "unknown argument(s) {}; allowed arguments: {}",
            unknown.join(", "),
            if allowed.is_empty() {
                "none".to_owned()
            } else {
                allowed.join(", ")
            }
        )));
    }
    let missing: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| args.get(*name).is_none_or(Value::is_null))
        .map(|name| format!("`{name}`"))
        .collect();
    if !missing.is_empty() {
        return Err(ToolError::new(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Ошибка шага: агент видит, на какой операции остановился инструмент.
fn step_error(operation_id: &str, message: &str) -> ToolError {
    ToolError::new(format!("`{operation_id}` failed: {message}"))
}

fn into_operation_response(response: ApiResponse) -> OperationResponse {
    let body = match response.body {
        ResponseBody::Json(value) => value,
        ResponseBody::Empty => Value::Null,
        ResponseBody::Text(text) => Value::String(text),
        ResponseBody::SavedToFile {
            path,
            bytes,
            content_type,
        } => serde_json::json!({
            "saved_to": path.display().to_string(),
            "bytes": bytes,
            "content_type": content_type,
        }),
    };
    OperationResponse {
        status: response.status,
        body,
        pagination: response.pagination,
    }
}

/// Элементы одной страницы: `content`, `elements` или тело-массив.
fn page_items(body: &Value) -> Vec<Value> {
    if let Value::Array(items) = body {
        return items.clone();
    }
    ITEM_FIELDS
        .iter()
        .find_map(|field| body.get(*field).and_then(Value::as_array))
        .cloned()
        .unwrap_or_default()
}

/// Токен следующей страницы из тела ответа; пустой — страниц больше нет.
fn continuation_token(body: &Value) -> Option<String> {
    CONTINUATION_FIELDS.iter().find_map(|field| {
        body.get(*field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    })
}

/// Контекст для тестов составных инструментов: RuStore API подменён `wiremock` по адресу
/// `base_url`, постоянный токен, без повторов.
pub mod testing {
    use std::sync::Arc;
    use std::time::Duration;

    use rustore_api::StaticToken;
    use rustore_api::executor::{ApiClient, ApiError, ClientConfig, RetryPolicy};
    use rustore_api::spec::AccessLevel;

    use super::Context;
    use crate::files::FilePolicy;

    /// Токен тестового клиента.
    pub const TEST_TOKEN: &str = "test-public-token";

    /// Клиент RuStore API по адресу `base_url` с постоянным токеном.
    pub fn client(base_url: &str) -> Result<ApiClient, ApiError> {
        let mut config = ClientConfig::new(Some(Arc::new(StaticToken::new(TEST_TOKEN))))?;
        config.base_url = base_url
            .parse()
            .map_err(|err| ApiError::InvalidConfig(format!("base_url: {err}")))?;
        config.timeout = Duration::from_secs(10);
        config.retry = RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        };
        ApiClient::new(config)
    }

    /// Поддельный составной инструмент для тестов реестра и сервера: схема с необязательной
    /// строкой `text`, ответ — `{"echo": <аргументы>, "access_token": "fake-secret-value"}`
    /// (поле токена проверяет вырезание учётных данных).
    pub fn fake_tool(
        name: &'static str,
        toolset: &'static str,
        access: AccessLevel,
        uses_local_files: bool,
    ) -> super::CompositeTool {
        super::CompositeTool {
            name,
            toolset,
            access,
            title: "Fake composite tool",
            description: "Echoes its arguments.",
            input_schema: || {
                serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "additionalProperties": false
                })
            },
            uses_local_files,
            run: |_ctx, args| {
                Box::pin(async move {
                    Ok(serde_json::json!({
                        "echo": args,
                        "access_token": "fake-secret-value"
                    }))
                })
            },
        }
    }

    /// Контекст с клиентом [`client`], уровнем `access`, пакетами профиля и файлами `files`.
    pub fn context(
        base_url: &str,
        access: AccessLevel,
        packages: &[&str],
        files: FilePolicy,
    ) -> Result<Context, ApiError> {
        Ok(Context::new(
            client(base_url)?,
            files,
            access,
            packages.iter().map(|name| (*name).to_owned()).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::registry::MAX_TOOL_NAME_LEN;

    fn context(server: &MockServer, access: AccessLevel) -> Context {
        testing::context(&server.uri(), access, &[], FilePolicy::disabled()).unwrap()
    }

    fn envelope(body: Value) -> Value {
        json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
    }

    #[test]
    fn composite_tools_are_consistent() {
        let tools = all();
        let mut names: Vec<&str> = tools.iter().map(|tool| tool.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            tools.len(),
            "повтор имени составного инструмента"
        );
        for tool in &tools {
            assert!(
                !tool.name.is_empty() && tool.name.len() <= MAX_TOOL_NAME_LEN,
                "{}",
                tool.name
            );
            assert!(
                rustore_api::spec::find(tool.name).is_none(),
                "{} совпадает с операцией",
                tool.name
            );
            assert!(
                rustore_api::spec::find_toolset(tool.toolset).is_some(),
                "{}: {}",
                tool.name,
                tool.toolset
            );
            let schema = (tool.input_schema)();
            assert_eq!(schema["type"], "object", "{}", tool.name);
            assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
            assert!(schema["properties"].get("jq").is_none(), "{}", tool.name);
            assert!(!tool.description.trim().is_empty(), "{}", tool.name);
            assert!(!tool.title.trim().is_empty(), "{}", tool.name);
        }
    }

    #[test]
    fn arguments_are_checked_by_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "integer"}},
            "required": ["a"],
            "additionalProperties": false
        });
        let args = |value: Value| value.as_object().cloned().unwrap();
        assert!(check_arguments(&schema, &args(json!({"a": "x", "b": 1}))).is_ok());
        let err = check_arguments(&schema, &args(json!({"a": "x", "c": 1}))).unwrap_err();
        assert!(err.message().contains("unknown argument(s) `c`"), "{err}");
        assert!(err.message().contains("`a`, `b`"), "{err}");
        let err = check_arguments(&schema, &args(json!({"b": 1}))).unwrap_err();
        assert!(
            err.message().contains("missing required argument(s): `a`"),
            "{err}"
        );
        let err = check_arguments(&schema, &args(json!({"a": null}))).unwrap_err();
        assert!(err.message().contains("`a`"), "{err}");
    }

    #[test]
    fn operations_are_found_by_id() {
        assert_eq!(
            operation("list_versions").unwrap().tool_name,
            "list_versions"
        );
        let err = operation("no_such_operation").unwrap_err();
        assert!(err.message().contains("internal error"), "{err}");
    }

    #[tokio::test]
    async fn call_checks_access_and_reports_the_step() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/public/v1/application/com.example.app/comment/statistic",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(envelope(json!({"averageUserRating": 4.5}))),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/public/v2/application/com.example.app/version/9"))
            .respond_with(ResponseTemplate::new(404).set_body_json(
                json!({"code": "NOT_FOUND", "message": "Version not found", "body": null}),
            ))
            .mount(&server)
            .await;
        let ctx = context(&server, AccessLevel::Read);

        let body = ctx
            .call("get_app_rating", json!({"packageName": "com.example.app"}))
            .await
            .unwrap();
        assert_eq!(body["averageUserRating"], 4.5);

        let err = ctx
            .call(
                "get_version",
                json!({"packageName": "com.example.app", "versionId": 9}),
            )
            .await
            .unwrap_err();
        assert!(err.message().starts_with("`get_version` failed:"), "{err}");
        assert!(err.message().contains("Version not found"), "{err}");

        // Аргументы проверяет схема операции.
        let err = ctx
            .call("get_version", json!({"packageName": "com.example.app"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("`versionId`"), "{err}");

        // Операция выше уровня сервера не отправляется (wiremock без такого пути).
        let err = ctx
            .call(
                "create_draft_version",
                json!({"packageName": "com.example.app"}),
            )
            .await
            .unwrap_err();
        assert!(err.message().contains("needs write access"), "{err}");
    }

    #[tokio::test]
    async fn collect_walks_numbered_pages() {
        let server = MockServer::start().await;
        let base = "/public/v1/application/com.example.app/version";
        for (page, ids, total_pages) in [(0, vec![1, 2], 3), (1, vec![3, 4], 3), (2, vec![5], 3)] {
            let content: Vec<Value> = ids.iter().map(|id| json!({"versionId": id})).collect();
            Mock::given(method("GET"))
                .and(path(base))
                .and(query_param("page", page.to_string()))
                .and(query_param("size", "2"))
                .and(query_param("versionStatuses", "ACTIVE"))
                .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                    "content": content,
                    "pageNumber": page,
                    "pageSize": 2,
                    "totalElements": 5,
                    "totalPages": total_pages
                }))))
                .expect(1)
                .mount(&server)
                .await;
        }
        let ctx = context(&server, AccessLevel::Read);
        let all = ctx
            .collect(
                "list_versions",
                json!({"packageName": "com.example.app", "versionStatuses": "ACTIVE"}),
                Collect {
                    page_size: 2,
                    max_items: 100,
                },
            )
            .await
            .unwrap();
        let ids: Vec<i64> = all
            .items
            .iter()
            .map(|item| item["versionId"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, [1, 2, 3, 4, 5]);
        assert!(all.complete);
        assert_eq!(all.pages, 3);
        assert_eq!(all.total, Some(5));
    }

    #[tokio::test]
    async fn collect_stops_at_the_item_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application/com.example.app/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "content": [{"versionId": 1}, {"versionId": 2}],
                "pageNumber": 0,
                "totalPages": 10
            }))))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = context(&server, AccessLevel::Read);
        let some = ctx
            .collect(
                "list_versions",
                json!({"packageName": "com.example.app"}),
                Collect {
                    page_size: 2,
                    max_items: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(some.items.len(), 2);
        assert!(!some.complete);
    }

    #[tokio::test]
    async fn collect_follows_continuation_tokens() {
        let server = MockServer::start().await;
        let apps = "/public/v1/application";
        Mock::given(method("GET"))
            .and(path(apps))
            .and(query_param("pagination", "true"))
            .and(query_param("pageSize", "2"))
            .and(query_param_is_missing("continuationToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "content": [{"packageName": "com.example.one"}, {"packageName": "com.example.two"}],
                "pageSize": "2",
                "continuationToken": "dG9rZW4tMg"
            }))))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(apps))
            .and(query_param("continuationToken", "dG9rZW4tMg"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "content": [{"packageName": "com.example.three"}],
                "pageSize": "2"
            }))))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = context(&server, AccessLevel::Read);
        let all = ctx
            .collect(
                "list_apps",
                json!({}),
                Collect {
                    page_size: 2,
                    max_items: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(all.items.len(), 3);
        assert!(all.complete);
        assert_eq!(all.pages, 2);
    }

    #[tokio::test]
    async fn collect_stops_on_a_repeated_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
                "content": [{"packageName": "com.example.one"}],
                "continuationToken": "same"
            }))))
            .expect(2)
            .mount(&server)
            .await;
        let ctx = context(&server, AccessLevel::Read);
        let all = ctx
            .collect("list_apps", json!({}), Collect::default())
            .await
            .unwrap();
        assert_eq!(all.pages, 2);
        assert!(all.complete);
    }

    #[tokio::test]
    async fn collect_reads_array_bodies_until_a_short_page() {
        let server = MockServer::start().await;
        let reviews = "/public/v1/application/com.example.app/comment";
        Mock::given(method("GET"))
            .and(path(reviews))
            .and(query_param("page", "0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(envelope(json!([{"commentId": 1}, {"commentId": 2}]))),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(reviews))
            .and(query_param("page", "1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(envelope(json!([{"commentId": 3}]))),
            )
            .expect(1)
            .mount(&server)
            .await;
        let ctx = context(&server, AccessLevel::Read);
        let all = ctx
            .collect(
                "list_reviews",
                json!({"packageName": "com.example.app"}),
                Collect {
                    page_size: 2,
                    max_items: 100,
                },
            )
            .await
            .unwrap();
        assert_eq!(all.items.len(), 3);
        assert!(all.complete);
        assert_eq!(all.total, None);
    }
}
