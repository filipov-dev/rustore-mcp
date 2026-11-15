//! Выполнение запросов к RuStore API.
//!
//! Исполнитель не зависит от таблицы операций: он принимает метод, относительный путь,
//! query и тело ([`ApiRequest`]). Все сгенерированные инструменты и `api_request` работают
//! через [`ApiClient::execute`]. Это граница безопасности:
//! - путь проверяется [`validate_relative_path`] и приклеивается к `base_url`, поэтому
//!   запрос не уходит на другой хост;
//! - заголовок `Public-Token` формируется только здесь из источника токенов
//!   ([`ClientConfig::tokens`]), запрос не может его переопределить; заголовок помечен как
//!   чувствительный;
//! - ответ 401 один раз повторяется с новым токеном (правила — у функции `retry_after_401`);
//! - ответы RuStore приходят в конверте `{"code", "message", "body", "timestamp"}`:
//!   `code` не `OK` — ошибка ([`ApiError::Http`]) даже при HTTP 200, а при `OK` вызывающему
//!   возвращается `body`; постраничная выдача (`content`, `pageNumber`, `totalPages`)
//!   разбирается в [`Pagination`];
//! - редиректы выполняются только в пределах того же origin;
//! - размер тела ответа ограничен `max_response_bytes`, ошибки — [`ERROR_BODY_LIMIT`];
//!   файлы загружаются потоково с диска, двоичные ответы пишутся только в файл (`save_to`);
//! - при 429 (и 503 для идемпотентных методов) запрос повторяется с учётом `Retry-After`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, multipart, redirect};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::access::{AccessToken, TokenError, TokenSource};

pub use crate::path::{render_path, validate_relative_path};

/// Базовый адрес публичного API RuStore. Пути запросов начинаются с `/public/...`.
pub const DEFAULT_BASE_URL: &str = "https://public-api-m.rustore.ru";

/// Заголовок с токеном доступа RuStore API.
pub const TOKEN_HEADER: &str = "Public-Token";

/// Таймаут запроса по умолчанию (одна попытка, включая чтение тела).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Таймаут запросов с файлом: загрузка сборки (до нескольких гигабайт) и `save_to`.
pub const DEFAULT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Лимит тела ответа по умолчанию, байт.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Максимальный размер тела ошибки (не-2xx), который читается в [`HttpError`].
pub const ERROR_BODY_LIMIT: usize = 8 * 1024;

/// Query-параметр номера страницы (с нуля).
pub const PAGE_PARAM: &str = "page";

/// Код успешного ответа в конверте RuStore.
pub const CODE_OK: &str = "OK";

/// Максимальное число редиректов в пределах того же origin.
const MAX_REDIRECTS: usize = 5;

/// `User-Agent` всех запросов.
pub(crate) const USER_AGENT: &str = concat!("rustore-mcp/", env!("CARGO_PKG_VERSION"));

/// HTTP-метод запроса: тот же тип, что и в таблице операций.
pub use crate::spec::HttpMethod;

impl HttpMethod {
    /// Повтор запроса не меняет результат.
    fn is_idempotent(self) -> bool {
        matches!(self, Self::Get | Self::Put | Self::Delete)
    }

    fn to_reqwest(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Post => Method::POST,
            Self::Put => Method::PUT,
            Self::Patch => Method::PATCH,
            Self::Delete => Method::DELETE,
        }
    }
}

/// Политика повторов при 429 и 503.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Сколько раз повторять запрос после первой попытки. `0` — без повторов.
    pub max_retries: u32,
    /// Пауза, если в ответе нет `Retry-After` (удваивается с каждой попыткой).
    pub default_delay: Duration,
    /// Максимальная пауза. Если `Retry-After` больше, запрос не повторяется, а ошибка
    /// возвращается сразу с [`HttpError::retry_after`].
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            default_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Настройки клиента.
///
/// `Debug` безопасен: источник токенов не раскрывает ни ключ, ни токены.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Базовый адрес API ([`DEFAULT_BASE_URL`]). Может содержать префикс пути, он
    /// сохраняется. Допустимы `https://` и `http://` только для loopback-адресов (тесты).
    pub base_url: url::Url,
    /// Источник токенов (обычно [`crate::access::KeyTokens`] по ключу профиля). `None` —
    /// ключа нет: [`ApiClient::execute`] возвращает [`ApiError::MissingToken`] и ничего не
    /// отправляет.
    pub tokens: Option<Arc<dyn TokenSource>>,
    /// Таймаут одной попытки, включая чтение тела.
    pub timeout: Duration,
    /// Таймаут попытки с файлом (загрузка из файла или `save_to`).
    pub transfer_timeout: Duration,
    /// Максимальный размер тела ответа в байтах. Больший ответ — [`ApiError::ResponseTooLarge`].
    /// На `save_to` не распространяется.
    pub max_response_bytes: usize,
    pub retry: RetryPolicy,
}

impl ClientConfig {
    /// Настройки для публичного API RuStore со значениями по умолчанию.
    pub fn new(tokens: Option<Arc<dyn TokenSource>>) -> Result<Self, ApiError> {
        let base_url = url::Url::parse(DEFAULT_BASE_URL)
            .map_err(|e| ApiError::InvalidConfig(format!("base_url: {e}")))?;
        Ok(Self {
            base_url,
            tokens,
            timeout: DEFAULT_TIMEOUT,
            transfer_timeout: DEFAULT_TRANSFER_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            retry: RetryPolicy::default(),
        })
    }
}

/// Тело запроса.
#[derive(Debug, Clone, Default)]
pub enum RequestBody {
    #[default]
    Empty,
    /// JSON-тело, отправляется как `application/json`.
    ///
    /// Если значение — строка с JSON-объектом или массивом, она разбирается и отправляется
    /// как объект: MCP-клиенты иногда передают тело строкой, а API ждёт объект.
    Json(Value),
    /// `multipart/form-data` (загрузка файлов).
    Multipart(Vec<MultipartPart>),
}

