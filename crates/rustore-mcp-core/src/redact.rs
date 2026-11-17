//! Вырезание значений, похожих на учётные данные, из ответов API.
//!
//! Ключ RuStore API и токен доступа сервер в ответы не кладёт сам, но они могут оказаться в
//! тексте: сервер может отразить заголовок `Public-Token` в ошибке, а пользователь — вставить
//! ключ в описание приложения или комментарий к версии. Агенту эти значения не нужны, а в его
//! контексте они опасны: оттуда они легко попадают в логи, коммиты и документацию. Поэтому во
//! всех ответах — успешных и ошибках, JSON и текстовых — значения заменяются на [`REDACTED`],
//! а к результату добавляется пометка ([`Redactions::note`]).
//!
//! Правила:
//!
//! 1. **Поля по имени** ([`is_credential_key`]): строковое значение поля, имя которого похоже
//!    на пароль, токен, ключ, подпись или заголовок авторизации ([`CREDENTIAL_KEYS`]: в том
//!    числе `jwe`, `privateKey`, `signature`, `serviceToken`, `Public-Token`), на любой
//!    глубине. Исключения ([`VISIBLE_SUFFIXES`]) — признаки (`has_password`, `isSecret`),
//!    пустые строки и поля платежей и постраничной выдачи (`pageToken`, `continuationToken`),
//!    которые нужны агенту как есть.
//! 2. **Значения по форме** ([`redact_secret_values`]) в любой строке: приватные ключи RSA в
//!    PEM и в Base64 (DER, начинаются с `MII`), токены JWE и JWT, значения `Public-Token`,
//!    `Bearer` и `Basic`, пароли в URL (`https://user:пароль@host`). Заменяется только сам
//!    секрет, остальной текст остаётся.
//! 3. **Параметры query в ссылках** ([`redact_url_query`]): значения параметров с именами
//!    учётных данных (`token`, `access_token`, `signature`, `X-Amz-Signature`, `sig`, `key`,
//!    `X-Goog-Credential` и т. п.) в адресах `scheme://host/...?...`. Ссылки на файлы
//!    хранилища RuStore (`fileUrl`, `iconUrl`) сейчас без подписи, но подписанная ссылка — это
//!    доступ к файлу, и агенту её значение не нужно. Имя параметра и остальная ссылка
//!    остаются.
//!
//! Push-токены устройств (`token` сообщения push, `push_token` в ошибках подписки на топик,
//! массивы `push_tokens`) — адреса живых людей для push-уведомлений: они скрываются по имени
//! поля, в том числе строки внутри массива с таким именем. Сервисный токен push и токены
//! устройств из запроса дополнительно вырезает исполнитель по точному значению.
//!
//! Токены покупок и подписок (`purchaseToken`, `subscriptionToken`) и номера счетов
//! (`invoiceId`) — не секреты доступа: это идентификаторы платежей, по которым агент
//! проверяет покупку или подписку в RuStore API. Без них методы платежей бесполезны, поэтому
//! такие поля не скрываются ни по имени, ни по форме.
//!
//! Для текстовых тел (не JSON) правило 1 применяется к парам `"ключ": "строка"` в тексте,
//! в том числе к незакрытой строке в конце (обрезанный JSON), правило 2 — ко всему тексту.
//!
//! Функции получают относительный путь запроса (для будущих правил по путям); сейчас правила
//! от пути не зависят. От `OperationDef` они не зависят, поэтому работают и для
//! сгенерированных инструментов, и для `api_request`.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use rustore_api::executor::{ApiError, ApiResponse, ResponseBody};
use serde_json::Value;

/// Чем заменяются скрытые значения.
pub const REDACTED: &str = "[redacted]";

/// Имена полей с учётными данными. Совпадение — имя целиком или суффикс после `_`
/// (`db_password`, `service_token`, `x_api_key`) без учёта регистра; camelCase, `-`, `.` и
/// пробелы приводятся к `_`.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "token",
    // Массивы токенов: `push_tokens` подписки на топик push.
    "tokens",
    "jwe",
    "jwt",
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
    "client_secret",
    "credentials",
    "signature",
    "authorization",
    "cookie",
];

