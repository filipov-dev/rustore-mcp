//! Ошибки вызова инструмента.
//!
//! Любая ошибка вызова возвращается MCP-клиенту как `CallToolResult` с `isError: true`
//! и текстом для агента на английском: агент должен понять, что исправить в аргументах
//! или что попросить у пользователя. Ключ и токен в тексты ошибок не попадают: [`ApiError`]
//! их не содержит, аргументы инструмента их не принимают, а сервер дополнительно вырезает
//! известные токены клиента из итогового текста.

use std::fmt;

use rustore_api::executor::{ApiError, HttpError};
use rustore_api::spec::{ApiService, OperationDef};

/// Ошибка вызова инструмента с текстом для агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

impl From<ApiError> for ToolError {
    fn from(err: ApiError) -> Self {
        Self(describe_api_error(&err))
    }
}

/// Текст ошибки исполнителя запросов для агента.
///
/// Тексты `ApiError` написаны для логов на русском, поэтому здесь своя формулировка на
/// английском; подробности от RuStore (`code`, `message`, тело ответа) добавляются как есть.
pub fn describe_api_error(err: &ApiError) -> String {
    describe_api_error_for(ApiService::PublicApi, err)
}

/// [`describe_api_error`] для запроса к сервису `service`: у Push API свой секрет и свои
/// ошибки, поэтому и подсказки к статусам свои.
pub fn describe_api_error_for(service: ApiService, err: &ApiError) -> String {
    match err {
        ApiError::Http(http) if service.is_push() => describe_push_http_error(http),
        ApiError::Http(http) => describe_http_error(http),
        ApiError::MissingPathParam(name) => format!("missing path argument `{name}`"),
        ApiError::InvalidPath(_) => "an identifier argument does not form a valid API path: \
             empty values and values like `.` or `..` are rejected"
            .to_owned(),
        ApiError::InvalidRequest(detail) => format!("invalid request: {detail}"),
        ApiError::InvalidConfig(detail) => {
            // Подробности — для лога: они на русском и относятся к настройке сервера.
            tracing::warn!(%detail, "некорректная конфигурация клиента RuStore API");
            "the server is misconfigured (invalid token or API address); ask the user to check \
             the server log and the profile settings"
                .to_owned()
        }
        ApiError::MissingToken => {
            "no RuStore API key is configured for this server, so the request was not sent"
                .to_owned()
        }
        ApiError::PushNotConfigured => "push notifications are not configured for this server \
             (no push project ID and service token in the profile), so nothing was sent. Ask \
             the user to store them with `rustore-mcp auth push login --profile <profile> \
             --project-id <ID> --token-file <file>` (both are in the RuStore console: the app > \
             Push notifications > Projects) and to restart the server. Never ask for the \
             service token in the chat"
            .to_owned(),
        ApiError::PushMessageTooLarge { size, limit } => format!(
            "the push message is {size} bytes (compact JSON of `body.message`), more than the \
             Push API limit of {limit} bytes, so it was not sent. Shorten the texts or the \
             `data` values"
        ),
        // Текст `TokenError` уже на английском и с подсказкой (ключ, часы, консоль RuStore).
        ApiError::TokenUnavailable(detail) => format!(
            "could not get a RuStore API access token with the server's key, so the request \
             was not sent. Ask the user to check the key as described in the server \
             instructions. Details: {detail}"
        ),
        ApiError::ResponseTooLarge { limit } => format!(
            "the RuStore API response is larger than {limit} bytes and was not returned. Narrow \
             the request: use a smaller page (`size`) or stricter filters, or select less with \
             `jq`"
        ),
        ApiError::Transport(detail) => format!(
            "could not reach the RuStore API (network error or timeout); retry later. \
             Details: {detail}"
        ),
        ApiError::Io(detail) => format!("local file error: {detail}"),
        ApiError::LocalFileChanged(path) => format!(
            "local file `{path}` was replaced after its path was checked (for example, it or a \
             parent directory became a symbolic link), so it was not used and nothing was \
             written to it. Use a regular file inside the allowed directories"
        ),
    }
}