/// Часть multipart-запроса.
#[derive(Debug, Clone)]
pub enum MultipartPart {
    Text {
        name: String,
        value: String,
    },
    /// Файл. `file_name` по умолчанию — имя локального файла (для [`FileSource::Path`]),
    /// для [`FileSource::Bytes`] имя обязательно. `content_type` по умолчанию угадывается
    /// по имени файла, иначе `application/octet-stream`.
    File {
        name: String,
        source: FileSource,
        file_name: Option<String>,
        content_type: Option<String>,
    },
}

/// Содержимое файла для multipart.
///
/// Исполнитель не решает, какие локальные файлы можно читать: путь уже проверен ядром.
#[derive(Clone)]
pub enum FileSource {
    /// Содержимое в памяти.
    Bytes(Vec<u8>),
    /// Локальный файл, читается потоково (заново при каждом повторе запроса) и в память
    /// целиком не загружается: сборки бывают размером в гигабайты.
    ///
    /// Путь — канонический (как его возвращает `canonicalize`): после открытия исполнитель
    /// проверяет, что путь не подменили, иначе [`ApiError::LocalFileChanged`].
    Path(PathBuf),
}

impl fmt::Debug for FileSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Содержимое файла в Debug не выводим: оно может быть большим и личным.
        match self {
            Self::Bytes(bytes) => write!(f, "Bytes({} байт)", bytes.len()),
            Self::Path(path) => f.debug_tuple("Path").field(path).finish(),
        }
    }
}

/// Запрос к API. `path` — относительный путь `/public/...` с уже подставленными
/// параметрами (см. [`render_path`]); query передаётся отдельно в `query`, в том числе
/// `page` и `size` постраничной выдачи.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: RequestBody,
    /// Если задан, тело успешного ответа сохраняется в файл, а не возвращается.
    ///
    /// Путь должен быть абсолютным, каталог — существовать и быть указан каноническим путём,
    /// а файла с таким именем быть не должно: существующие файлы не перезаписываются. После
    /// создания файла проверяется, что каталог не подменили ([`ApiError::LocalFileChanged`]).
    /// При ошибке частично записанный файл удаляется. Лимит `max_response_bytes` к файлу не
    /// применяется.
    pub save_to: Option<PathBuf>,
}

impl ApiRequest {
    /// Запрос без query, тела и `save_to`.
    pub fn new(method: HttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            body: RequestBody::Empty,
            save_to: None,
        }
    }
}

/// Тело ответа.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    Empty,
    /// JSON-ответ. Конверт RuStore снят: здесь его `body` (или весь конверт, если `body`
    /// пустой — тогда в нём остаётся `message`).
    Json(Value),
    /// Текст ответа; для двоичного ответа без `save_to` — пояснение с подсказкой.
    Text(String),
    SavedToFile {
        path: PathBuf,
        bytes: u64,
        content_type: Option<String>,
    },
}

/// Постраничная выдача RuStore: `body` с массивом `content` и полями `pageNumber`,
/// `pageSize`, `totalElements`, `totalPages` (номер страницы — с нуля). Следующая страница
/// запрашивается тем же запросом с query-параметром [`PAGE_PARAM`] = `next_page`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Pagination {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pages: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_elements: Option<u64>,
    /// Номер следующей страницы; нет — это последняя страница.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<u64>,
    /// Имя аргумента, в котором передаётся `next_page` ([`PAGE_PARAM`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_as: Option<&'static str>,
}

impl Pagination {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Постраничная выдача из тела ответа (уже без конверта). Не страница — пустая.
    pub fn from_body(body: &Value) -> Self {
        let Some(object) = body.as_object() else {
            return Self::default();
        };
        if !object.get("content").is_some_and(Value::is_array) {
            return Self::default();
        }
        // Имена полей — как у RuStore; запасные — как у Spring `Page`, на случай другого
        // вида выдачи в отдельных методах.
        let pageable = object.get("pageable");
        let number = |names: &[&str]| {
            names.iter().find_map(|name| {
                object
                    .get(*name)
                    .or_else(|| pageable.and_then(|p| p.get(*name)))
                    .and_then(as_u64)
            })
        };
        let page = number(&["pageNumber", "number", "page"]);
        let page_size = number(&["pageSize", "size"]);
        let total_pages = number(&["totalPages"]);
        let total_elements = number(&["totalElements", "total"]);
        let last = object.get("last").and_then(Value::as_bool);
        let next_page = match (page, total_pages, last) {
            (Some(_), _, Some(true)) => None,
            (Some(page), Some(total), _) => (page + 1 < total).then_some(page + 1),
            (Some(page), None, Some(false)) => Some(page + 1),
            _ => None,
        };
        Self {
            page,
            page_size,
            total_pages,
            total_elements,
            next_page,
            pass_as: next_page.map(|_| PAGE_PARAM),
        }
    }
}

/// Число или строка с числом.
fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Успешный ответ API (статус 2xx и `code` = `OK`).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: ResponseBody,
    pub pagination: Pagination,
}

