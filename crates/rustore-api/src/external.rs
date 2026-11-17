//! Скачивание файлов по ссылкам, которые отдаёт RuStore API (иконка и скриншоты версии).
//!
//! `get_version` и `list_screenshots` возвращают `iconUrl` и `fileUrl` — ссылки на
//! хранилище RuStore (`static.rustore.ru`), а не пути RuStore API. Исполнитель
//! ([`crate::executor::ApiClient`]) такие адреса не принимает, и правильно: у него токен
//! доступа. Это отдельный клиент со своими правилами:
//!
//! - только `https://` на стандартном порту и только хосты из белого списка (точное
//!   совпадение имени, без поддоменов); для тестов — конструктор
//!   [`ExternalClient::with_loopback_http`], который есть только с `cfg(test)` или
//!   возможностью `testing` и разрешает `http://` на loopback-адресах из белого списка;
//! - логин и пароль в адресе отклоняются;
//! - заголовков `Public-Token`, `Authorization` и cookie нет: клиент не знает токена и не
//!   хранит cookie;
//! - редиректы выполняются только на адреса, которые прошли бы ту же проверку (не больше
//!   [`MAX_REDIRECTS`]); редирект на чужой хост или на `http` останавливается и становится
//!   ошибкой;
//! - тело ответа ограничено `max_bytes` (по `Content-Length` до чтения и по факту при чтении),
//!   запрос ограничен таймаутом;
//! - адрес (в нём может появиться подпись или токен в query) не попадает в `Debug`, тексты
//!   ошибок и логи: в ошибках — только хост.
//!
//! Тексты [`ExternalError`] — на английском: ядро отдаёт их агенту как есть.

use std::time::Duration;

use reqwest::redirect;

use crate::executor::{USER_AGENT, is_loopback, read_limited};

/// Хранилище файлов RuStore: иконки и скриншоты версий.
pub const RUSTORE_STATIC_HOST: &str = "static.rustore.ru";

/// Таймаут скачивания одного файла по умолчанию.
pub const DEFAULT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Лимит размера скачиваемого файла по умолчанию. Скриншот при загрузке — не больше 5 МБ,
/// иконка меньше, поэтому запас двукратный.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 10 * 1024 * 1024;

/// Сколько редиректов в пределах белого списка выполняется.
pub const MAX_REDIRECTS: usize = 5;

/// Ошибка скачивания. Адреса в тексте нет, только хост.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalError {
    /// Адрес не прошёл проверку (схема, хост, порт, учётные данные); запрос не отправлялся.
    #[error("the URL is not allowed: {0}")]
    NotAllowed(String),
    /// Ответ не 2xx (в том числе остановленный редирект на чужой адрес).
    #[error("the storage returned HTTP {status}{}", redirect_hint(*status))]
    Http { status: u16 },
    /// Файл больше лимита; ничего не возвращается.
    #[error("the file is larger than {limit} bytes")]
    TooLarge { limit: u64 },
    /// Сеть, TLS, таймаут.
    #[error("network error: {0}")]
    Transport(String),
    /// Клиент не удалось создать.
    #[error("cannot create the download client: {0}")]
    InvalidConfig(String),
}

fn redirect_hint(status: u16) -> &'static str {
    if (300..400).contains(&status) {
        " (a redirect to a host that is not allowed is not followed)"
    } else {
        ""
    }
}

/// Скачанный файл.
#[derive(Clone, PartialEq, Eq)]
pub struct Download {
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Правила проверки адреса: белый список хостов и (только в тестах) http на loopback.
#[derive(Debug, Clone)]
struct UrlRules {
    /// Имена хостов в нижнем регистре.
    hosts: Vec<String>,
    loopback_http: bool,
}

impl UrlRules {
    fn check(&self, url: &url::Url) -> Result<(), ExternalError> {
        let not_allowed = |reason: String| Err(ExternalError::NotAllowed(reason));
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return not_allowed("no host".into());
        };
        if !url.username().is_empty() || url.password().is_some() {
            return not_allowed("credentials in the URL are not allowed".into());
        }
        let host = host.trim_end_matches('.');
        match url.scheme() {
            // Нестандартный порт `url` сообщает через `port()`; у стандартного — `None`.
            "https" if url.port().is_none() => {}
            "https" => return not_allowed(format!("host `{host}`: a non-standard port")),
            "http" if self.loopback_http && is_loopback(url) => {}
            "http" => return not_allowed(format!("host `{host}`: only https is allowed")),
            other => return not_allowed(format!("the scheme `{other}` is not supported")),
        }
        if !self.hosts.iter().any(|allowed| allowed == host) {
            return not_allowed(format!(
                "host `{host}` is not in the allow list ({})",
                self.hosts.join(", ")
            ));
        }
        Ok(())
    }
}

