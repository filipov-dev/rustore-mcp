//! Токены доступа RuStore API (JWE) для исполнителя запросов.
//!
//! Источник токенов — трейт [`TokenSource`]: исполнитель берёт у него токен перед каждой
//! попыткой, после ответа 401 просит сбросить отвергнутый ([`TokenSource::invalidate`]) и
//! вырезает известные токены из текстов ([`TokenSource::known_tokens`]).
//!
//! [`KeyTokens`] получает токен по ключу RuStore API ([`ApiKey`]):
//! `POST https://public-api-m.rustore.ru/public/auth/` с JSON
//! `{"keyId", "timestamp", "signature"}`, где `signature` — SHA512withRSA строки
//! `keyId + timestamp` ([`ApiKey::sign_token_request`]), а `timestamp` — время ISO 8601 с
//! миллисекундами и смещением. Сервер принимает время, которое отличается от его часов не
//! больше чем на минуту, и отвечает `{"code": "OK", "body": {"jwe": "...", "ttl": 900}}`.
//! Токен живёт `ttl` секунд (15 минут) и передаётся в заголовке `Public-Token`.
//!
//! Токен хранится только в памяти процесса и заменяется заранее, до истечения срока;
//! параллельные запросы ждут одного обновления. Если RuStore отверг время запроса
//! (`Range timestamp not valid`), запрос токена один раз повторяется с новым временем: так
//! переживается задержка в сети; если отказ повторился — часы машины расходятся с
//! настоящим временем, и ошибка об этом говорит.
//!
//! [`StaticToken`] — заранее известный токен без обновления (тесты).
//!
//! Токен, подпись и ключ хранятся в [`Zeroizing`], их `Debug` и `Display` не раскрывают
//! значение, в тексты ошибок и логи они не попадают. Время берётся из [`Clock`]: тесты
//! подставляют [`ManualClock`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::executor::{
    DEFAULT_BASE_URL, USER_AGENT, check_base_url, describe_reqwest_error, envelope_error_parts,
    read_limited,
};
use crate::key::{ApiKey, CONSOLE_HINT, KeyError};

/// Путь запроса токена относительно базового адреса API.
pub const AUTH_PATH: &str = "/public/auth/";

/// Срок жизни токена, если RuStore его не сообщил.
pub const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(900);

/// Наибольший срок жизни токена, который мы принимаем, как бы долго его ни обещал сервер.
pub const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// Токен заменяется новым, когда до истечения остаётся меньше этого времени (но не больше
/// четверти срока жизни токена).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Признак ответа RuStore об отвергнутом времени запроса.
const TIMESTAMP_REJECTED: &str = "range timestamp";

/// Сколько прежних токенов помнить для вырезания из текстов.
const REMEMBERED_TOKENS: usize = 4;

/// Таймаут запроса токена.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Предел тела ответа на запрос токена.
const TOKEN_RESPONSE_LIMIT: usize = 64 * 1024;

/// Предел длины сообщения RuStore в тексте ошибки.
const MAX_ERROR_MESSAGE: usize = 300;

/// Будущее, которое возвращают методы трейта токенов (трейт должен оставаться
/// dyn-совместимым: исполнитель хранит `Arc<dyn TokenSource>`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Источник текущего времени.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> SystemTime;
}

/// Системные часы.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Управляемые часы для тестов: время меняется только вызовами [`ManualClock::advance`]
/// и [`ManualClock::set`].
#[derive(Debug)]
pub struct ManualClock(Mutex<SystemTime>);

impl ManualClock {
    pub fn new(start: SystemTime) -> Self {
        Self(Mutex::new(start))
    }

    pub fn set(&self, time: SystemTime) {
        *lock(&self.0) = time;
    }

    pub fn advance(&self, by: Duration) {
        let mut now = lock(&self.0);
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *lock(&self.0)
    }
}

/// Блокировка без паники: под блокировками этого модуля нет кода, который паникует, поэтому
/// отравление не означает испорченных данных.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Токен доступа RuStore API (JWE). `Debug` и `Display` не раскрывают значение, память
/// затирается при освобождении.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(Zeroizing<String>);