/// Ошибка RuStore API: статус не 2xx или конверт с `code` не `OK` (тогда `status` может
/// быть и 200).
///
/// RuStore возвращает ошибки в конверте `{"code": "ERROR", "message": "...", "body": null,
/// "timestamp": ...}`; разобранные поля лежат в `code` и `message`, исходный текст
/// (обрезанный до [`ERROR_BODY_LIMIT`]) — в `body`. Токен из текстов вырезан.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub status: u16,
    pub code: Option<String>,
    pub message: Option<String>,
    pub body: String,
    /// Через сколько секунд можно повторить запрос (`Retry-After`), если сервер сообщил.
    pub retry_after: Option<u64>,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RuStore API вернул {}", self.status)?;
        if let Some(reason) = StatusCode::from_u16(self.status)
            .ok()
            .and_then(|s| s.canonical_reason())
        {
            write!(f, " {reason}")?;
        }
        match (&self.code, &self.message) {
            (Some(code), Some(message)) => write!(f, ": {code}: {message}")?,
            (Some(code), None) => write!(f, ": {code}")?,
            (None, Some(message)) => write!(f, ": {message}")?,
            (None, None) => write!(f, ": {}", self.body)?,
        }
        if let Some(secs) = self.retry_after {
            write!(f, " (повторите через {secs} с)")?;
        }
        Ok(())
    }
}

/// Ошибки запроса. Текст ошибок не содержит токен.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("недопустимый путь API: {0}")]
    InvalidPath(String),
    #[error("не хватает параметра пути `{0}`")]
    MissingPathParam(String),
    /// Некорректные аргументы запроса, не связанные с путём (тип содержимого, `save_to`).
    #[error("некорректный запрос: {0}")]
    InvalidRequest(String),
    /// Некорректная конфигурация клиента (`base_url`, токен).
    #[error("некорректная конфигурация клиента RuStore API: {0}")]
    InvalidConfig(String),
    /// У клиента нет источника токенов (см. [`ClientConfig::tokens`]); запрос не
    /// отправлялся.
    #[error("нет ключа RuStore API для запроса")]
    MissingToken,
    /// Не удалось получить токен по ключу; запрос не отправлялся (или не повторялся после
    /// 401). Текст — на английском, из [`TokenError`], без ключа, подписи и токена.
    #[error("не удалось получить токен RuStore API: {0}")]
    TokenUnavailable(String),
    /// RuStore ответил статусом не 2xx или конвертом с ошибкой.
    #[error("{0}")]
    Http(Box<HttpError>),
    /// Тело ответа больше лимита; ответ не возвращается.
    #[error(
        "ответ RuStore API больше {limit} байт; сузьте запрос (size, фильтры) \
         или сохраните ответ в файл (save_to)"
    )]
    ResponseTooLarge { limit: usize },
    /// Сеть, TLS, таймаут.
    #[error("ошибка соединения с RuStore API: {0}")]
    Transport(String),
    #[error("ошибка ввода-вывода: {0}")]
    Io(String),
    /// Локальный файл (для загрузки или `save_to`) подменили после проверки пути: путь
    /// перестал быть каноническим или указывает на другой файл. Файл не отправлен, ответ
    /// в него не записан.
    #[error("локальный файл `{0}` подменён после проверки пути")]
    LocalFileChanged(String),
}

impl ApiError {
    /// HTTP-статус ответа RuStore, если ошибка пришла от API.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http(e) => Some(e.status),
            _ => None,
        }
    }
}

impl From<TokenError> for ApiError {
    fn from(err: TokenError) -> Self {
        Self::TokenUnavailable(err.to_string())
    }
}

