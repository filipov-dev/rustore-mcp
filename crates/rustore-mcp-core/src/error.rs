//! Ошибки вызова инструмента.
//!
//! Любая ошибка вызова возвращается MCP-клиенту как `CallToolResult` с `isError: true`
//! и текстом для агента на английском: агент должен понять, что исправить в аргументах
//! или что попросить у пользователя. Ключ и токен в тексты ошибок не попадают: [`ApiError`]
//! их не содержит, аргументы инструмента их не принимают, а сервер дополнительно вырезает
//! известные токены клиента из итогового текста.

use std::fmt;

use rustore_api::executor::{ApiError, HttpError};

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
    match err {
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
    let code = err.code.as_deref().map(str::trim).filter(|c| !c.is_empty());
    let message = err
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let mut text = format!("RuStore API returned HTTP {status}{hint}.");
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