impl AccessToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Значение токена. Использовать только для заголовка `Public-Token`.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(***)")
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Ошибки получения токена. Тексты на английском и не содержат ни ключа, ни подписи, ни
/// токена.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("{0}")]
    Key(#[from] KeyError),
    /// RuStore отказал: неверный `keyId`, подпись не сходится с ключом, ключ удалён или
    /// истёк и т. п.
    #[error("{}", describe_rejection(*.status, .code.as_deref(), .message.as_deref()))]
    Rejected {
        status: u16,
        /// `code` из ответа RuStore.
        code: Option<String>,
        /// `message` из ответа RuStore, обрезанное и без секретов.
        message: Option<String>,
    },
    /// RuStore дважды отверг время запроса: часы машины расходятся с настоящим временем.
    #[error(
        "RuStore rejected the time of the token request twice (`{0}`): the system clock of \
         this machine differs from the real time by more than a minute. Synchronize the clock \
         (turn on automatic date and time) and retry"
    )]
    ClockSkew(String),
    /// Ответ не похож на ответ RuStore на запрос токена.
    #[error("the RuStore token endpoint returned an unexpected response: {0}")]
    InvalidResponse(String),
    /// Сеть, TLS, таймаут.
    #[error("could not reach the RuStore token endpoint: {0}")]
    Transport(String),
    /// Некорректная настройка источника токенов.
    #[error("invalid token source configuration: {0}")]
    Config(String),
}

fn describe_rejection(status: u16, code: Option<&str>, message: Option<&str>) -> String {
    let mut text = format!("RuStore rejected the API key (HTTP {status}");
    if let Some(code) = code.filter(|c| !c.is_empty()) {
        text.push_str(", ");
        text.push_str(code);
    }
    text.push(')');
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        text.push_str(": ");
        text.push_str(message);
    }
    let lower = message.unwrap_or_default().to_ascii_lowercase();
    let hint = if lower.contains("signature") {
        format!(
            " The signature did not verify: the private key does not belong to this key ID. \
             Check that the key ID and the private key are from the same key in the \
             {CONSOLE_HINT}, or create a new key there."
        )
    } else if lower.contains("keyid") || lower.contains("key id") || lower.contains("not found") {
        format!(
            " Check the key ID (keyId) in the {CONSOLE_HINT}: the key may have been deleted or \
             expired."
        )
    } else {
        format!(
            " Check that the key exists and has not expired in the {CONSOLE_HINT}, and that \
             the key ID matches the private key."
        )
    };
    text.push_str(&hint);
    text
}

/// Новый токен и его срок жизни.
#[derive(Debug, Clone)]
pub struct FetchedToken {
    pub token: AccessToken,
    pub lifetime: Duration,
}

/// Источник токенов для исполнителя.
pub trait TokenSource: Send + Sync + fmt::Debug {
    /// Действующий токен: из памяти или новый.
    fn token(&self) -> BoxFuture<'_, Result<AccessToken, TokenError>>;

    /// API отверг токен (401): следующий [`Self::token`] должен получить новый. Если токен
    /// уже заменён (параллельный запрос успел обновить), ничего не делает.
    fn invalidate<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, ()>;

    /// Текущий и недавние прежние токены: исполнитель вырезает их из текстов ошибок и
    /// ответов на случай, если сервер отразил заголовок с токеном.
    fn known_tokens(&self) -> Vec<AccessToken>;
}

#[derive(Clone)]
struct Cached {
    token: AccessToken,
    obtained_at: SystemTime,
    expires_at: SystemTime,
}

/// Токены по ключу RuStore API с кэшем в памяти.
///
/// Токен заменяется, когда до истечения остаётся меньше [`REFRESH_MARGIN`] или часы отвели
/// назад раньше времени получения. Обновление идёт под асинхронной блокировкой: параллельные
/// запросы ждут его и получают тот же новый токен.
pub struct KeyTokens {
    key: ApiKey,
    auth_url: url::Url,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    cache: tokio::sync::Mutex<Option<Cached>>,
    /// Недавние токены (новые в конце), для [`TokenSource::known_tokens`] без асинхронной
    /// блокировки.
    remembered: Mutex<Vec<AccessToken>>,
}