/// Клиент скачивания файлов хранилища RuStore. Без токена RuStore API.
#[derive(Debug, Clone)]
pub struct ExternalClient {
    http: reqwest::Client,
    rules: UrlRules,
    max_bytes: u64,
}

impl ExternalClient {
    /// Клиент для хранилища RuStore ([`RUSTORE_STATIC_HOST`]) с таймаутом
    /// [`DEFAULT_DOWNLOAD_TIMEOUT`] и лимитом [`DEFAULT_MAX_DOWNLOAD_BYTES`].
    pub fn rustore_static() -> Result<Self, ExternalError> {
        Self::new(
            &[RUSTORE_STATIC_HOST],
            DEFAULT_DOWNLOAD_TIMEOUT,
            DEFAULT_MAX_DOWNLOAD_BYTES,
        )
    }

    /// Клиент с белым списком `hosts` (только https), таймаутом одного запроса `timeout`
    /// и лимитом тела `max_bytes`.
    pub fn new(hosts: &[&str], timeout: Duration, max_bytes: u64) -> Result<Self, ExternalError> {
        Self::build(hosts, timeout, max_bytes, false)
    }

    /// Как [`Self::new`], но ещё разрешает `http://` на loopback-адресах из `hosts`: для
    /// тестов с `wiremock`. В обычной сборке этого конструктора нет.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_loopback_http(
        hosts: &[&str],
        timeout: Duration,
        max_bytes: u64,
    ) -> Result<Self, ExternalError> {
        Self::build(hosts, timeout, max_bytes, true)
    }

    fn build(
        hosts: &[&str],
        timeout: Duration,
        max_bytes: u64,
        loopback_http: bool,
    ) -> Result<Self, ExternalError> {
        let rules = UrlRules {
            hosts: hosts
                .iter()
                .map(|host| host.trim().trim_end_matches('.').to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            loopback_http,
        };
        if rules.hosts.is_empty() {
            return Err(ExternalError::InvalidConfig(
                "the host allow list is empty".into(),
            ));
        }
        // Редирект проверяется теми же правилами, что и исходный адрес: на чужой хост или на
        // http клиент не идёт, ответ 3xx возвращается как есть и становится ошибкой.
        let redirect_rules = rules.clone();
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if redirect_rules.check(attempt.url()).is_ok() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(30)))
            .redirect(policy)
            .build()
            .map_err(|err| ExternalError::InvalidConfig(describe(&err.without_url())))?;
        Ok(Self {
            http,
            rules,
            max_bytes,
        })
    }

    /// Лимит размера файла, байт.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Проверяет адрес без запроса.
    pub fn check_url(&self, url: &str) -> Result<(), ExternalError> {
        self.parse(url).map(|_| ())
    }

    fn parse(&self, url: &str) -> Result<url::Url, ExternalError> {
        let parsed = url::Url::parse(url.trim())
            .map_err(|_| ExternalError::NotAllowed("not an absolute URL".into()))?;
        self.rules.check(&parsed)?;
        Ok(parsed)
    }

    /// Скачивает файл по адресу `url` в память (не больше [`Self::max_bytes`]).
    pub async fn get(&self, url: &str) -> Result<Download, ExternalError> {
        let url = self.parse(url)?;
        let host = url.host_str().unwrap_or_default().to_owned();
        tracing::debug!(%host, "скачивание файла хранилища RuStore");
        let mut response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| ExternalError::Transport(describe(&err.without_url())))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ExternalError::Http {
                status: status.as_u16(),
            });
        }
        let limit = self.max_bytes;
        if response.content_length().is_some_and(|len| len > limit) {
            return Err(ExternalError::TooLarge { limit });
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_ascii_lowercase());
        let read_limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let (bytes, truncated) = read_limited(&mut response, read_limit)
            .await
            .map_err(|_| ExternalError::Transport("cannot read the response body".into()))?;
        // `read_limited` обрезает ровно на лимите; файл ровно в лимит — не превышение, поэтому
        // обрезка определяется по наличию данных сверх него.
        if truncated {
            return Err(ExternalError::TooLarge { limit });
        }
        Ok(Download {
            content_type,
            bytes,
        })
    }
}