/// Суффиксы имён, которые совпадают с [`CREDENTIAL_KEYS`], но не являются учётными данными
/// и не проверяются по форме: идентификаторы покупок, подписок и постраничной выдачи нужны
/// агенту как есть (см. описание модуля).
pub const VISIBLE_SUFFIXES: &[&str] = &[
    "purchase_token",
    "subscription_token",
    "invoice_id",
    "page_token",
    "next_token",
    // `continuationToken` списка приложений и продуктового каталога: без него не получить
    // следующую страницу.
    "continuation_token",
];

/// Префиксы флагов (`has_password`, `is_secret`): это признаки, а не значения.
const FLAG_PREFIXES: &[&str] = &["has_", "is_"];

/// Что было скрыто в одном ответе.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions {
    /// Имена полей, значения которых скрыты целиком (правило 1).
    pub credential_fields: BTreeSet<String>,
    /// Сколько значений, похожих на секреты по форме, скрыто внутри строк (правило 2).
    pub secret_values: usize,
}

impl Redactions {
    pub fn is_empty(&self) -> bool {
        self.credential_fields.is_empty() && self.secret_values == 0
    }

    fn merge(&mut self, other: Redactions) {
        self.credential_fields.extend(other.credential_fields);
        self.secret_values += other.secret_values;
    }

    /// Пояснение для агента; `None`, если ничего не скрыто.
    pub fn note(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.credential_fields.is_empty() {
            let fields: Vec<&str> = self.credential_fields.iter().map(String::as_str).collect();
            parts.push(format!(
                "Values of fields that look like credentials were replaced with \"{REDACTED}\": \
                 {}.",
                fields.join(", ")
            ));
        }
        if self.secret_values > 0 {
            parts.push(format!(
                "{} value(s) that look like private keys, tokens or passwords (RSA private keys, \
                 JWE or JWT tokens, Public-Token or Authorization values, passwords and \
                 credential query parameters in URLs) \
                 were replaced with \"{REDACTED}\" inside the text.",
                self.secret_values
            ));
        }
        Some(format!(
            "[{} The server never shows them; do not ask the user for them, and do not try to \
             recover them in other ways.]",
            parts.join(" ")
        ))
    }
}

/// Похоже ли имя поля на поле с учётными данными (см. [`CREDENTIAL_KEYS`]).
pub fn is_credential_key(key: &str) -> bool {
    let key = normalize_key(key);
    if FLAG_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) || is_visible_normalized(&key) {
        return false;
    }
    CREDENTIAL_KEYS.iter().any(|candidate| {
        key == *candidate
            || key
                .strip_suffix(candidate)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

/// Поле, которое не скрывается и не проверяется по форме ([`VISIBLE_SUFFIXES`]).
pub fn is_visible_key(key: &str) -> bool {
    is_visible_normalized(&normalize_key(key))
}

fn is_visible_normalized(key: &str) -> bool {
    VISIBLE_SUFFIXES.iter().any(|suffix| {
        key == *suffix
            || key
                .strip_suffix(suffix)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

/// `accessToken`, `Public-Token` → `access_token`, `public_token`.
fn normalize_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous: Option<char> = None;
    for c in key.trim().chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            normalized.push('_');
        }
        if matches!(c, '-' | ' ' | '.') {
            normalized.push('_');
        } else {
            normalized.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    normalized
}

/// Вырезает значения из JSON-ответа запроса по пути `path`.
pub fn redact_json(_path: &str, json: &mut Value) -> Redactions {
    let mut redactions = Redactions::default();
    walk(json, &mut redactions);
    redactions
}

fn walk(value: &mut Value, redactions: &mut Redactions) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if is_visible_key(key) {
                    continue;
                }
                if let Value::String(text) = item {
                    redact_field(key, text, redactions);
                    continue;
                }
                // Массив строк под именем учётных данных (`push_tokens`): скрывается каждая.
                if let Value::Array(items) = item
                    && is_credential_key(key)
                    && items.iter().all(Value::is_string)
                {
                    for text in items.iter_mut().filter_map(|v| match v {
                        Value::String(text) => Some(text),
                        _ => None,
                    }) {
                        redact_field(key, text, redactions);
                    }
                    continue;
                }
                walk(item, redactions);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, redactions);
            }
        }
        Value::String(text) => {
            redactions.secret_values += redact_secret_values_in_place(text);
        }
        _ => {}
    }
}