impl fmt::Debug for KeyTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyTokens")
            .field("key", &self.key)
            .field("auth_url", &self.auth_url.as_str())
            .finish_non_exhaustive()
    }
}

/// Тело запроса токена. Поля заимствованы: подпись живёт в [`Zeroizing`] вызывающего, а
/// сериализованное тело тоже кладётся в [`Zeroizing`].
#[derive(Serialize)]
struct AuthRequest<'a> {
    #[serde(rename = "keyId")]
    key_id: &'a str,
    timestamp: &'a str,
    signature: &'a str,
}

/// `body` успешного ответа.
#[derive(Deserialize)]
struct AuthBody {
    jwe: Option<String>,
    ttl: Option<Value>,
}

impl Drop for AuthBody {
    fn drop(&mut self) {
        if let Some(jwe) = &mut self.jwe {
            zeroize::Zeroize::zeroize(jwe);
        }
    }
}

/// Итог одной попытки получить токен.
enum Attempt {
    Token(FetchedToken),
    /// RuStore отверг время запроса; внутри — его сообщение.
    TimestampRejected(String),
}

impl KeyTokens {
    /// Токены с адресом RuStore API по умолчанию ([`DEFAULT_BASE_URL`]).
    pub fn new(key: ApiKey) -> Result<Self, TokenError> {
        let base = url::Url::parse(DEFAULT_BASE_URL)
            .map_err(|e| TokenError::Config(format!("base URL: {e}")))?;
        Self::with_base_url(key, &base)
    }

