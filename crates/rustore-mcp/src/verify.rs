//! Проверка ключа RuStore API: пробное получение токена и запрос списка приложений
//! (`GET /public/v1/application`) через общий исполнитель запросов.
//!
//! Проверка идёт тем же путём, что и запросы сервера: [`KeyTokens`] подписывает запрос токена,
//! исполнитель отправляет `Public-Token`, снимает конверт ответа и вырезает токен из ошибок.
//! Значения ключа и токена в сообщения не попадают.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use rustore_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, DEFAULT_BASE_URL, HttpMethod, ResponseBody,
    RetryPolicy,
};
use rustore_api::key::CONSOLE_HINT;
use rustore_api::{ApiKey, KeyTokens, TokenSource};
use serde_json::Value;

/// Путь пробного запроса: список приложений компании.
pub const CHECK_PATH: &str = "/public/v1/application";

/// Пробный запрос для текстов и `auth status --json`.
pub fn check_request() -> String {
    format!("GET {CHECK_PATH}")
}

/// Итог проверки: RuStore выдал токен и ответил на пробный запрос.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Сколько приложений доступно ключу, если ответ это сообщает.
    pub apps: Option<u64>,
}

impl fmt::Display for CheckOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RuStore issued a token")?;
        match self.apps {
            Some(1) => write!(f, " and listed 1 app ({})", check_request()),
            Some(count) => write!(f, " and listed {count} apps ({})", check_request()),
            None => write!(f, " and answered {}", check_request()),
        }
    }
}

/// Проверка ключа. В тестах подменяется, чтобы не ходить в сеть.
pub trait KeyVerifier {
    async fn check(&self, key: &ApiKey) -> anyhow::Result<CheckOutcome>;
}

/// Проверка через RuStore API.
#[derive(Debug, Clone)]
pub struct ApiVerifier {
    /// Настройки клиента без источника токенов: адрес API и таймауты проверки.
    config: ClientConfig,
}

impl ApiVerifier {
    /// Проверка через RuStore API по адресу по умолчанию.
    pub fn new() -> anyhow::Result<Self> {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Проверка через API по другому адресу (тесты).
    pub fn with_base_url(base: &str) -> anyhow::Result<Self> {
        let mut config = ClientConfig::new(None)?;
        config.base_url = base
            .parse()
            .map_err(|err| anyhow!("invalid RuStore API URL `{base}`: {err}"))?;
        config.timeout = Duration::from_secs(30);
        config.retry = RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        };
        Ok(Self { config })
    }
}

impl KeyVerifier for ApiVerifier {
    async fn check(&self, key: &ApiKey) -> anyhow::Result<CheckOutcome> {
        // Клиент создаётся только при проверке: без корневых сертификатов системы HTTPS-клиент
        // не создаётся, а `--no-verify` должен работать и там.
        let tokens = KeyTokens::with_base_url(key.clone(), &self.config.base_url)?;
        // Сначала токен: так отказ в ключе отличается от отказа в доступе к методу.
        tokens.token().await.map_err(|err| anyhow!("{err}"))?;
        let mut config = self.config.clone();
        config.tokens = Some(Arc::new(tokens));
        let client = ApiClient::new(config)?;
        let mut request = ApiRequest::new(HttpMethod::Get, CHECK_PATH);
        // Одной страницы из одного приложения достаточно: число приложений — в `totalElements`.
        request.query.push(("size".into(), "1".into()));
        let response = client
            .execute(request)
            .await
            .map_err(|err| describe_failure(&err))?;
        let apps = match &response.body {
            ResponseBody::Json(body) => count_apps(body, response.pagination.total_elements),
            _ => None,
        };
        Ok(CheckOutcome { apps })
    }
}

/// Число приложений: `totalElements` постраничной выдачи, иначе длина списка.
fn count_apps(body: &Value, total: Option<u64>) -> Option<u64> {
    total.or_else(|| {
        body.get("content")
            .or(Some(body))
            .and_then(Value::as_array)
            .map(|list| list.len() as u64)
    })
}