/// Описание ошибки reqwest без адреса: вид ошибки и причины.
fn describe(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "cannot connect"
    } else if err.is_redirect() {
        "redirect error"
    } else if err.is_body() || err.is_decode() {
        "cannot read the response body"
    } else {
        "request failed"
    };
    let mut text = kind.to_owned();
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

    fn client() -> ExternalClient {
        ExternalClient::rustore_static().unwrap()
    }

    #[test]
    fn only_https_on_allowed_hosts() {
        let client = client();
        client
            .check_url("https://static.rustore.ru/rustore/a/b.png")
            .unwrap();
        client
            .check_url("https://STATIC.rustore.ru./a.png?x=1")
            .unwrap();
        for (url, reason) in [
            ("http://static.rustore.ru/a.png", "only https"),
            ("https://evil.example/a.png", "not in the allow list"),
            (
                "https://static.rustore.ru.evil.example/a.png",
                "not in the allow list",
            ),
            (
                "https://cdn.static.rustore.ru/a.png",
                "not in the allow list",
            ),
            ("https://static.rustore.ru:8443/a.png", "non-standard port"),
            ("https://user:pw@static.rustore.ru/a.png", "credentials"),
            ("ftp://static.rustore.ru/a.png", "scheme"),
            ("/a.png", "not an absolute URL"),
            ("http://127.0.0.1:8080/a.png", "only https"),
        ] {
            let err = client.check_url(url).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(reason), "{url}: {text}");
            assert!(!text.contains("a.png"), "адрес в ошибке: {text}");
        }
    }

    #[test]
    fn loopback_http_needs_the_test_constructor_and_the_allow_list() {
        let client =
            ExternalClient::with_loopback_http(&["127.0.0.1"], Duration::from_secs(1), 10).unwrap();
        client.check_url("http://127.0.0.1:1234/a.png").unwrap();
        let err = client.check_url("http://localhost:1234/a.png").unwrap_err();
        assert!(err.to_string().contains("not in the allow list"), "{err}");
        // Http разрешён только на loopback, даже если хост в списке.
        let client =
            ExternalClient::with_loopback_http(&["static.rustore.ru"], Duration::from_secs(1), 10)
                .unwrap();
        let err = client
            .check_url("http://static.rustore.ru/a.png")
            .unwrap_err();
        assert!(err.to_string().contains("only https"), "{err}");
        let err = ExternalClient::new(&[" "], Duration::from_secs(1), 10).unwrap_err();
        assert!(matches!(err, ExternalError::InvalidConfig(_)), "{err}");
    }

    fn local(max_bytes: u64) -> ExternalClient {
        ExternalClient::with_loopback_http(&["127.0.0.1"], Duration::from_secs(5), max_bytes)
            .unwrap()
    }

    #[tokio::test]
    async fn downloads_within_the_limit_and_follows_allowed_redirects() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/a.png"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![7u8; 16], "image/png"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/big.png"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![7u8; 17], "image/png"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/moved.png"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/a.png", server.uri())),
            )
            .mount(&server)
            .await;
        // Тот же сервер, но имя `localhost` не в белом списке.
        let foreign = server.uri().replace("127.0.0.1", "localhost");
        Mock::given(method("GET"))
            .and(path("/away.png"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", format!("{foreign}/a.png")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/missing.png"))
            .respond_with(ResponseTemplate::new(404).set_body_string("secret-ish body"))
            .mount(&server)
            .await;
        let client = local(16);

        let file = client
            .get(&format!("{}/a.png", server.uri()))
            .await
            .unwrap();
        assert_eq!(file.bytes, vec![7u8; 16]);
        assert_eq!(file.content_type.as_deref(), Some("image/png"));

        let err = client
            .get(&format!("{}/big.png?token=abc", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, ExternalError::TooLarge { limit: 16 });

        let moved = client
            .get(&format!("{}/moved.png", server.uri()))
            .await
            .unwrap();
        assert_eq!(moved.bytes.len(), 16);

        let err = client
            .get(&format!("{}/away.png", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, ExternalError::Http { status: 302 });
        assert!(err.to_string().contains("not followed"), "{err}");

        let err = client
            .get(&format!("{}/missing.png?sig=abc", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "the storage returned HTTP 404");

        // Чужой хост — без запроса.
        let err = client.get(&format!("{foreign}/a.png")).await.unwrap_err();
        assert!(matches!(err, ExternalError::NotAllowed(_)), "{err}");
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(
            requests
                .iter()
                .all(|request| request.headers.get("public-token").is_none()
                    && request.headers.get("authorization").is_none()),
            "запрос с заголовком авторизации"
        );
    }

    #[test]
    fn debug_hides_the_body() {
        let download = Download {
            content_type: Some("image/png".into()),
            bytes: vec![1, 2, 3],
        };
        assert_eq!(
            format!("{download:?}"),
            r#"Download { content_type: Some("image/png"), bytes: 3 }"#
        );
    }
}