/// Клиент RuStore API.
#[derive(Clone)]
pub struct ApiClient {
    config: ClientConfig,
    http: reqwest::Client,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        check_base_url(&config.base_url)?;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, */*;q=0.8"),
        );

        // Редиректы разрешены только в пределах origin из base_url. Внешние редиректы
        // (например, на хранилище файлов) не выполняются: так токен и тело запроса не уходят
        // на чужой хост, а клиент не превращается в прокси. Заголовок `Public-Token` — не
        // `Authorization`, и reqwest сам его при смене хоста не убирает, поэтому это
        // правило обязательно. Остановленный редирект возвращается как ответ 3xx и
        // превращается в `ApiError::Http`.
        let origin = config.base_url.origin();
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("слишком много редиректов")
            } else if attempt.url().origin() == origin {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });

        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .timeout(config.timeout)
            .redirect(policy)
            .build()
            .map_err(|e| ApiError::InvalidConfig(describe_reqwest_error(&e)))?;

        Ok(Self { config, http })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Заменяет известные токены клиента в тексте на `***`. Защита в глубину для текстов,
    /// которые уходят агенту: токен не должен попасть в ответ, даже если API его отразил.
    pub fn scrub_token(&self, text: String) -> String {
        self.redact(text)
    }

    /// Выполняет запрос. Не-2xx ответы и конверты с ошибкой возвращаются как
    /// [`ApiError::Http`].
    pub async fn execute(&self, request: ApiRequest) -> Result<ApiResponse, ApiError> {
        self.execute_with_limit(request, self.config.max_response_bytes)
            .await
    }

    /// Как [`Self::execute`], но тело ответа читается до `max_response_bytes` вместо лимита
    /// из конфигурации.
    ///
    /// Нужен, когда ответ обрабатывается перед возвратом агенту (например, фильтром в ядре
    /// MCP): обработке нужен ответ целиком, а лимит конфигурации вызывающий применяет уже
    /// к результату обработки.
    pub async fn execute_with_limit(
        &self,
        request: ApiRequest,
        max_response_bytes: usize,
    ) -> Result<ApiResponse, ApiError> {
        let Some(tokens) = &self.config.tokens else {
            return Err(ApiError::MissingToken);
        };
        let url = self.build_url(&request.path, &request.query)?;
        if let Some(save_to) = &request.save_to {
            check_save_to(save_to).await?;
        }
        if let RequestBody::Multipart(parts) = &request.body {
            check_multipart(parts)?;
        }
        let transfer = request.save_to.is_some() || has_file(&request.body);
        // Токен — после всех проверок запроса: ключ не подписывает запрос токена ради
        // запроса, который всё равно не уйдёт.
        let mut token = tokens.token().await?;
        let mut auth = token_header(&token)?;
        let token_name = HeaderName::from_static("public-token");

        tracing::debug!(method = request.method.as_str(), path = %request.path, "запрос к RuStore API");

        let retry = self.config.retry;
        let mut attempt: u32 = 0;
        let mut reauthorized = false;
        let response = loop {
            let mut builder = self
                .http
                .request(request.method.to_reqwest(), url.clone())
                .header(token_name.clone(), auth.clone());
            if transfer {
                builder = builder.timeout(self.config.transfer_timeout);
            }
            let builder = attach_body(builder, &request.body).await?;
            let response = builder
                .send()
                .await
                .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?;
            let status = response.status();
            if status.is_success() {
                break response;
            }

            if status == StatusCode::UNAUTHORIZED
                && !reauthorized
                && retry_after_401(request.method, response.url() == &url)
            {
                reauthorized = true;
                tokens.invalidate(&token).await;
                let fresh = tokens.token().await?;
                if fresh != token {
                    tracing::debug!("RuStore API вернул 401, повтор с новым токеном");
                    auth = token_header(&fresh)?;
                    token = fresh;
                    drop(response);
                    continue;
                }
            }

            let retry_after = parse_retry_after(response.headers());
            let retryable = status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::SERVICE_UNAVAILABLE && request.method.is_idempotent());
            if retryable && attempt < retry.max_retries {
                let delay = retry_after.unwrap_or_else(|| backoff(retry.default_delay, attempt));
                if delay <= retry.max_delay {
                    attempt += 1;
                    tracing::debug!(
                        status = status.as_u16(),
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "повтор запроса к RuStore API"
                    );
                    drop(response);
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            return Err(self.http_error(status, retry_after, response).await);
        };

        let status = response.status();
        let body = match &request.save_to {
            Some(path) => {
                let content_type = content_type(&response);
                let bytes = save_to_file(response, path).await?;
                ResponseBody::SavedToFile {
                    path: path.clone(),
                    bytes,
                    content_type,
                }
            }
            None => self.read_body(response, max_response_bytes).await?,
        };
        let body = self.unwrap_envelope(status, body)?;
        let pagination = match &body {
            ResponseBody::Json(value) => Pagination::from_body(value),
            _ => Pagination::default(),
        };
        Ok(ApiResponse {
            status: status.as_u16(),
            body,
            pagination,
        })
    }

    /// Снимает конверт RuStore с JSON-ответа: `code` не `OK` — ошибка, иначе — `body`.
    /// Ответы без конверта возвращаются как есть.
    fn unwrap_envelope(
        &self,
        status: StatusCode,
        body: ResponseBody,
    ) -> Result<ResponseBody, ApiError> {
        let ResponseBody::Json(value) = body else {
            return Ok(body);
        };
        if !is_envelope(&value) {
            return Ok(ResponseBody::Json(value));
        }
        let (code, message) = envelope_error_parts(&value);
        if code.as_deref() != Some(CODE_OK) {
            let full = serde_json::to_string(&value).unwrap_or_default();
            let mut text = lossy_prefix(&full.as_bytes()[..full.len().min(ERROR_BODY_LIMIT)]);
            if full.len() > ERROR_BODY_LIMIT {
                text.push_str(" …(обрезано)");
            }
            return Err(ApiError::Http(Box::new(HttpError {
                status: status.as_u16(),
                code: code.map(|c| self.redact(c)),
                message: message.map(|m| self.redact(m)),
                body: self.redact(text),
                retry_after: None,
            })));
        }
        let Value::Object(mut object) = value else {
            return Ok(ResponseBody::Empty);
        };
        match object.remove("body") {
            Some(Value::Null) | None => Ok(ResponseBody::Json(Value::Object(object))),
            Some(inner) => Ok(ResponseBody::Json(inner)),
        }
    }

    /// Собирает URL: префикс пути из `base_url` + проверенный относительный путь + query.
    fn build_url(&self, path: &str, query: &[(String, String)]) -> Result<url::Url, ApiError> {
        validate_relative_path(path)?;

        let mut url = self.config.base_url.clone();
        let prefix = url.path().trim_end_matches('/').to_string();
        // Путь уже состоит только из pchar и `%XX`, поэтому set_path его не меняет.
        url.set_path(&format!("{prefix}{path}"));
        url.set_query(None);
        url.set_fragment(None);
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }

        // Повторная проверка: итоговый адрес обязан остаться в origin базового URL.
        if url.origin() != self.config.base_url.origin() {
            return Err(ApiError::InvalidPath(
                "путь выводит за пределы RuStore API".into(),
            ));
        }
        Ok(url)
    }

    async fn http_error(
        &self,
        status: StatusCode,
        retry_after: Option<Duration>,
        mut response: reqwest::Response,
    ) -> ApiError {
        let mut error = HttpError {
            status: status.as_u16(),
            code: None,
            message: None,
            body: String::new(),
            retry_after: retry_after.map(|d| d.as_secs()),
        };
        if status.is_redirection() {
            // Location не возвращаем: там может быть подписанная ссылка на внешнее хранилище.
            error.body = "редирект на другой адрес не выполняется".to_string();
            return ApiError::Http(Box::new(error));
        }
        match read_limited(&mut response, ERROR_BODY_LIMIT).await {
            Ok((bytes, truncated)) => {
                if !truncated && let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    let (code, message) = envelope_error_parts(&value);
                    error.code = code.map(|c| self.redact(c));
                    error.message = message.map(|m| self.redact(m));
                }
                let mut text = lossy_prefix(&bytes).trim().to_string();
                if truncated {
                    text.push_str(" …(обрезано)");
                }
                if text.is_empty() {
                    text = status
                        .canonical_reason()
                        .unwrap_or("пустой ответ")
                        .to_string();
                }
                error.body = self.redact(text);
            }
            Err(e) => error.body = format!("не удалось прочитать тело ошибки: {e}"),
        }
        ApiError::Http(Box::new(error))
    }

    async fn read_body(
        &self,
        mut response: reqwest::Response,
        limit: usize,
    ) -> Result<ResponseBody, ApiError> {
        let content_type = content_type(&response);
        let content_length = response.content_length();

        // Заведомо двоичный ответ не читаем вовсе — сразу подсказываем `save_to`.
        let declared_binary = content_type
            .as_deref()
            .filter(|ct| is_binary_content_type(ct));
        if let Some(ct) = declared_binary
            && content_length != Some(0)
        {
            return Ok(ResponseBody::Text(binary_note(Some(ct), content_length)));
        }
        if content_length.is_some_and(|len| len > limit as u64) {
            return Err(ApiError::ResponseTooLarge { limit });
        }

        let (bytes, truncated) = read_limited(&mut response, limit).await?;
        if bytes.is_empty() {
            return Ok(ResponseBody::Empty);
        }
        if looks_binary(&bytes, truncated) {
            return Ok(ResponseBody::Text(binary_note(
                content_type.as_deref(),
                content_length,
            )));
        }
        if truncated {
            return Err(ApiError::ResponseTooLarge { limit });
        }

        let json_declared = content_type.as_deref().is_some_and(is_json_content_type);
        let json_allowed = json_declared
            || content_type
                .as_deref()
                .is_none_or(|ct| ct.starts_with("text/plain"));
        if json_allowed && let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            return Ok(ResponseBody::Json(value));
        }
        Ok(ResponseBody::Text(self.redact(lossy_prefix(&bytes))))
    }

    /// Защита в глубину: если сервер вдруг отразил токен в тексте, вырезаем его вместе с
    /// недавними прежними токенами (например, отклонённым в ответе 401).
    fn redact(&self, mut text: String) -> String {
        let tokens = self
            .config
            .tokens
            .as_ref()
            .map(|tokens| tokens.known_tokens())
            .unwrap_or_default();
        for token in &tokens {
            let token = token.expose();
            if !token.is_empty() && text.contains(token) {
                text = text.replace(token, "***");
            }
        }
        text
    }
}

/// Можно ли повторить запрос с новым токеном после ответа 401.
///
/// Ответ 401 RuStore возвращает на этапе проверки токена, до выполнения операции: токен
/// проверяется раньше, чем запрос попадает в обработчик, поэтому 401 означает, что запрос
/// ничего не изменил. Это верно для запроса, на который пришёл ответ, — но не для цепочки
/// редиректов: если первый запрос выполнился и ответил редиректом (например, `303` после
/// `POST`), а 401 пришёл уже на адрес редиректа, то исходный запрос мог изменить данные.
/// Поэтому идемпотентные методы повторяются всегда, а остальные (`POST`, `PATCH`) — только
/// если 401 пришёл на сам запрос, без редиректов (`same_url`).
fn retry_after_401(method: HttpMethod, same_url: bool) -> bool {
    method.is_idempotent() || same_url
}

/// Заголовок `Public-Token: <токен>`, помеченный как sensitive.
fn token_header(token: &AccessToken) -> Result<HeaderValue, ApiError> {
    let value = token.expose();
    if value.trim().is_empty() {
        return Err(ApiError::InvalidConfig("пустой токен".into()));
    }
    let mut header = HeaderValue::from_str(value).map_err(|_| {
        ApiError::InvalidConfig("токен содержит недопустимые для заголовка символы".into())
    })?;
    header.set_sensitive(true);
    Ok(header)
}

/// Конверт RuStore: объект со строкой `code` и хотя бы одним из полей `body`, `message`,
/// `timestamp`.
fn is_envelope(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.get("code").is_some_and(Value::is_string)
            && ["body", "message", "timestamp"]
                .iter()
                .any(|key| object.contains_key(*key))
    })
}

/// `code` и `message` ответа RuStore. Для ответов без конверта (например, ошибки шлюза в
/// виде `{"status", "error", "message"}`) `message` берётся из `message` или `error`.
pub(crate) fn envelope_error_parts(value: &Value) -> (Option<String>, Option<String>) {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let code = text("code");
    let message = text("message").or_else(|| text("error"));
    (code, message)
}

/// Есть ли в теле файл (тогда действует `transfer_timeout`).
fn has_file(body: &RequestBody) -> bool {
    matches!(body, RequestBody::Multipart(parts)
        if parts.iter().any(|p| matches!(p, MultipartPart::File { .. })))
}

pub(crate) fn check_base_url(base: &url::Url) -> Result<(), ApiError> {
    if base.cannot_be_a_base() || base.host().is_none() {
        return Err(ApiError::InvalidConfig(format!(
            "base_url `{base}` не является адресом HTTP API"
        )));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(ApiError::InvalidConfig(
            "base_url не должен содержать логин или пароль".into(),
        ));
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(ApiError::InvalidConfig(
            "base_url не должен содержать query или fragment".into(),
        ));
    }
    match base.scheme() {
        "https" => Ok(()),
        // Без TLS токен ушёл бы открытым текстом, поэтому http — только для локальных адресов.
        "http" if is_loopback(base) => Ok(()),
        "http" => Err(ApiError::InvalidConfig(
            "http допускается только для localhost; используйте https".into(),
        )),
        other => Err(ApiError::InvalidConfig(format!(
            "схема `{other}` не поддерживается"
        ))),
    }
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Пауза без `Retry-After`: `default_delay * 2^attempt`.
fn backoff(default_delay: Duration, attempt: u32) -> Duration {
    default_delay.saturating_mul(1u32 << attempt.min(16))
}

/// `Retry-After` в секундах. Формат HTTP-date не поддерживается: тогда пауза по умолчанию.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    header_str(headers, "retry-after")
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

fn content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
}

/// Проверяет multipart до отправки: тип содержимого и имя файла для содержимого в памяти.
fn check_multipart(parts: &[MultipartPart]) -> Result<(), ApiError> {
    for part in parts {
        if let MultipartPart::File {
            source,
            file_name,
            content_type,
            ..
        } = part
        {
            if matches!(source, FileSource::Bytes(_))
                && file_name.as_deref().is_none_or(str::is_empty)
            {
                return Err(ApiError::InvalidRequest(
                    "для файла из памяти нужно имя файла".into(),
                ));
            }
            if let Some(ct) = content_type {
                parse_mime(ct)?;
            }
        }
    }
    Ok(())
}

async fn attach_body(
    builder: reqwest::RequestBuilder,
    body: &RequestBody,
) -> Result<reqwest::RequestBuilder, ApiError> {
    Ok(match body {
        RequestBody::Empty => builder,
        RequestBody::Json(value) => builder.json(&normalize_json_body(value.clone())),
        RequestBody::Multipart(parts) => {
            let mut form = multipart::Form::new();
            for part in parts {
                form = match part {
                    MultipartPart::Text { name, value } => form.text(name.clone(), value.clone()),
                    MultipartPart::File {
                        name,
                        source,
                        file_name,
                        content_type,
                    } => {
                        let mut file_part = match source {
                            FileSource::Path(path) => file_part(path).await?,
                            FileSource::Bytes(bytes) => multipart::Part::bytes(bytes.clone()),
                        };
                        if let Some(file_name) = file_name {
                            file_part = file_part.file_name(sanitize_file_name(file_name));
                        }
                        let mime = match content_type {
                            Some(ct) => Some(parse_mime(ct)?),
                            // Для Path тип уже угадан по расширению локального файла.
                            None if matches!(source, FileSource::Bytes(_)) => file_name
                                .as_deref()
                                .map(|n| mime_guess::from_path(n).first_or_octet_stream()),
                            None => None,
                        };
                        if let Some(mime) = mime {
                            file_part = file_part.mime_str(mime.as_ref()).map_err(|_| {
                                ApiError::InvalidRequest(format!(
                                    "некорректный тип содержимого `{mime}`"
                                ))
                            })?;
                        }
                        form.part(name.clone(), file_part)
                    }
                };
            }
            builder.multipart(form)
        }
    })
}

/// Часть multipart из локального файла: как `Part::file` (потоковое чтение, имя файла и тип
/// по расширению), но файл открывается один раз и проверяется [`open_verified`], а
/// содержимое читается из уже проверенного дескриптора, а не по пути заново.
async fn file_part(path: &Path) -> Result<multipart::Part, ApiError> {
    let file = open_verified(path).await?;
    let len = file
        .metadata()
        .await
        .map_err(|e| io_error("не удалось прочитать файл", path, &e))?
        .len();
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut part = multipart::Part::stream_with_length(file, len)
        .mime_str(mime.as_ref())
        .map_err(|_| ApiError::InvalidRequest(format!("некорректный тип содержимого `{mime}`")))?;
    if let Some(name) = path.file_name() {
        part = part.file_name(name.to_string_lossy().into_owned());
    }
    Ok(part)
}

/// Открывает локальный файл для отправки и проверяет, что открыт именно файл по этому пути.
///
/// Путь проверяет ядро (разрешённые каталоги) и передаёт канонический — без символических
/// ссылок и `..`. Между проверкой и открытием путь могут подменить (заменить файл или
/// каталог на символическую ссылку наружу), поэтому после открытия сверяется, что путь
/// по-прежнему канонический и указывает на тот же файл, что открытый дескриптор
/// ([`ensure_same_file`]). Иначе — [`ApiError::LocalFileChanged`], запрос не отправляется.
async fn open_verified(path: &Path) -> Result<tokio::fs::File, ApiError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| io_error("не удалось открыть файл", path, &e))?;
    let opened = file
        .metadata()
        .await
        .map_err(|e| io_error("не удалось прочитать файл", path, &e))?;
    if !opened.is_file() {
        return Err(ApiError::InvalidRequest(format!(
            "`{}` не обычный файл",
            path.display()
        )));
    }
    ensure_same_file(path, &opened).await?;
    Ok(file)
}

/// Путь `path` канонический и сейчас указывает на тот же файл, что `opened` (метаданные
/// открытого дескриптора). См. [`open_verified`] и [`save_to_file`].
async fn ensure_same_file(path: &Path, opened: &std::fs::Metadata) -> Result<(), ApiError> {
    let changed = || ApiError::LocalFileChanged(path.display().to_string());
    let canonical = tokio::fs::canonicalize(path).await.map_err(|_| changed())?;
    if canonical != path {
        return Err(changed());
    }
    let current = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| changed())?;
    if current.file_type().is_symlink() || !same_file(&current, opened) {
        return Err(changed());
    }
    Ok(())
}

/// Один и тот же файл: устройство и inode.
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Без inode (Windows) сравниваются тип, размер и время изменения: подмену на другой файл
/// это обнаруживает в большинстве случаев, а символическую ссылку отсекает проверка
/// канонического пути.
#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.is_file() == b.is_file() && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

/// Управляющие символы в имени файла заменяются на `_`: reqwest экранирует их обратной
/// косой, но перевод строки остаётся и мог бы разорвать заголовок части.
fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

/// Регрессия: строка с JSON-объектом или массивом отправляется как объект,
/// а не как JSON-строка с экранированным JSON внутри.
fn normalize_json_body(value: Value) -> Value {
    if let Value::String(text) = &value {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) if parsed.is_object() || parsed.is_array() => return parsed,
                _ => {}
            }
        }
    }
    value
}

fn parse_mime(ct: &str) -> Result<mime_guess::mime::Mime, ApiError> {
    ct.parse()
        .map_err(|_| ApiError::InvalidRequest(format!("некорректный тип содержимого `{ct}`")))
}

/// Проверяет `save_to` до отправки запроса, чтобы не выполнять запрос впустую.
async fn check_save_to(path: &Path) -> Result<(), ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::InvalidRequest(format!(
            "save_to должен быть абсолютным путём: `{}`",
            path.display()
        )));
    }
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Err(ApiError::InvalidRequest(format!(
            "save_to должен указывать на файл: `{}`",
            path.display()
        )));
    };
    match tokio::fs::metadata(parent).await {
        Ok(meta) if meta.is_dir() => {}
        _ => {
            return Err(ApiError::InvalidRequest(format!(
                "каталог для save_to не существует: `{}`",
                parent.display()
            )));
        }
    }
    // Каталог — канонический путь (так его передаёт ядро после проверки разрешённых
    // каталогов): после создания файла это сверяется ещё раз (см. `save_to_file`).
    if tokio::fs::canonicalize(parent).await.ok().as_deref() != Some(parent) {
        return Err(ApiError::InvalidRequest(format!(
            "каталог save_to должен быть указан каноническим путём, без символических ссылок \
             и `..`: `{}`",
            parent.display()
        )));
    }
    if tokio::fs::symlink_metadata(path).await.is_ok() {
        return Err(ApiError::InvalidRequest(format!(
            "файл уже существует, перезапись запрещена: `{}`",
            path.display()
        )));
    }
    Ok(())
}

async fn save_to_file(mut response: reqwest::Response, path: &Path) -> Result<u64, ApiError> {
    // create_new: не перезаписываем существующий файл и не идём по симлинку,
    // даже если он появился после check_save_to.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| io_error("не удалось создать файл", path, &e))?;
    // Каталог могли подменить символической ссылкой между проверкой и созданием файла:
    // тогда файл создан не там, где проверяли. Ничего в него не пишем и не удаляем по пути
    // (путь теперь ведёт в другое место); пустой файл остаётся, данные — нет.
    let created = file
        .metadata()
        .await
        .map_err(|e| io_error("не удалось прочитать файл", path, &e))?;
    ensure_same_file(path, &created).await?;

    let result = async {
        let mut written: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| io_error("ошибка записи в файл", path, &e))?;
            written += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| io_error("ошибка записи в файл", path, &e))?;
        Ok(written)
    }
    .await;

    if result.is_err() {
        drop(file);
        // Частично записанный файл бесполезен и вводит в заблуждение — удаляем.
        let _ = tokio::fs::remove_file(path).await;
    }
    result
}

/// Читает тело не больше `limit` байт. Возвращает прочитанное и признак обрезки.
pub(crate) async fn read_limited(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), ApiError> {
    let mut buf = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ApiError::Transport(describe_reqwest_error(&e)))?
    {
        let room = limit.saturating_sub(buf.len());
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            // Остаток не дочитываем: соединение закроется при drop ответа.
            return Ok((buf, true));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, false))
}

/// UTF-8 текст из байтов; незавершённый символ в конце (после обрезки) отбрасывается.
fn lossy_prefix(bytes: &[u8]) -> String {
    let end = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => bytes.len(),
    };
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn is_json_content_type(ct: &str) -> bool {
    let essence = ct.split(';').next().unwrap_or("").trim();
    essence == "application/json" || essence.ends_with("+json")
}

fn is_text_content_type(ct: &str) -> bool {
    let essence = ct.split(';').next().unwrap_or("").trim();
    essence.starts_with("text/")
        || is_json_content_type(essence)
        || essence.ends_with("+xml")
        || matches!(
            essence,
            "application/xml"
                | "application/javascript"
                | "application/x-www-form-urlencoded"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-ndjson"
        )
}

fn is_binary_content_type(ct: &str) -> bool {
    !ct.is_empty() && !is_text_content_type(ct)
}

/// Эвристика для ответов без явного текстового типа: NUL или невалидный UTF-8.
fn looks_binary(bytes: &[u8], truncated: bool) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => false,
        // Обрезка могла разрезать многобайтовый символ — это не признак двоичных данных.
        Err(e) => !(truncated && e.error_len().is_none()),
    }
}

fn binary_note(content_type: Option<&str>, length: Option<u64>) -> String {
    let ct = content_type.unwrap_or("тип не указан");
    let size = match length {
        Some(len) => format!("{len} байт"),
        None => "размер неизвестен".to_string(),
    };
    format!(
        "Ответ двоичный ({ct}, {size}) и не возвращается текстом. \
         Повторите запрос с save_to, чтобы сохранить его в файл."
    )
}

fn io_error(action: &str, path: &Path, err: &std::io::Error) -> ApiError {
    ApiError::Io(format!("{action} `{}`: {err}", path.display()))
}

/// Безопасное описание ошибки reqwest: без URL (там может быть query) и с причинами.
pub(crate) fn describe_reqwest_error(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "превышен таймаут"
    } else if err.is_connect() {
        "не удалось подключиться"
    } else if err.is_redirect() {
        "ошибка редиректа"
    } else if err.is_body() || err.is_decode() {
        "ошибка чтения тела ответа"
    } else if err.is_builder() {
        "некорректный запрос"
    } else {
        "ошибка запроса"
    };
    let mut text = kind.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Подмена файла после открытия (гонка между проверкой пути и чтением) обнаруживается.
    #[tokio::test]
    async fn replaced_files_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let path = base.join("upload.txt");
        std::fs::write(&path, "original").unwrap();

        let file = open_verified(&path).await.unwrap();
        let opened = file.metadata().await.unwrap();
        ensure_same_file(&path, &opened).await.unwrap();

        // Путь теперь ведёт к другому файлу.
        let other = base.join("other.txt");
        std::fs::write(&other, "other content").unwrap();
        std::fs::rename(&other, &path).unwrap();
        let err = ensure_same_file(&path, &opened).await.unwrap_err();
        assert!(matches!(err, ApiError::LocalFileChanged(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let inside = base.join("inside");
        std::fs::create_dir(&inside).unwrap();

        // Сам файл — ссылка наружу.
        let link = inside.join("link.txt");
        std::os::unix::fs::symlink(outside.join("secret.txt"), &link).unwrap();
        let err = open_verified(&link).await.unwrap_err();
        assert!(matches!(err, ApiError::LocalFileChanged(_)), "{err:?}");

        // Каталог в пути — ссылка наружу: и чтение, и запись отклоняются.
        let dir_link = inside.join("out");
        std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
        let err = open_verified(&dir_link.join("secret.txt"))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::LocalFileChanged(_)), "{err:?}");
        let err = check_save_to(&dir_link.join("new.bin")).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
        assert!(!outside.join("new.bin").exists());

        // Каталог подменили ссылкой после проверки (между check_save_to и созданием файла):
        // файл создаётся по ссылке, но данные в него не пишутся.
        let target_dir = inside.join("target");
        std::fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("new.bin");
        check_save_to(&target).await.unwrap();
        std::fs::remove_dir(&target_dir).unwrap();
        std::os::unix::fs::symlink(&outside, &target_dir).unwrap();
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .await
            .unwrap();
        let created = file.metadata().await.unwrap();
        let err = ensure_same_file(&target, &created).await.unwrap_err();
        assert!(matches!(err, ApiError::LocalFileChanged(_)), "{err:?}");
    }

    #[test]
    fn normalize_parses_json_strings() {
        assert_eq!(
            normalize_json_body(Value::String(r#"{"a":1}"#.into())),
            serde_json::json!({"a": 1})
        );
        assert_eq!(
            normalize_json_body(Value::String(" [1,2]".into())),
            serde_json::json!([1, 2])
        );
        assert_eq!(
            normalize_json_body(Value::String("plain".into())),
            Value::String("plain".into())
        );
        assert_eq!(
            normalize_json_body(Value::String("{broken".into())),
            Value::String("{broken".into())
        );
    }

    #[test]
    fn lossy_prefix_drops_cut_char() {
        let text = "ab\u{0436}";
        let bytes = text.as_bytes();
        assert_eq!(lossy_prefix(&bytes[..bytes.len() - 1]), "ab");
        assert_eq!(lossy_prefix(bytes), text);
    }

    #[test]
    fn content_type_classification() {
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("application/vnd.api+json"));
        assert!(is_text_content_type("text/plain"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(is_binary_content_type("image/png"));
        assert!(!is_binary_content_type("application/xml"));
    }

    #[test]
    fn base_url_rules() {
        let ok = |s: &str| check_base_url(&url::Url::parse(s).unwrap()).is_ok();
        assert!(ok(DEFAULT_BASE_URL));
        assert!(ok("https://api.example/prefix/"));
        assert!(ok("http://127.0.0.1:8080"));
        assert!(ok("http://localhost:8080/api"));
        assert!(ok("http://[::1]:8080"));
        assert!(!ok("http://public-api-m.rustore.ru"));
        assert!(!ok("https://user:pass@api.example"));
        assert!(!ok("https://api.example/?x=1"));
        assert!(!ok("ftp://api.example"));
        assert!(!ok("mailto:a@b"));
    }

    #[test]
    fn file_name_control_chars_replaced() {
        assert_eq!(sanitize_file_name("a\r\nb\"c.txt"), "a__b\"c.txt");
    }

    #[test]
    fn backoff_doubles() {
        let base = Duration::from_millis(100);
        assert_eq!(backoff(base, 0), base);
        assert_eq!(backoff(base, 2), Duration::from_millis(400));
    }

    #[test]
    fn envelopes_are_recognized() {
        assert!(is_envelope(&serde_json::json!({"code": "OK", "body": {}})));
        assert!(is_envelope(
            &serde_json::json!({"code": "ERROR", "message": "x"})
        ));
        assert!(!is_envelope(&serde_json::json!({"code": "OK"})));
        assert!(!is_envelope(&serde_json::json!({"code": 1, "body": {}})));
        assert!(!is_envelope(&serde_json::json!([1])));
        assert_eq!(
            envelope_error_parts(&serde_json::json!({"code": "ERROR", "message": " bad "})),
            (Some("ERROR".into()), Some("bad".into()))
        );
        assert_eq!(
            envelope_error_parts(&serde_json::json!({"status": 404, "error": "Not Found"})),
            (None, Some("Not Found".into()))
        );
    }

    #[test]
    fn pagination_from_page_bodies() {
        let page = |body: Value| Pagination::from_body(&body);
        let first = page(serde_json::json!({
            "content": [1, 2], "pageNumber": 0, "pageSize": 2, "totalElements": 5, "totalPages": 3
        }));
        assert_eq!(first.next_page, Some(1));
        assert_eq!(first.pass_as, Some(PAGE_PARAM));
        assert_eq!(first.total_elements, Some(5));
        let last = page(serde_json::json!({
            "content": [5], "pageNumber": 2, "pageSize": 2, "totalPages": 3
        }));
        assert_eq!(last.next_page, None);
        assert_eq!(last.pass_as, None);
        assert!(!last.is_empty());
        // Spring `Page`: number, size, last.
        let spring =
            page(serde_json::json!({"content": [], "number": 1, "size": 10, "last": false}));
        assert_eq!(spring.next_page, Some(2));
        let spring_last =
            page(serde_json::json!({"content": [], "number": 1, "totalPages": 9, "last": true}));
        assert_eq!(spring_last.next_page, None);
        // Не страница.
        assert!(page(serde_json::json!({"items": []})).is_empty());
        assert!(page(serde_json::json!([1, 2])).is_empty());
    }
}