/// Понятная ошибка проверки: что отметить в консоли RuStore.
fn describe_failure(err: &ApiError) -> anyhow::Error {
    match err.status() {
        Some(401) => anyhow!(
            "RuStore accepted the key but rejected the token for {} ({err}). Check that the \
             key is active in the {CONSOLE_HINT}",
            check_request()
        ),
        Some(403) => anyhow!(
            "RuStore issued a token, but the key has no access to {} ({err}). In the \
             {CONSOLE_HINT}, open the key and tick the API methods it may call (at least the \
             application list) and the apps it may work with, then retry",
            check_request()
        ),
        _ => anyhow!("the check request {} failed: {err}", check_request()),
    }
}

#[cfg(test)]
pub mod testing {
    use std::sync::Mutex;

    use super::*;

    /// Проверка без сети: заданный итог или ошибка, запоминает проверенные keyId.
    #[derive(Debug, Default)]
    pub struct FakeVerifier {
        pub failure: Option<String>,
        pub apps: Option<u64>,
        pub checked: Mutex<Vec<String>>,
    }

    impl FakeVerifier {
        pub fn failing(message: &str) -> Self {
            Self {
                failure: Some(message.to_owned()),
                ..Self::default()
            }
        }

        pub fn checked(&self) -> Vec<String> {
            self.checked.lock().map(|c| c.clone()).unwrap_or_default()
        }
    }

    impl KeyVerifier for FakeVerifier {
        async fn check(&self, key: &ApiKey) -> anyhow::Result<CheckOutcome> {
            if let Ok(mut checked) = self.checked.lock() {
                checked.push(key.key_id().as_str().to_owned());
            }
            match &self.failure {
                Some(message) => Err(anyhow!("{message}")),
                None => Ok(CheckOutcome { apps: self.apps }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rustore_api::testing;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TOKEN: &str = "jwe-token-for-tests";

    async fn mount_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/public/auth/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {"jwe": TOKEN, "ttl": 900}
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn checks_with_a_token_and_counts_apps() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("GET"))
            .and(path(CHECK_PATH))
            .and(query_param("size", "1"))
            .and(header("public-token", TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "OK",
                "body": {
                    "content": [{"packageName": "com.example.app"}],
                    "pageNumber": 0,
                    "pageSize": 1,
                    "totalElements": 3,
                    "totalPages": 3
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(&server.uri()).unwrap();
        let outcome = verifier.check(&testing::api_key()).await.unwrap();
        assert_eq!(outcome.apps, Some(3));
        assert_eq!(
            outcome.to_string(),
            "RuStore issued a token and listed 3 apps (GET /public/v1/application)"
        );
    }

    #[tokio::test]
    async fn explains_missing_access() {
        let server = MockServer::start().await;
        mount_token(&server).await;
        Mock::given(method("GET"))
            .and(path(CHECK_PATH))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "code": "FORBIDDEN",
                "message": format!("no access for {TOKEN}")
            })))
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(&server.uri()).unwrap();
        let err = verifier.check(&testing::api_key()).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("tick the API methods"), "{text}");
        assert!(text.contains("Company > RuStore API"), "{text}");
        assert!(!text.contains(TOKEN), "{text}");
    }

    #[tokio::test]
    async fn token_rejection_is_reported_before_the_api_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/public/auth/"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "code": "ERROR",
                "message": "Invalid signature"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(CHECK_PATH))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(&server.uri()).unwrap();
        let err = verifier.check(&testing::api_key()).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("does not belong to this key ID"), "{text}");
        assert!(!text.contains(testing::TEST_KEY_ID), "{text}");
    }

    #[tokio::test]
    async fn transport_errors_are_reported() {
        // Порт закрыт: соединение не устанавливается.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let verifier = ApiVerifier::with_base_url(&format!("http://{address}")).unwrap();
        let err = verifier.check(&testing::api_key()).await.unwrap_err();
        assert!(format!("{err:#}").contains("could not reach"), "{err:#}");
    }

    #[test]
    fn counts_plain_lists() {
        assert_eq!(count_apps(&json!([1, 2]), None), Some(2));
        assert_eq!(count_apps(&json!({"content": [1]}), None), Some(1));
        assert_eq!(count_apps(&json!({"content": [1]}), Some(5)), Some(5));
        assert_eq!(count_apps(&json!({"x": 1}), None), None);
    }
}