/// Текст ошибки исполнителя для агента с подсказкой о правах ключа, когда операция известна.
///
/// Права ключа RuStore API задаются в консоли по методам и по приложениям, а новые методы и
/// приложения в уже созданный ключ не добавляются. На 401 и 403 конкретного метода агент
/// получает имя метода (как в справочнике и в консоли), раздел и приложение из пути запроса
/// (`packageName` или `appId`): пользователю остаётся отметить их у ключа.
///
/// У Push API свои секрет и подсказки ([`describe_api_error_for`]): права ключа RuStore API
/// к нему не относятся, поэтому подсказка о правах — только для Public API.
pub fn describe_api_error_with_hint(
    err: &ApiError,
    service: ApiService,
    op: Option<&OperationDef>,
    path: &str,
) -> String {
    let mut text = describe_api_error_for(service, err);
    if service.is_push() {
        return text;
    }
    if let (Some(status @ (401 | 403)), Some(op)) = (err.status(), op) {
        text.push(' ');
        text.push_str(&permission_hint(status, op, path));
    }
    text
}

/// Подсказка об отказе по правам для операции `op` с путём запроса `path`.
pub fn permission_hint(status: u16, op: &OperationDef, path: &str) -> String {
    let section = rustore_api::spec::find_toolset(op.toolset)
        .map(|toolset| toolset.tag)
        .unwrap_or(op.toolset);
    let app = app_in_path(op.path, path);
    let prefix = if status == 401 {
        "If other tools work with this key, the key"
    } else {
        "The key"
    };
    let app_part = match &app {
        Some(app) => format!(
            ", or the key has no access to the app `{app}` (apps added to the company after the \
             key was created are not added to it)"
        ),
        None => String::new(),
    };
    format!(
        "{prefix} may lack the method \"{summary}\" (section \"{section}\", tool `{tool}`){app_part}. \
         Methods and apps are ticked per key in the RuStore console, Company > RuStore API: ask \
         the user to tick them for the key or to create a new key with them. `auth_check` (or \
         `rustore-mcp auth check`) shows which sections and apps the key can use.",
        summary = op.summary,
        tool = op.tool_name,
    )
}

/// Приложение из пути запроса: значение сегмента `{packageName}` или `{appId}` шаблона.
fn app_in_path(template: &str, path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    template
        .split('/')
        .zip(path.split('/'))
        .find_map(|(pattern, value)| {
            matches!(pattern, "{packageName}" | "{appId}")
                .then(|| percent_encoding::percent_decode_str(value).decode_utf8_lossy())
                .filter(|value| !value.is_empty())
                .map(|value| value.into_owned())
        })
}

/// Текст ошибки Push API: статус, подсказка и `status`/`message` ответа.
fn describe_push_http_error(err: &HttpError) -> String {
    let hint = match err.status {
        300..=399 => " (a redirect to another host was not followed)",
        400 => " (the request was rejected: check the message against the tool schema)",
        401 | 403 => {
            " (the push service token was rejected: it may be deleted or belong to another \
             push project; ask the user to store a valid one with `rustore-mcp auth push login` \
             and to restart the server)"
        }
        404 => {
            " (NOT_FOUND: the device push token is invalid or expired; the app gets a new one \
             from the push SDK. For a topic, check its name)"
        }
        429 => " (the push sending limit is exceeded: retry later)",
        500..=599 => " (Push API server error: retry later)",
        _ => "",
    };
    let mut text = format!("RuStore Push API returned HTTP {}{hint}.", err.status);
    append_details(&mut text, err);
    text
}

/// `code`/`message` ответа, иначе тело, и `Retry-After`.
fn append_details(text: &mut String, err: &HttpError) {
    let code = err.code.as_deref().map(str::trim).filter(|c| !c.is_empty());
    let message = err
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match (code, message) {
        (Some(code), Some(message)) => text.push_str(&format!(" Error {code}: {message}")),
        (None, Some(message)) => text.push_str(&format!(" Error: {message}")),
        (Some(code), None) => text.push_str(&format!(" Error code: {code}")),
        (None, None) if !err.body.trim().is_empty() => {
            text.push_str(" Response body: ");
            text.push_str(err.body.trim());
        }
        (None, None) => {}
    }
    if let Some(secs) = err.retry_after {
        text.push_str(&format!(" Retry after {secs} s."));
    }
}