/// Строковое значение поля `key`.
fn redact_field(key: &str, text: &mut String, redactions: &mut Redactions) {
    if text.is_empty() {
        return;
    }
    if is_credential_key(key) {
        if text != REDACTED {
            *text = REDACTED.to_owned();
            redactions.credential_fields.insert(key.to_owned());
        }
        return;
    }
    redactions.secret_values += redact_secret_values_in_place(text);
}

/// Значения по форме (правило 2): регулярные выражения и проверка найденного значения.
struct SecretPattern {
    regex: Regex,
    /// Группа с секретом; остальной текст совпадения сохраняется.
    group: usize,
    /// Дополнительная проверка секрета: отсекает похожие обычные слова.
    accept: fn(&str) -> bool,
}

fn any_value(_: &str) -> bool {
    true
}

/// Значение после `Bearer`: токены содержат цифры, обычные слова — нет.
fn has_digit(value: &str) -> bool {
    value.chars().any(|c| c.is_ascii_digit())
}

/// Значение после `Basic`: base64 (длина кратна 4) с цифрой или выравниванием `=`.
fn looks_like_base64(value: &str) -> bool {
    value.len().is_multiple_of(4) && (value.ends_with('=') || has_digit(value))
}

static SECRET_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
    let pattern = |source: &str, group: usize, accept: fn(&str) -> bool| {
        Regex::new(source).ok().map(|regex| SecretPattern {
            regex,
            group,
            accept,
        })
    };
    [
        // Блок закрытого ключа PEM (до конца блока или до конца текста, если он обрезан).
        pattern(
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
            0,
            any_value,
        ),
        // Приватный ключ RSA в Base64 без PEM (так его показывает консоль RuStore): DER
        // `SEQUENCE` длиной 256–65535 байт (`MII` и ещё три символа), затем версия 0 и либо
        // `AlgorithmIdentifier` (PKCS#8, `IBADAN`), либо модуль (PKCS#1, `IBAAK`). Ключ может
        // быть разбит на строки: следующая строка — продолжение ключа, если она длиннее 15
        // символов Base64 или кончается выравниванием `=` (так обычный текст после ключа
        // не скрывается).
        pattern(
            r"\bMII[A-Za-z0-9+/]{3}IBA(?:DAN|AK)[A-Za-z0-9+/]{40,}={0,2}(?:\r?\n(?:[A-Za-z0-9+/]{16,}={0,2}|[A-Za-z0-9+/]{1,15}={1,2}))*",
            0,
            any_value,
        ),
        // JWE в компактной форме (токен доступа RuStore API): пять частей base64url через
        // точку, заголовок — JSON (`eyJ`); зашифрованный ключ бывает пустым.
        pattern(
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
            any_value,
        ),
        // JWT.
        pattern(
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
            any_value,
        ),
        // Значение заголовка `Public-Token` в тексте (`Public-Token: …`, `public-token=…`).
        pattern(
            r#"(?i)\bpublic[-_]token["']?\s*[:=]\s*["']?([A-Za-z0-9._~+/=-]{16,})"#,
            1,
            any_value,
        ),
        // Значение заголовка авторизации: схема остаётся, токен скрывается.
        pattern(
            r"\bBearer\s+([A-Za-z0-9._~+/=-][A-Za-z0-9._~+/=_-]{15,})",
            1,
            has_digit,
        ),
        pattern(r"\bBasic\s+([A-Za-z0-9+/]{8,}={0,2})", 1, looks_like_base64),
        // Пароль в URL: `scheme://user:пароль@host`.
        pattern(
            r#"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@"'<>]+:([^\s/@"'<>]+)@"#,
            1,
            any_value,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// Заменяет в тексте значения, похожие на секреты по форме (правило 2). Возвращает текст
/// и число замен.
pub fn redact_secret_values(text: &str) -> (String, usize) {
    let mut current = text.to_owned();
    let count = redact_secret_values_in_place(&mut current);
    (current, count)
}

fn redact_secret_values_in_place(text: &mut String) -> usize {
    // Быстрый выход для обычных строк: у всех шаблонов есть одна из этих подстрок.
    const MARKERS: &[&str] = &[
        "PRIVATE KEY",
        "MII",
        "eyJ",
        "ublic-",
        "ublic_",
        "UBLIC-",
        "UBLIC_",
        "Bearer",
        "Basic",
        "://",
    ];
    if !MARKERS.iter().any(|marker| text.contains(marker)) {
        return 0;
    }
    let mut count = 0;
    for pattern in SECRET_PATTERNS.iter() {
        let replaced = pattern.regex.replace_all(text, |caps: &Captures<'_>| {
            let whole = caps.get(0).map_or("", |m| m.as_str());
            let Some(secret) = caps.get(pattern.group) else {
                return whole.to_owned();
            };
            if secret.as_str() == REDACTED || !(pattern.accept)(secret.as_str()) {
                return whole.to_owned();
            }
            count += 1;
            let start = secret.start() - caps.get(0).map_or(0, |m| m.start());
            let end = start + secret.len();
            format!("{}{REDACTED}{}", &whole[..start], &whole[end..])
        });
        if let std::borrow::Cow::Owned(updated) = replaced {
            *text = updated;
        }
    }
    count + redact_url_query(text)
}

/// Имена параметров query, которые скрываются в ссылках сверх [`is_credential_key`]
/// (после приведения имени к snake_case): подписи и ключи подписанных ссылок хранилищ.
const URL_QUERY_KEYS: &[&str] = &["sig", "key", "auth", "credential"];

/// Параметр query с учётными данными (правило 3).
fn is_url_query_credential(name: &str) -> bool {
    if is_visible_key(name) {
        return false;
    }
    let normalized = normalize_key(name);
    is_credential_key(name)
        || URL_QUERY_KEYS.iter().any(|candidate| {
            normalized == *candidate
                || normalized
                    .strip_suffix(candidate)
                    .is_some_and(|rest| rest.ends_with('_'))
        })
}

static URL_QUERY: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s?#"'<>]*\?([^\s#"'<>]*)"#).ok());

/// Скрывает значения параметров query с учётными данными в ссылках (правило 3).
/// Возвращает число замен.
fn redact_url_query(text: &mut String) -> usize {
    let Some(regex) = URL_QUERY.as_ref() else {
        return 0;
    };
    if !text.contains("://") || !text.contains('?') {
        return 0;
    }
    let mut count = 0;
    let replaced = regex.replace_all(text, |caps: &Captures<'_>| {
        let whole = caps.get(0).map_or("", |m| m.as_str());
        let Some(query) = caps.get(1) else {
            return whole.to_owned();
        };
        let offset = query.start() - caps.get(0).map_or(0, |m| m.start());
        let params: Vec<String> = query
            .as_str()
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((name, value))
                    if !value.is_empty() && value != REDACTED && is_url_query_credential(name) =>
                {
                    count += 1;
                    format!("{name}={REDACTED}")
                }
                _ => pair.to_owned(),
            })
            .collect();
        format!("{}{}", &whole[..offset], params.join("&"))
    });
    if let std::borrow::Cow::Owned(updated) = replaced {
        *text = updated;
    }
    count
}

