//! Push API RuStore: учётные данные проекта push и проверки запроса до отправки.
//!
//! Push-уведомления — отдельный сервис RuStore со своими хостами ([`PUSH_BASE_URL`] для
//! отправки на устройство, [`PUSH_TOPICS_BASE_URL`] для топиков) и своим секретом:
//! сервисным токеном проекта push (консоль RuStore: приложение → Push-уведомления →
//! Проекты). Ключ RuStore API и токен доступа Public API здесь не участвуют.
//!
//! - [`PushCredentials`] — идентификатор проекта и сервисный токен профиля. Хранятся в
//!   системном хранилище отдельной записью ([`crate::auth::KeyStore::get_push`]); токен не
//!   раскрывается в `Debug` и затирается при освобождении.
//! - [`PushConfig`] — настройки исполнителя для push: адреса сервисов с префиксом проекта и
//!   источник сервисного токена. Исполнитель кладёт токен в `Authorization: Bearer` только
//!   запросам операций push ([`crate::spec::ApiService::is_push`]) и только на эти адреса.
//! - [`check_message_size`] — лимит сообщения до запроса, [`request_secrets`] — push-токены
//!   устройств из тела запроса: исполнитель вырезает их из ответов и ошибок.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use crate::access::{StaticToken, TokenSource};
use crate::executor::ApiError;

/// Сервис отправки push-уведомления на устройство.
pub const PUSH_BASE_URL: &str = "https://vkpns.rustore.ru";
/// Сервис push-уведомлений по топикам.
pub const PUSH_TOPICS_BASE_URL: &str = "https://vkpns-topics.rustore.ru";
/// Максимальный объём сообщения по документации («Максимальный объем сообщения 4096
/// байт»): компактный JSON поля `message` тела запроса в UTF-8.
pub const MAX_MESSAGE_BYTES: usize = 4096;
/// Длина идентификатора проекта push.
pub const MAX_PROJECT_ID_LEN: usize = 128;
/// Длина сервисного токена: с запасом, токены консоли заметно короче.
pub const MAX_SERVICE_TOKEN_LEN: usize = 4096;
/// Где взять проект и сервисный токен.
pub const CONSOLE_HINT: &str = "RuStore console (the app > Push notifications > Projects)";

/// Ошибка учётных данных push. Значение токена в текст не попадает.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PushCredentialsError {
    #[error(
        "invalid push project ID: use 1-{MAX_PROJECT_ID_LEN} characters from A-Z, a-z, 0-9, \
         `-`, `_`, `.` as shown in the {CONSOLE_HINT}"
    )]
    ProjectId,
    #[error("the push service token is empty")]
    EmptyToken,
    #[error(
        "the push service token must be one line of printable ASCII without spaces, at most \
         {MAX_SERVICE_TOKEN_LEN} characters; copy it again from the {CONSOLE_HINT}"
    )]
    TokenFormat,
    #[error("the stored push credentials are not valid JSON with projectId and serviceToken")]
    Stored,
}

/// Запись учётных данных push в хранилище и в файле секрета.
#[derive(Serialize, Deserialize)]
struct StoredPush {
    #[serde(rename = "projectId", alias = "project_id")]
    project_id: Option<String>,
    #[serde(rename = "serviceToken", alias = "service_token")]
    service_token: Option<String>,
}

impl Drop for StoredPush {
    fn drop(&mut self) {
        if let Some(token) = &mut self.service_token {
            token.zeroize();
        }
    }
}

/// Идентификатор проекта push и сервисный токен.
#[derive(Clone, PartialEq, Eq)]
pub struct PushCredentials {
    project_id: String,
    service_token: Zeroizing<String>,
}

impl fmt::Debug for PushCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushCredentials")
            .field("project_id", &self.project_id)
            .field("service_token", &"***")
            .finish()
    }
}