    /// Токены с другим базовым адресом API (тесты). Как и у исполнителя, допустимы
    /// `https://` и `http://` только для loopback; префикс пути сохраняется.
    pub fn with_base_url(key: ApiKey, base: &url::Url) -> Result<Self, TokenError> {
        check_base_url(base).map_err(|e| TokenError::Config(e.to_string()))?;
        let mut auth_url = base.clone();
        let prefix = auth_url.path().trim_end_matches('/').to_owned();
        auth_url.set_path(&format!("{prefix}{AUTH_PATH}"));
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TOKEN_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| TokenError::Config(describe_reqwest_error(&e)))?;
        Ok(Self {
            key,
            auth_url,
            http,
            clock: Arc::new(SystemClock),
            cache: tokio::sync::Mutex::new(None),
            remembered: Mutex::new(Vec::new()),
        })
    }

    /// Те же токены с другими часами (тесты).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Ключ, по которому выпускаются токены.
    pub fn key(&self) -> &ApiKey {
        &self.key
    }

    /// Новый токен без кэша: с одним повтором, если RuStore отверг время запроса.
    pub async fn fetch(&self) -> Result<FetchedToken, TokenError> {
        match self.attempt().await? {
            Attempt::Token(token) => Ok(token),
            Attempt::TimestampRejected(_) => {
                tracing::debug!("RuStore отверг время запроса токена, повтор с новым временем");
                match self.attempt().await? {
                    Attempt::Token(token) => Ok(token),
                    Attempt::TimestampRejected(message) => Err(TokenError::ClockSkew(message)),
                }
            }
        }
    }

    /// Одна попытка получить токен.
    async fn attempt(&self) -> Result<Attempt, TokenError> {
        let sent_at = self.clock.now();
        let timestamp = format_timestamp(sent_at)?;
        let signature = self.key.sign_token_request(&timestamp)?;
        let body = Zeroizing::new(
            serde_json::to_vec(&AuthRequest {
                key_id: self.key.key_id().as_str(),
                timestamp: &timestamp,
                signature: &signature,
            })
            .map_err(|e| TokenError::Config(e.to_string()))?,
        );
        let mut response = self
            .http
            .post(self.auth_url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| TokenError::Transport(describe_reqwest_error(&e.without_url())))?;
        let status = response.status();
        let (bytes, truncated) = read_limited(&mut response, TOKEN_RESPONSE_LIMIT)
            .await
            .map_err(|e| TokenError::Transport(e.to_string()))?;
        let bytes = Zeroizing::new(bytes);
        if truncated {
            return Err(TokenError::InvalidResponse(format!(
                "HTTP {status}, the response is larger than {TOKEN_RESPONSE_LIMIT} bytes"
            )));
        }
        let scrub = |text: &str| self.scrub(text, &signature);
        let value: Option<Value> = serde_json::from_slice(&bytes).ok();
        let Some(value) = value else {
            return Err(if status.is_success() {
                TokenError::InvalidResponse(format!("HTTP {status}, the response is not JSON"))
            } else {
                TokenError::Rejected {
                    status: status.as_u16(),
                    code: None,
                    message: status.canonical_reason().map(str::to_owned),
                }
            });
        };
        let (code, message) = envelope_error_parts(&value);
        let code_ok = code.as_deref() == Some("OK");
        if !(status.is_success() && code_ok) {
            let message = message.map(|m| scrub(&m));
            if message
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().contains(TIMESTAMP_REJECTED))
            {
                return Ok(Attempt::TimestampRejected(message.unwrap_or_default()));
            }
            return Err(TokenError::Rejected {
                status: status.as_u16(),
                code: code.map(|c| scrub(&c)),
                message,
            });
        }
        let body = value
            .get("body")
            .cloned()
            .and_then(|b| serde_json::from_value::<AuthBody>(b).ok());
        let Some(mut body) = body else {
            return Err(TokenError::InvalidResponse(
                "the response has no `body` with a token".into(),
            ));
        };
        let jwe = body.jwe.take().filter(|t| !t.trim().is_empty());
        let Some(jwe) = jwe.map(Zeroizing::new) else {
            return Err(TokenError::InvalidResponse(
                "the response has no token (`body.jwe`)".into(),
            ));
        };
        if jwe.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(TokenError::InvalidResponse(
                "the token contains characters not allowed in an HTTP header".into(),
            ));
        }
        let lifetime = body
            .ttl
            .as_ref()
            .and_then(ttl_seconds)
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TOKEN_LIFETIME)
            .min(MAX_TOKEN_LIFETIME);
        Ok(Attempt::Token(FetchedToken {
            token: AccessToken::new(jwe.as_str()),
            lifetime,
        }))
    }

    /// Текст из ответа без подписи, ключа и известных токенов; обрезан до
    /// [`MAX_ERROR_MESSAGE`].
    fn scrub(&self, text: &str, signature: &str) -> String {
        let mut text = text.to_owned();
        let private_key = self.key.expose_private_key_base64();
        for secret in [signature, private_key] {
            if !secret.is_empty() {
                text = text.replace(secret, "***");
            }
        }
        for token in lock(&self.remembered).iter() {
            if !token.expose().is_empty() {
                text = text.replace(token.expose(), "***");
            }
        }
        if text.chars().count() > MAX_ERROR_MESSAGE {
            text = text.chars().take(MAX_ERROR_MESSAGE).collect();
            text.push('…');
        }
        text
    }

    fn fresh(cached: &Cached, now: SystemTime) -> bool {
        let lifetime = cached
            .expires_at
            .duration_since(cached.obtained_at)
            .unwrap_or_default();
        let margin = REFRESH_MARGIN.min(lifetime / 4);
        now >= cached.obtained_at
            && now
                .checked_add(margin)
                .is_some_and(|deadline| deadline < cached.expires_at)
    }

    async fn current(&self) -> Result<AccessToken, TokenError> {
        let mut cache = self.cache.lock().await;
        let now = self.clock.now();
        if let Some(cached) = cache.as_ref()
            && Self::fresh(cached, now)
        {
            return Ok(cached.token.clone());
        }
        // Срок считается от времени до отправки запроса: так токен не переживёт свой срок
        // на сервере из-за задержки ответа.
        let fetched = self.fetch().await?;
        let token = fetched.token;
        {
            let mut remembered = lock(&self.remembered);
            remembered.push(token.clone());
            if remembered.len() > REMEMBERED_TOKENS {
                remembered.remove(0);
            }
        }
        *cache = Some(Cached {
            token: token.clone(),
            obtained_at: now,
            expires_at: now + fetched.lifetime,
        });
        tracing::debug!(
            lifetime_secs = fetched.lifetime.as_secs(),
            "получен токен RuStore API"
        );
        Ok(token)
    }
}