/// Вырезает значения из текстового тела: строковые значения пар `"ключ": "строка"` с ключами
/// учётных данных, в том числе незакрытая строка в конце (обрезанный JSON), и значения,
/// похожие на секреты по форме.
pub fn redact_text(_path: &str, text: &str) -> (String, Redactions) {
    let mut redactions = Redactions::default();
    let mut text = redact_credential_pairs(text, &mut redactions);
    redactions.secret_values += redact_secret_values_in_place(&mut text);
    (text, redactions)
}

fn redact_credential_pairs(text: &str, redactions: &mut Redactions) -> String {
    let bytes = text.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(offset) = text.get(i..).and_then(|rest| rest.find('"')) {
        let start = i + offset;
        let (key_end, after_key) = string_end(bytes, start);
        i = after_key;
        let colon = skip_ws(after_key);
        if bytes.get(colon) != Some(&b':') {
            continue;
        }
        let value_start = skip_ws(colon + 1);
        if bytes.get(value_start) != Some(&b'"') {
            continue;
        }
        let (value_end, after_value) = string_end(bytes, value_start);
        i = after_value;
        let key = text.get(start + 1..key_end).unwrap_or_default();
        let value = text.get(value_start + 1..value_end).unwrap_or_default();
        if !value.is_empty() && value != REDACTED && is_credential_key(key) {
            result.push_str(text.get(copied..value_start).unwrap_or_default());
            result.push('"');
            result.push_str(REDACTED);
            result.push('"');
            copied = after_value;
            redactions.credential_fields.insert(key.to_owned());
        }
    }
    result.push_str(text.get(copied..).unwrap_or_default());
    result
}