impl PushCredentials {
    /// Проверяет и собирает учётные данные. Пробелы и перевод строки по краям токена
    /// (файл с `\n` в конце, `echo`) отбрасываются.
    pub fn new(project_id: &str, service_token: &str) -> Result<Self, PushCredentialsError> {
        let project_id = project_id.trim();
        if !is_valid_project_id(project_id) {
            return Err(PushCredentialsError::ProjectId);
        }
        let token = service_token.trim();
        if token.is_empty() {
            return Err(PushCredentialsError::EmptyToken);
        }
        // Токен уходит в заголовок: только видимые символы ASCII, без пробелов.
        if token.len() > MAX_SERVICE_TOKEN_LEN || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(PushCredentialsError::TokenFormat);
        }
        Ok(Self {
            project_id: project_id.to_owned(),
            service_token: Zeroizing::new(token.to_owned()),
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Идентификатор проекта в маске для `auth status` (идентификатор не секрет, но и
    /// целиком в выводе не нужен).
    pub fn masked_project_id(&self) -> String {
        let chars: Vec<char> = self.project_id.chars().collect();
        if chars.len() <= 8 {
            return "*".repeat(chars.len());
        }
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 2..].iter().collect();
        format!("{head}…{tail}")
    }

    /// Значение для системного хранилища: JSON `{"projectId", "serviceToken"}`.
    pub fn to_stored(&self) -> Zeroizing<String> {
        let stored = StoredPush {
            project_id: Some(self.project_id.clone()),
            service_token: Some(self.service_token.as_str().to_owned()),
        };
        // Сериализация структуры из строк не может завершиться ошибкой.
        Zeroizing::new(serde_json::to_string(&stored).unwrap_or_default())
    }

    /// Учётные данные из значения хранилища или файла секрета (`serve --http --push-file`).
    pub fn from_stored(text: &str) -> Result<Self, PushCredentialsError> {
        let stored: StoredPush =
            serde_json::from_str(text).map_err(|_| PushCredentialsError::Stored)?;
        match (
            stored.project_id.as_deref(),
            stored.service_token.as_deref(),
        ) {
            (Some(project), Some(token)) => Self::new(project, token),
            _ => Err(PushCredentialsError::Stored),
        }
    }

    /// Источник сервисного токена для исполнителя.
    pub fn token_source(&self) -> Arc<dyn TokenSource> {
        Arc::new(StaticToken::new(self.service_token.as_str()))
    }
}

/// Идентификатор проекта push: как в консоли (`myproject-b5ae1`,
/// `Lpvk29a2Al2P6tWu-a-aN3PKwO6-qvXZ`). Он становится сегментом пути, поэтому разделители,
/// `.` и `..` не допускаются.
pub fn is_valid_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_PROJECT_ID_LEN
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Настройки исполнителя для Push API.
///
/// `Debug` безопасен: источник токена его не раскрывает.
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// Проект push: подставляется в адрес сервиса (`/v1/projects/<проект>`).
    pub project_id: String,
    /// Сервисный токен проекта (обычно [`PushCredentials::token_source`]).
    pub token: Arc<dyn TokenSource>,
    /// Адрес сервиса отправки на устройство ([`PUSH_BASE_URL`]; тесты подменяют).
    pub base_url: url::Url,
    /// Адрес сервиса топиков ([`PUSH_TOPICS_BASE_URL`]; тесты подменяют).
    pub topics_base_url: url::Url,
}

impl PushConfig {
    /// Настройки по учётным данным профиля с адресами RuStore.
    pub fn new(credentials: &PushCredentials) -> Result<Self, ApiError> {
        let parse = |url: &str| {
            url::Url::parse(url).map_err(|e| ApiError::InvalidConfig(format!("push: {e}")))
        };
        Ok(Self {
            project_id: credentials.project_id().to_owned(),
            token: credentials.token_source(),
            base_url: parse(PUSH_BASE_URL)?,
            topics_base_url: parse(PUSH_TOPICS_BASE_URL)?,
        })
    }
}

/// Проверяет объём сообщения до запроса: компактный JSON поля `message` не больше
/// [`MAX_MESSAGE_BYTES`]. Тело без `message` не проверяется: его отклонит Push API.
pub fn check_message_size(body: &Value) -> Result<(), ApiError> {
    let Some(message) = body.get("message") else {
        return Ok(());
    };
    let size = serde_json::to_vec(message)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if size > MAX_MESSAGE_BYTES {
        return Err(ApiError::PushMessageTooLarge {
            size,
            limit: MAX_MESSAGE_BYTES,
        });
    }
    Ok(())
}

/// Push-токены устройств из тела запроса (`message.token`, `push_tokens`): исполнитель
/// вырезает их из ответов и ошибок, если сервис их отразит.
pub fn request_secrets(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(token) = body.pointer("/message/token").and_then(Value::as_str) {
        out.push(token.to_owned());
    }
    if let Some(tokens) = body.get("push_tokens").and_then(Value::as_array) {
        out.extend(tokens.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    out.retain(|token| token.trim().len() >= 8);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn credentials_are_validated_and_hidden() {
        let creds = PushCredentials::new(" myproject-b5ae1 ", "ss-token-0123456789\n").unwrap();
        assert_eq!(creds.project_id(), "myproject-b5ae1");
        let debug = format!("{creds:?}");
        assert!(!debug.contains("ss-token"), "{debug}");
        assert_eq!(creds.masked_project_id(), "mypr…e1");
        let stored = creds.to_stored();
        assert_eq!(PushCredentials::from_stored(&stored).unwrap(), creds);

        for bad in ["", "a/b", "..", "проект", &"x".repeat(129)] {
            assert_eq!(
                PushCredentials::new(bad, "token").unwrap_err(),
                PushCredentialsError::ProjectId,
                "{bad}"
            );
        }
        assert_eq!(
            PushCredentials::new("p", "  ").unwrap_err(),
            PushCredentialsError::EmptyToken
        );
        for bad in ["two words", "line\nbreak", "токен"] {
            let err = PushCredentials::new("p", bad).unwrap_err();
            assert_eq!(err, PushCredentialsError::TokenFormat);
            assert!(!err.to_string().contains(bad));
        }
        let err = PushCredentials::from_stored("{\"serviceToken\":\"secret-value\"}").unwrap_err();
        assert!(!err.to_string().contains("secret-value"));
    }

    #[test]
    fn message_size_is_limited() {
        let body = |text: &str| json!({"message": {"token": "t", "data": {"k": text}}});
        assert!(check_message_size(&body("short")).is_ok());
        // Обрамление `{"data":{"k":""},"token":"t"}` — 29 байт.
        let frame = serde_json::to_vec(&body("")["message"]).unwrap().len();
        assert_eq!(frame, 29);
        assert!(check_message_size(&body(&"a".repeat(MAX_MESSAGE_BYTES - frame))).is_ok());
        let err =
            check_message_size(&body(&"a".repeat(MAX_MESSAGE_BYTES - frame + 1))).unwrap_err();
        assert!(
            matches!(err, ApiError::PushMessageTooLarge { size, limit } if size == MAX_MESSAGE_BYTES + 1 && limit == MAX_MESSAGE_BYTES),
            "{err:?}"
        );
        // Кириллица — по два байта на символ.
        assert!(check_message_size(&body(&"я".repeat(2100))).is_err());
        assert!(check_message_size(&json!({"push_tokens": ["a"]})).is_ok());
    }

    #[test]
    fn device_tokens_are_collected() {
        let body = json!({"message": {"token": "device-token-1234"}});
        assert_eq!(request_secrets(&body), ["device-token-1234"]);
        let body = json!({"push_tokens": ["device-token-aaaa", "short", 1]});
        assert_eq!(request_secrets(&body), ["device-token-aaaa"]);
    }
}