impl TokenSource for KeyTokens {
    fn token(&self) -> BoxFuture<'_, Result<AccessToken, TokenError>> {
        Box::pin(self.current())
    }

    fn invalidate<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if cache
                .as_ref()
                .is_some_and(|cached| &cached.token == rejected)
            {
                *cache = None;
            }
        })
    }

    fn known_tokens(&self) -> Vec<AccessToken> {
        lock(&self.remembered).clone()
    }
}

/// `ttl` числом или строкой с числом.
fn ttl_seconds(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Заранее известный токен без обновления (тесты и отладка с внешним токеном).
#[derive(Debug, Clone)]
pub struct StaticToken(AccessToken);

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(AccessToken::new(token))
    }
}

impl TokenSource for StaticToken {
    fn token(&self) -> BoxFuture<'_, Result<AccessToken, TokenError>> {
        let token = self.0.clone();
        Box::pin(async move { Ok(token) })
    }

    fn invalidate<'a>(&'a self, _rejected: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn known_tokens(&self) -> Vec<AccessToken> {
        vec![self.0.clone()]
    }
}

/// Время запроса токена: ISO 8601 в UTC с миллисекундами и числовым смещением,
/// `2024-06-18T08:49:08.290+00:00`. RuStore принимает любое смещение; UTC не зависит от
/// часового пояса машины.
pub fn format_timestamp(time: SystemTime) -> Result<String, KeyError> {
    let since_epoch = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| KeyError::Clock)?;
    let secs = since_epoch.as_secs();
    let millis = since_epoch.subsec_millis();
    let days = i64::try_from(secs / 86_400).map_err(|_| KeyError::Clock)?;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err(KeyError::Clock);
    }
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}+00:00",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    ))
}

/// Дата григорианского календаря по числу дней от 1970-01-01 (алгоритм Говарда Хиннанта).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_iso8601_utc_with_millis() {
        let at = |secs: u64, millis: u64| {
            format_timestamp(UNIX_EPOCH + Duration::from_millis(secs * 1000 + millis)).unwrap()
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000+00:00");
        assert_eq!(at(1_718_700_548, 290), "2024-06-18T08:49:08.290+00:00");
        // 29 февраля високосного года и конец года.
        assert_eq!(at(951_825_600, 7), "2000-02-29T12:00:00.007+00:00");
        assert_eq!(at(1_767_225_599, 999), "2025-12-31T23:59:59.999+00:00");
        assert!(format_timestamp(UNIX_EPOCH - Duration::from_secs(1)).is_err());
    }

    #[test]
    fn rejections_have_hints_without_secrets() {
        let text = TokenError::Rejected {
            status: 400,
            code: Some("ERROR".into()),
            message: Some("Signature encode error".into()),
        }
        .to_string();
        assert!(text.contains("HTTP 400, ERROR"), "{text}");
        assert!(text.contains("does not belong to this key ID"), "{text}");
        assert!(text.contains("Company > RuStore API"), "{text}");

        let text = TokenError::Rejected {
            status: 400,
            code: None,
            message: Some("KeyId must not be null".into()),
        }
        .to_string();
        assert!(text.contains("Check the key ID"), "{text}");

        let text = TokenError::ClockSkew("Range timestamp not valid".into()).to_string();
        assert!(text.contains("system clock"), "{text}");
    }

    #[test]
    fn token_debug_hides_value() {
        let token = AccessToken::new("eyJsecret.jwe");
        assert_eq!(format!("{token:?}"), "AccessToken(***)");
        assert_eq!(token.to_string(), "***");
    }
}