/// Для строки, открытой кавычкой в `start`: позиция закрывающей кавычки (или конец текста)
/// и позиция сразу после строки. Обе позиции — границы символов UTF-8: это `"` или конец.
fn string_end(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut j = start + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return (j, j + 1),
            _ => j += 1,
        }
    }
    (bytes.len(), bytes.len())
}

/// Вырезает значения из успешного ответа API и возвращает, что скрыто.
pub fn redact_response(path: &str, response: &mut ApiResponse) -> Redactions {
    match &mut response.body {
        ResponseBody::Json(json) => redact_json(path, json),
        ResponseBody::Text(text) => {
            let (redacted, redactions) = redact_text(path, text);
            *text = redacted;
            redactions
        }
        // Файл агент не видит, пустое тело скрывать нечего.
        ResponseBody::SavedToFile { .. } | ResponseBody::Empty => Redactions::default(),
    }
}

/// Ошибка API с вырезанными значениями в теле, `code` и `message` RuStore, и что скрыто.
pub fn redact_api_error(path: &str, err: ApiError) -> (ApiError, Redactions) {
    let ApiError::Http(mut http) = err else {
        return (err, Redactions::default());
    };
    let mut redactions = Redactions::default();
    http.body = match serde_json::from_str::<Value>(&http.body) {
        Ok(mut json) => {
            let found = redact_json(path, &mut json);
            if found.is_empty() {
                http.body
            } else {
                redactions.merge(found);
                json.to_string()
            }
        }
        Err(_) => {
            let (text, found) = redact_text(path, &http.body);
            redactions.merge(found);
            text
        }
    };
    for field in [&mut http.code, &mut http.message] {
        if let Some(value) = field.take() {
            let (text, found) = redact_text(path, &value);
            redactions.merge(found);
            *field = Some(text);
        }
    }
    (ApiError::Http(http), redactions)
}