/// Текст ошибки RuStore: статус, подсказка и `code`/`message` RuStore.
fn describe_http_error(err: &HttpError) -> String {
    let status = err.status;
    let hint = match status {
        // Конверт с `code` не `OK` при HTTP 2xx.
        200..=299 => " (RuStore reported an error in the response)",
        300..=399 => {
            " (a redirect outside the RuStore API was not followed; the resource is probably \
             served from another host)"
        }
        400 | 422 => " (the request was rejected: check the arguments against the tool schema)",
        // Как заменить ключ, зависит от режима сервера (хранилище или ключ сервера), поэтому
        // подробности — в инструкциях сервера.
        401 => {
            " (the access token or the key was rejected: the key may be deleted or expired; ask \
             the user to replace it as described in the server instructions)"
        }
        403 => {
            " (access denied: the RuStore API key has no permission for this method or this \
             app; ask the user to tick them for the key in the RuStore console, Company > \
             RuStore API)"
        }
        404 => {
            " (not found: check the package name and IDs, and that the key has access to the app)"
        }
        409 => " (conflict with the current state: for example, a draft already exists)",
        429 => " (rate limited: retry later)",
        500..=599 => " (RuStore server error: retry later)",
        _ => "",
    };
    let mut text = format!("RuStore API returned HTTP {status}{hint}.");
    append_details(&mut text, err);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16, code: Option<&str>, message: Option<&str>, body: &str) -> ApiError {
        ApiError::Http(Box::new(HttpError {
            status,
            code: code.map(str::to_owned),
            message: message.map(str::to_owned),
            body: body.to_owned(),
            retry_after: None,
        }))
    }

    #[test]
    fn push_errors_get_push_hints() {
        let err = http(403, Some("PERMISSION_DENIED"), Some("bad token"), "");
        let text = describe_api_error_for(ApiService::Push, &err);
        assert!(text.contains("Push API returned HTTP 403"), "{text}");
        assert!(text.contains("auth push login"), "{text}");
        assert!(
            text.contains("Error PERMISSION_DENIED: bad token"),
            "{text}"
        );
        // Для Public API у 403 — подсказка про права ключа, а не про push.
        assert!(!describe_api_error(&err).contains("push"));

        let err = http(404, Some("NOT_FOUND"), None, "");
        let text = describe_api_error_for(ApiService::PushTopics, &err);
        assert!(
            text.contains("device push token is invalid or expired"),
            "{text}"
        );

        let text = describe_api_error(&ApiError::PushNotConfigured);
        assert!(text.contains("auth push login"), "{text}");
        assert!(text.contains("nothing was sent"), "{text}");
        let text = describe_api_error(&ApiError::PushMessageTooLarge {
            size: 5000,
            limit: 4096,
        });
        assert!(
            text.contains("5000 bytes") && text.contains("4096"),
            "{text}"
        );
    }

    #[test]
    fn http_errors_have_status_hint_and_details() {
        let text = describe_api_error(&http(
            404,
            Some("ERROR"),
            Some("Application not found"),
            r#"{"code":"ERROR","message":"Application not found"}"#,
        ));
        assert!(text.contains("HTTP 404"), "{text}");
        assert!(text.contains("not found: check the package name"), "{text}");
        assert!(
            text.contains("Error ERROR: Application not found"),
            "{text}"
        );
        assert!(!text.contains("Response body"), "{text}");

        let text = describe_api_error(&http(200, Some("ERROR"), Some("Draft exists"), "{}"));
        assert!(
            text.contains("HTTP 200 (RuStore reported an error"),
            "{text}"
        );

        let text = describe_api_error(&http(401, None, None, " "));
        assert!(text.contains("server instructions"), "{text}");
        assert!(!text.contains("Response body"), "{text}");

        let text = describe_api_error(&http(403, None, Some("Access denied"), ""));
        assert!(text.contains("Company > RuStore API"), "{text}");
        assert!(text.contains("Error: Access denied"), "{text}");

        let text = describe_api_error(&http(502, None, None, "Bad Gateway"));
        assert!(text.contains("retry later"), "{text}");
        assert!(text.contains("Response body: Bad Gateway"), "{text}");

        let mut limited = HttpError {
            status: 429,
            code: None,
            message: None,
            body: String::new(),
            retry_after: Some(40),
        };
        let text = describe_api_error(&ApiError::Http(Box::new(limited.clone())));
        assert!(text.contains("Retry after 40 s"), "{text}");
        limited.retry_after = None;
        assert!(!describe_api_error(&ApiError::Http(Box::new(limited))).contains("Retry after"));
    }

    #[test]
    fn permission_errors_name_the_method_and_the_app() {
        let op = rustore_api::spec::find("list_versions").unwrap();
        let path = "/public/v1/application/com.example.app/version";
        let text = describe_api_error_with_hint(
            &http(403, None, Some("Forbidden"), ""),
            ApiService::PublicApi,
            Some(op),
            path,
        );
        assert!(text.contains("HTTP 403"), "{text}");
        assert!(text.contains(op.summary), "{text}");
        assert!(text.contains("tool `list_versions`"), "{text}");
        assert!(text.contains("Версии и публикация"), "{text}");
        assert!(text.contains("the app `com.example.app`"), "{text}");
        assert!(text.contains("`auth_check`"), "{text}");

        // 401: ключ мог быть отклонён целиком, поэтому подсказка условная.
        let text = describe_api_error_with_hint(
            &http(401, None, None, ""),
            ApiService::PublicApi,
            Some(op),
            path,
        );
        assert!(text.contains("If other tools work"), "{text}");

        // appId в пути платежей; у списка приложений приложения нет.
        let op = rustore_api::spec::find("list_payments_by_date").unwrap();
        let text = describe_api_error_with_hint(
            &http(403, None, None, ""),
            ApiService::PublicApi,
            Some(op),
            "/public/applications/385727/invoices?invoiceDate=2026-01-01",
        );
        assert!(text.contains("the app `385727`"), "{text}");
        let op = rustore_api::spec::find("list_apps").unwrap();
        let text = describe_api_error_with_hint(
            &http(403, None, None, ""),
            ApiService::PublicApi,
            Some(op),
            "/public/v1/application",
        );
        assert!(text.contains(op.summary), "{text}");
        assert!(!text.contains("no access to the app"), "{text}");

        // Без операции и на других статусах подсказки нет.
        let plain = describe_api_error(&http(403, None, None, ""));
        assert_eq!(
            describe_api_error_with_hint(
                &http(403, None, None, ""),
                ApiService::PublicApi,
                None,
                path
            ),
            plain
        );
        let text = describe_api_error_with_hint(
            &http(404, None, None, ""),
            ApiService::PublicApi,
            Some(op),
            path,
        );
        assert!(!text.contains("auth_check"), "{text}");
    }

    #[test]
    fn other_errors_are_english() {
        assert_eq!(
            describe_api_error(&ApiError::MissingPathParam("packageName".into())),
            "missing path argument `packageName`"
        );
        assert!(
            describe_api_error(&ApiError::InvalidPath("детали".into())).contains("valid API path")
        );
        let text = describe_api_error(&ApiError::InvalidConfig("пустой токен".into()));
        assert!(!text.contains("пустой"), "{text}");
        assert!(describe_api_error(&ApiError::MissingToken).contains("RuStore API key"));
        let text = describe_api_error(&ApiError::TokenUnavailable(
            "RuStore rejected the API key".into(),
        ));
        assert!(text.contains("server instructions"), "{text}");
        assert!(text.contains("RuStore rejected the API key"), "{text}");
        let text = describe_api_error(&ApiError::ResponseTooLarge { limit: 1024 });
        assert!(
            text.contains("1024 bytes") && text.contains("`size`"),
            "{text}"
        );
        assert!(
            describe_api_error(&ApiError::Transport("timeout".into()))
                .contains("could not reach the RuStore API")
        );
    }
}