/// Текст сообщения для агента (ошибки, не связанные с телом ответа): значения по форме и
/// пары `"ключ": "строка"`. Возвращает текст с пометкой, если что-то скрыто.
pub fn redact_message(text: &str) -> String {
    let (mut text, redactions) = redact_text("", text);
    if let Some(note) = redactions.note() {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    text
}

#[cfg(test)]
mod tests {
    use rustore_api::executor::{HttpError, Pagination};
    use rustore_api::testing::private_key_base64;
    use serde_json::json;

    use super::*;

    const APP: &str = "/public/v1/application/com.example.app";
    /// Заголовок, данные и подпись JWT по отдельности: целиком в исходниках они выглядели бы
    /// для сканеров секретов как настоящие.
    const JWT_PARTS: [&str; 3] = [
        "eyJhbGciOiJIUzI1NiJ9",
        "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
        "c2lnbmF0dXJlLXZhbHVl",
    ];
    /// Части JWE (RSA-OAEP, A256GCM): заголовок, ключ, вектор, шифротекст, тег.
    const JWE_PARTS: [&str; 5] = [
        "eyJhbGciOiJSU0EtT0FFUC0yNTYiLCJlbmMiOiJBMjU2R0NNIn0",
        "OKOawDo13gRp2ojaHV7LFpZcgV7T6DVZKTyKOMTYUmKoTCVJRgckCL9kiMT03JGe",
        "48V1_ALb6US04U3b",
        "5eym8TW_c8SuK0ltJ3rpYIzOeDQz7TALvtu6UG9oMo4vpzs9tX_EFShS8iB7j6ji",
        "XFBoMYUZodetZdvTiFvSkQ",
    ];
    const PEM_DASHES: &str = "-----";

    fn jwe() -> String {
        JWE_PARTS.join(".")
    }

    fn jwt() -> String {
        JWT_PARTS.join(".")
    }

    #[test]
    fn push_device_tokens_are_hidden_by_field() {
        let mut json = json!({
            "message": "1 error(s)",
            "errors": [{"push_token": "bk3RNwTe3H0:CI2k_HHwgIpoDKCIZvvD", "error": {"code": 1001, "status": "FORBIDDEN", "message": "s2s token invalid"}}],
            "push_tokens": ["kHrfTow3AjqdgUeCJLwbIHd7c0", "9mbJVS0-2e0cCcIx7X5nxMIgp8"],
            "tokenCount": 2
        });
        let redactions = redact_json("/topics/news/batch_subscribe", &mut json);
        assert_eq!(json["errors"][0]["push_token"], REDACTED);
        assert_eq!(json["push_tokens"], json!([REDACTED, REDACTED]));
        // Обычный текст и числа остаются.
        assert_eq!(json["errors"][0]["error"]["message"], "s2s token invalid");
        assert_eq!(json["tokenCount"], 2);
        assert!(redactions.credential_fields.contains("push_token"));
        assert!(redactions.credential_fields.contains("push_tokens"));
    }

    #[test]
    fn credential_keys() {
        for key in [
            "password",
            "Password",
            "db_pwd",
            "token",
            "accessToken",
            "serviceToken",
            "Public-Token",
            "public_token",
            "jwe",
            "client-secret",
            "Authorization",
            "privateKey",
            "private_key",
            "signature",
            "requestSignature",
            "apiKey",
            "X-Api-Key",
            "Set-Cookie",
            "credentials",
        ] {
            assert!(is_credential_key(key), "{key}");
        }
        for key in [
            "key",
            "keyId",
            "keys",
            "type",
            "has_password",
            "isSecret",
            "purchaseToken",
            "subscriptionToken",
            "subscription_token",
            "invoiceId",
            "nextPageToken",
            "pageToken",
            "continuationToken",
            "tokens_count",
            "token_type",
            "author",
            "tokenizer",
            "packageName",
        ] {
            assert!(!is_credential_key(key), "{key}");
        }
        assert!(is_visible_key("purchaseToken"));
        assert!(!is_visible_key("token"));
    }

    #[test]
    fn redacts_credential_fields_and_keeps_payment_ids() {
        let mut body = json!({
            "appName": "Example",
            "jwe": jwe(),
            "ttl": 900,
            "nested": [{"privateKey": "AAAA", "signature": "sig-1", "token": ""}],
            "purchaseToken": jwt(),
            "subscriptionToken": "1234.5678",
            "invoiceId": "inv-1",
            "keyId": "key-1"
        });
        let redactions = redact_json(APP, &mut body);
        assert_eq!(body["jwe"], REDACTED);
        assert_eq!(body["nested"][0]["privateKey"], REDACTED);
        assert_eq!(body["nested"][0]["signature"], REDACTED);
        // Пустое значение оставляем: скрывать нечего.
        assert_eq!(body["nested"][0]["token"], "");
        // Идентификаторы платежей не скрываются даже по форме.
        assert_eq!(body["purchaseToken"], jwt());
        assert_eq!(body["subscriptionToken"], "1234.5678");
        assert_eq!(body["invoiceId"], "inv-1");
        assert_eq!(body["keyId"], "key-1");
        assert_eq!(body["appName"], "Example");
        assert_eq!(
            redactions.credential_fields.iter().collect::<Vec<_>>(),
            ["jwe", "privateKey", "signature"]
        );
        assert_eq!(redactions.secret_values, 0);
        let note = redactions.note().unwrap();
        assert!(note.contains("jwe, privateKey, signature"), "{note}");
        assert!(Redactions::default().note().is_none());
    }

    #[test]
    fn secret_values_by_shape() {
        let key = private_key_base64();
        let wrapped: String = key
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let cases = [
            (
                format!("token {} in text", jwe()),
                "token [redacted] in text".to_owned(),
            ),
            (format!("jwt {}", jwt()), "jwt [redacted]".to_owned()),
            (format!("key: {key} end"), "key: [redacted] end".to_owned()),
            (
                format!("key:\n{wrapped}\nend"),
                "key:\n[redacted]\nend".to_owned(),
            ),
            (
                "Public-Token: abcdefABCDEF0123456789".to_owned(),
                "Public-Token: [redacted]".to_owned(),
            ),
            (
                r#"{"public-token" = "abcdefABCDEF0123456789"}"#.to_owned(),
                r#"{"public-token" = "[redacted]"}"#.to_owned(),
            ),
            (
                "Authorization: Bearer abcdef0123456789abcdef".to_owned(),
                "Authorization: Bearer [redacted]".to_owned(),
            ),
            (
                "Basic Ym90OnBAc3M=".to_owned(),
                "Basic [redacted]".to_owned(),
            ),
            (
                "clone https://bot:hunter2@git.example.test/repo.git".to_owned(),
                "clone https://bot:[redacted]@git.example.test/repo.git".to_owned(),
            ),
            (
                format!(
                    "key:\n{d}BEGIN PRIVATE KEY{d}\n{key}\n{d}END PRIVATE KEY{d}\nend",
                    d = PEM_DASHES
                ),
                "key:\n[redacted]\nend".to_owned(),
            ),
            (
                format!(
                    "{d}BEGIN RSA PRIVATE KEY{d}\nMIIE (обрезано",
                    d = PEM_DASHES
                ),
                "[redacted]".to_owned(),
            ),
        ];
        for (input, expected) in cases {
            let (text, count) = redact_secret_values(&input);
            assert_eq!(text, expected, "{input}");
            assert_eq!(count, 1, "{input}");
        }
        // Обычный текст не меняется.
        for plain in [
            "Basic authentication is used",
            "Bearer tokens are described in the docs",
            "Public-Token header is required",
            "https://example.test/path?x=1",
            "mailto:user@example.test",
            "MIIT is a university",
            "com.example.app",
            "dGVzdA==",
            "[redacted]",
        ] {
            assert_eq!(
                redact_secret_values(plain),
                (plain.to_owned(), 0),
                "{plain}"
            );
        }
    }

    #[test]
    fn credential_query_parameters_in_urls() {
        let (text, count) = redact_secret_values(
            "file https://static.rustore.ru/a/0.png?token=abc123&w=100&X-Amz-Signature=f00d \
             and https://static.rustore.ru/b.png?sig=1&key=2&X-Goog-Credential=3#top",
        );
        assert_eq!(count, 5, "{text}");
        assert_eq!(
            text,
            "file https://static.rustore.ru/a/0.png?token=[redacted]&w=100&\
             X-Amz-Signature=[redacted] and https://static.rustore.ru/b.png?sig=[redacted]&\
             key=[redacted]&X-Goog-Credential=[redacted]#top"
        );
        // Обычные параметры, параметры постраничной выдачи и текст без ссылок не меняются.
        for plain in [
            "https://static.rustore.ru/a/0.png",
            "https://static.rustore.ru/a/0.png?w=100&h=200",
            "https://public-api-m.rustore.ru/x?pageToken=abc&continuationToken=def",
            "token=abc without a link?",
        ] {
            assert_eq!(
                redact_secret_values(plain),
                (plain.to_owned(), 0),
                "{plain}"
            );
        }
        // В JSON-ответе — с пометкой.
        let mut json = json!({"fileUrl": "https://static.rustore.ru/a.png?access_token=xyz"});
        let found = redact_json(APP, &mut json);
        assert_eq!(found.secret_values, 1);
        assert_eq!(
            json["fileUrl"],
            "https://static.rustore.ru/a.png?access_token=[redacted]"
        );
    }

    #[test]
    fn secret_values_in_texts_of_the_app() {
        let mut body = json!({
            "packageName": "com.example.app",
            "whatsNew": format!("debug key {}", private_key_base64()),
            "comments": [format!("token {}", jwe())]
        });
        let redactions = redact_json(APP, &mut body);
        assert_eq!(body["whatsNew"], "debug key [redacted]");
        assert_eq!(body["comments"][0], "token [redacted]");
        assert_eq!(body["packageName"], "com.example.app");
        assert_eq!(redactions.secret_values, 2);
        assert!(redactions.credential_fields.is_empty());
        let note = redactions.note().unwrap();
        assert!(note.contains("2 value(s)"), "{note}");
    }

    #[test]
    fn redacts_text_and_truncated_json() {
        let (text, redactions) =
            redact_text(APP, r#"{"a": "b", "password" : "x\"y", "jwe": "tail"#);
        assert_eq!(
            text,
            r#"{"a": "b", "password" : "[redacted]", "jwe": "[redacted]""#
        );
        assert_eq!(redactions.credential_fields.len(), 2);
        let (text, redactions) = redact_text(APP, &format!("<html>{}</html>", jwe()));
        assert_eq!(text, "<html>[redacted]</html>");
        assert_eq!(redactions.secret_values, 1);
        let (plain, redactions) = redact_text(APP, "плain text without pairs");
        assert_eq!(plain, "плain text without pairs");
        assert!(redactions.is_empty());
    }

    #[test]
    fn redacts_responses_and_errors() {
        let mut response = ApiResponse {
            status: 200,
            body: ResponseBody::Json(json!({"secret": "s"})),
            pagination: Pagination::default(),
        };
        assert!(!redact_response(APP, &mut response).is_empty());
        assert_eq!(
            response.body,
            ResponseBody::Json(json!({"secret": REDACTED}))
        );

        let token = jwe();
        let err = ApiError::Http(Box::new(HttpError {
            status: 401,
            code: Some("ERROR".into()),
            message: Some(format!(r#"bad "password": "p1", Public-Token: {token}"#)),
            body: format!(r#"{{"code":"ERROR","message":"token {token}","jwe":"t1"}}"#),
            retry_after: None,
        }));
        let (ApiError::Http(http), redactions) = redact_api_error(APP, err) else {
            panic!("ожидалась ошибка HTTP");
        };
        assert!(
            !http.body.contains(&token) && !http.body.contains("t1"),
            "{}",
            http.body
        );
        let message = http.message.unwrap();
        assert!(
            !message.contains("p1") && !message.contains(&token),
            "{message}"
        );
        assert_eq!(http.code.as_deref(), Some("ERROR"));
        assert!(redactions.credential_fields.contains("password"));
        assert!(redactions.credential_fields.contains("jwe"));
        assert_eq!(redactions.secret_values, 2);

        // Тело без секретов возвращается как есть (без переформатирования).
        let err = ApiError::Http(Box::new(HttpError {
            status: 404,
            code: Some("ERROR".into()),
            message: Some("Application not found".into()),
            body: r#"{ "code": "ERROR", "message": "Application not found" }"#.into(),
            retry_after: None,
        }));
        let (ApiError::Http(http), redactions) = redact_api_error(APP, err) else {
            panic!("ожидалась ошибка HTTP");
        };
        assert_eq!(
            http.body,
            r#"{ "code": "ERROR", "message": "Application not found" }"#
        );
        assert!(redactions.is_empty());

        let (err, redactions) = redact_api_error(APP, ApiError::MissingToken);
        assert!(matches!(err, ApiError::MissingToken));
        assert!(redactions.is_empty());
    }

    #[test]
    fn messages_get_a_note() {
        let text = redact_message(&format!(
            "could not reach https://bot:pw1@example.test ({})",
            jwe()
        ));
        assert!(!text.contains("pw1") && !text.contains(&jwe()), "{text}");
        assert!(text.contains("2 value(s)"), "{text}");
        assert_eq!(redact_message("plain error"), "plain error");
    }
}
