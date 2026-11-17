//! Ключ RuStore API: идентификатор `keyId` и приватный ключ RSA.
//!
//! Ключ создаётся в консоли RuStore (Компания → API RuStore): консоль показывает `keyId` и
//! приватный ключ строкой Base64 (DER, PKCS#8). Принимаются ещё DER PKCS#1 в Base64 и оба
//! формата в PEM (`BEGIN PRIVATE KEY`, `BEGIN RSA PRIVATE KEY`): так ключ можно передать и
//! после конвертации инструментами вроде `openssl`. Внутри ключ всегда приводится к PKCS#8.
//!
//! Подпись запроса токена — SHA512withRSA (RSASSA-PKCS1-v1_5 с SHA-512) строки
//! `keyId + timestamp` в UTF-8, результат — Base64 ([`ApiKey::sign`]).
//!
//! В память процесса ключ попадает только как [`ApiKey`]: его `Debug` не раскрывает ни
//! приватный ключ, ни `keyId`, память Base64 и DER затирается при освобождении. `keyId`
//! секретом не является, но относится к кабинету конкретной компании, поэтому в логи он
//! тоже не пишется; пользователю в его терминале он показывается в маске
//! ([`KeyId::masked`]).
//!
//! Тексты ошибок разбора не содержат ни ключа, ни его фрагментов, ни `keyId`.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey};
use rsa::sha2::Sha512;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

/// Предел размера файла ключа. Ключ RSA 4096 в PEM — около 3,3 КБ.
pub const MAX_KEY_FILE_BYTES: usize = 16 * 1024;

/// Минимальный размер ключа RSA в битах.
pub const MIN_RSA_BITS: usize = 2048;

/// Предел длины `keyId`.
pub const MAX_KEY_ID_LEN: usize = 256;

/// Где в консоли RuStore создаются ключи (для текстов ошибок и подсказок).
pub const CONSOLE_HINT: &str = "RuStore console > Company > RuStore API";

/// Ошибки разбора и использования ключа. Тексты не содержат ни ключа, ни `keyId`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error(
        "the key ID is not valid: {0}. Copy the key ID (keyId) shown next to the key in the \
         {CONSOLE_HINT}"
    )]
    InvalidKeyId(String),
    #[error(
        "the private key is not valid: {0}. Expected the private key of a RuStore API key: \
         the Base64 text shown when the key is created in the {CONSOLE_HINT}, or the same key \
         in PEM"
    )]
    InvalidPrivateKey(String),
    /// Файл ключа (JSON с `keyId` и `privateKey`) не разобран.
    #[error("the key file is not valid: {0}")]
    InvalidKeyFile(String),
    /// Системные часы раньше 1970 года или далеко в будущем: время подписи не записать.
    #[error("the system clock is out of range, cannot sign the token request")]
    Clock,
    /// Подпись не удалась (на корректном ключе не случается).
    #[error("could not sign the token request with the private key: {0}")]
    Sign(String),
}

fn invalid_key(reason: impl Into<String>) -> KeyError {
    KeyError::InvalidPrivateKey(reason.into())
}

/// Идентификатор ключа RuStore API (`keyId`). `Debug` скрыт.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyId(String);

impl KeyId {
    /// Проверяет `keyId`: 1–[`MAX_KEY_ID_LEN`] видимых символов ASCII без кавычек и обратной
    /// косой. Пробелы по краям отбрасываются.
    pub fn new(value: &str) -> Result<Self, KeyError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(KeyError::InvalidKeyId("it is empty".into()));
        }
        if value.contains("PRIVATE KEY") || value.len() > 200 && value.starts_with("MII") {
            return Err(KeyError::InvalidKeyId(
                "it looks like the private key, not its ID".into(),
            ));
        }
        if value.len() > MAX_KEY_ID_LEN {
            return Err(KeyError::InvalidKeyId(format!(
                "it is longer than {MAX_KEY_ID_LEN} characters"
            )));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
        {
            return Err(KeyError::InvalidKeyId(
                "it may contain only visible ASCII characters without spaces and quotes".into(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `keyId` в маске для вывода пользователю: первые и последние символы.
    pub fn masked(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        if chars.len() <= 8 {
            let visible: String = chars.iter().take(2).collect();
            return format!("{visible}***");
        }
        let head: String = chars.iter().take(4).collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}***{tail}")
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyId(***)")
    }
}

/// Ключ RuStore API с проверенным приватным ключом RSA.
///
/// Создаётся через [`ApiKey::new`], [`ApiKey::parse_key_file`] или
/// [`ApiKey::from_stored`]. `Debug` не раскрывает ни ключ, ни `keyId`.
#[derive(Clone)]
pub struct ApiKey {
    key_id: KeyId,
    /// Приватный ключ в PKCS#8 DER, Base64 (так его показывает консоль RuStore).
    private_key_base64: Zeroizing<String>,
    signing_key: SigningKey<Sha512>,
    bits: usize,
}

/// Запись ключа в системном хранилище и в файле ключа: `{"keyId", "privateKey"}`. Файл
/// принимается и в формате Gradle-плагина публикации RuStore (`{"key_id", "client_secret"}`,
/// его же пишет `key exec`).
#[derive(Serialize, Deserialize)]
struct StoredKey {
    #[serde(rename = "keyId", alias = "key_id")]
    key_id: Option<String>,
    #[serde(rename = "privateKey", alias = "private_key", alias = "client_secret")]
    private_key: Option<String>,
}

impl Drop for StoredKey {
    fn drop(&mut self) {
        if let Some(key) = &mut self.private_key {
            key.zeroize();
        }
    }
}

impl ApiKey {
    /// Ключ из `keyId` и текста приватного ключа (Base64 DER PKCS#8 или PKCS#1, либо PEM).
    pub fn new(key_id: &str, private_key: &str) -> Result<Self, KeyError> {
        let key_id = KeyId::new(key_id)?;
        let (private_key_base64, signing_key, bits) = parse_private_key(private_key)?;
        Ok(Self {
            key_id,
            private_key_base64,
            signing_key,
            bits,
        })
    }

    /// Ключ из содержимого файла ключа (`auth login --key-file`, `serve --http --key-file`):
    ///
    /// - JSON `{"keyId": "...", "privateKey": "..."}` (или `{"key_id", "client_secret"}`,
    ///   как у Gradle-плагина публикации RuStore) — `keyId` из файла; `key_id`, если задан,
    ///   должен с ним совпасть;
    /// - только приватный ключ (Base64 или PEM) — `keyId` обязателен в `key_id`.
    ///
    /// Возвращает `Ok(None)`, если в файле только приватный ключ, а `key_id` не задан:
    /// вызывающий может спросить `keyId` и повторить с ним.
    pub fn parse_key_file(text: &str, key_id: Option<&str>) -> Result<Option<Self>, KeyError> {
        if text.len() > MAX_KEY_FILE_BYTES {
            return Err(KeyError::InvalidKeyFile(format!(
                "it is larger than {MAX_KEY_FILE_BYTES} bytes"
            )));
        }
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.starts_with('{') {
            let stored: StoredKey = serde_json::from_str(text).map_err(describe_json_error)?;
            let Some(private_key) = stored.private_key.as_deref() else {
                return Err(KeyError::InvalidKeyFile(
                    "the JSON has no `privateKey` field".into(),
                ));
            };
            let key_id = match (stored.key_id.as_deref(), key_id) {
                (Some(stored), Some(given)) => {
                    if KeyId::new(stored)? != KeyId::new(given)? {
                        return Err(KeyError::InvalidKeyFile(
                            "the `keyId` in the file differs from the key ID given with \
                             --key-id"
                                .into(),
                        ));
                    }
                    stored
                }
                (Some(stored), None) => stored,
                (None, Some(given)) => given,
                (None, None) => return Ok(None),
            };
            return Self::new(key_id, private_key).map(Some);
        }
        // Только приватный ключ: проверяем его сразу, чтобы не спрашивать keyId впустую.
        let (private_key_base64, signing_key, bits) = parse_private_key(text)?;
        let Some(key_id) = key_id else {
            return Ok(None);
        };
        Ok(Some(Self {
            key_id: KeyId::new(key_id)?,
            private_key_base64,
            signing_key,
            bits,
        }))
    }

    /// Ключ из записи системного хранилища ([`Self::to_stored`]).
    pub fn from_stored(text: &str) -> Result<Self, KeyError> {
        let stored: StoredKey = serde_json::from_str(text).map_err(describe_json_error)?;
        match (stored.key_id.as_deref(), stored.private_key.as_deref()) {
            (Some(key_id), Some(private_key)) => Self::new(key_id, private_key),
            _ => Err(KeyError::InvalidKeyFile(
                "the stored value has no `keyId` or `privateKey`".into(),
            )),
        }
    }

    /// Запись для системного хранилища: JSON `{"keyId", "privateKey"}` (приватный ключ —
    /// Base64 DER PKCS#8). Только для записи в хранилище; не выводить и не логировать.
    pub fn to_stored(&self) -> Zeroizing<String> {
        let stored = StoredKey {
            key_id: Some(self.key_id.as_str().to_owned()),
            private_key: Some(self.private_key_base64.as_str().to_owned()),
        };
        // Сериализация структуры из строк не может завершиться ошибкой.
        Zeroizing::new(serde_json::to_string(&stored).unwrap_or_default())
    }

    pub fn key_id(&self) -> &KeyId {
        &self.key_id
    }

    /// Размер ключа RSA в битах.
    pub fn bits(&self) -> usize {
        self.bits
    }

    /// Приватный ключ в Base64 (DER PKCS#8). Нужен только для записи в хранилище и
    /// вырезания из текстов; не выводить.
    pub fn expose_private_key_base64(&self) -> &str {
        &self.private_key_base64
    }

    /// Подпись SHA512withRSA сообщения в Base64 (стандартный алфавит с `=`).
    pub fn sign(&self, message: &[u8]) -> Result<Zeroizing<String>, KeyError> {
        let signature = self
            .signing_key
            .try_sign(message)
            .map_err(|err| KeyError::Sign(err.to_string()))?;
        Ok(Zeroizing::new(STANDARD.encode(signature.to_bytes())))
    }

    /// Подпись запроса токена: SHA512withRSA строки `keyId + timestamp`.
    pub fn sign_token_request(&self, timestamp: &str) -> Result<Zeroizing<String>, KeyError> {
        let mut message = Zeroizing::new(String::with_capacity(
            self.key_id.as_str().len() + timestamp.len(),
        ));
        message.push_str(self.key_id.as_str());
        message.push_str(timestamp);
        self.sign(message.as_bytes())
    }
}

impl PartialEq for ApiKey {
    fn eq(&self, other: &Self) -> bool {
        self.key_id == other.key_id && self.private_key_base64 == other.private_key_base64
    }
}

impl Eq for ApiKey {}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKey")
            .field("key_id", &self.key_id)
            .field("bits", &self.bits)
            .finish_non_exhaustive()
    }
}

/// Описание ошибки JSON без фрагментов текста: только вид и позиция.
fn describe_json_error(err: serde_json::Error) -> KeyError {
    use serde_json::error::Category;
    let what = match err.classify() {
        Category::Syntax | Category::Eof | Category::Io => "it is not valid JSON",
        Category::Data => "it is not a JSON object with string fields `keyId` and `privateKey`",
    };
    KeyError::InvalidKeyFile(format!(
        "{what} (line {}, column {})",
        err.line(),
        err.column()
    ))
}

/// Разбирает текст приватного ключа. Возвращает Base64 DER PKCS#8, ключ подписи и размер.
fn parse_private_key(
    text: &str,
) -> Result<(Zeroizing<String>, SigningKey<Sha512>, usize), KeyError> {
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Err(invalid_key("it is empty"));
    }
    let der = if text.contains("-----BEGIN") {
        pem_to_der(text)?
    } else {
        decode_base64(text)?
    };
    let key = match rsa::PrivateKey::from_pkcs8_der(&der) {
        Ok(key) => key,
        Err(pkcs8_err) => match rsa::PrivateKey::from_pkcs1_der(&der) {
            Ok(key) => key,
            Err(_) if rsa::PublicKey::from_public_key_der(&der).is_ok() => {
                return Err(invalid_key("it is a public key; the private key is needed"));
            }
            // Описание — только вид ошибки ASN.1, без байтов ключа.
            Err(_) => {
                return Err(invalid_key(format!(
                    "it is not an RSA private key in PKCS#8 or PKCS#1 ({pkcs8_err})"
                )));
            }
        },
    };
    let bits = key.size() * 8;
    if bits < MIN_RSA_BITS {
        return Err(invalid_key(format!(
            "it is a {bits}-bit RSA key; at least {MIN_RSA_BITS} bits are required"
        )));
    }
    let pkcs8 = key
        .to_pkcs8_der()
        .map_err(|err| invalid_key(format!("it cannot be encoded as PKCS#8 ({err})")))?;
    let base64 = Zeroizing::new(STANDARD.encode(pkcs8.as_bytes()));
    Ok((base64, SigningKey::<Sha512>::new(key), bits))
}

/// Base64 без пробелов и переводов строк (консоль может показать ключ с переносами).
fn decode_base64(text: &str) -> Result<Zeroizing<Vec<u8>>, KeyError> {
    let compact: Zeroizing<String> =
        Zeroizing::new(text.chars().filter(|c| !c.is_whitespace()).collect());
    if compact.starts_with('{') {
        return Err(invalid_key(
            "it is JSON; pass the key file itself, or only the private key",
        ));
    }
    STANDARD
        .decode(compact.as_bytes())
        .map(Zeroizing::new)
        .map_err(|_| invalid_key("it is neither Base64 nor PEM"))
}

/// Тело единственного PEM-блока приватного ключа RSA в DER.
fn pem_to_der(text: &str) -> Result<Zeroizing<Vec<u8>>, KeyError> {
    const DASHES: &str = "-----";
    if text.matches("-----BEGIN").count() != 1 {
        return Err(invalid_key("it contains more than one PEM block"));
    }
    let begin = text.find("-----BEGIN ").unwrap_or(0) + "-----BEGIN ".len();
    let Some(label_len) = text[begin..].find(DASHES) else {
        return Err(invalid_key("the PEM header is broken"));
    };
    let label = &text[begin..begin + label_len];
    match label {
        "PRIVATE KEY" | "RSA PRIVATE KEY" => {}
        "ENCRYPTED PRIVATE KEY" => {
            return Err(invalid_key(
                "it is encrypted with a password; export it without encryption",
            ));
        }
        "PUBLIC KEY" | "RSA PUBLIC KEY" | "CERTIFICATE" => {
            return Err(invalid_key(format!(
                "it is a PEM `{label}`; the private key is needed"
            )));
        }
        _ => {
            return Err(invalid_key(
                "it is not a PEM block `PRIVATE KEY` or `RSA PRIVATE KEY`",
            ));
        }
    }
    let body_start = begin + label_len + DASHES.len();
    let end_marker = format!("{DASHES}END {label}{DASHES}");
    let Some(body_len) = text[body_start..].find(&end_marker) else {
        return Err(invalid_key("the PEM block has no END line"));
    };
    let body = &text[body_start..body_start + body_len];
    if body.contains(':') {
        return Err(invalid_key(
            "the PEM block has headers (it is probably encrypted); export it without encryption",
        ));
    }
    decode_base64(body)
}

/// Ключи для тестов: генерируются при выполнении, в репозитории ключей нет.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::sync::OnceLock;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rsa::pkcs8::EncodePrivateKey;

    use super::ApiKey;

    /// Фиктивный `keyId` тестов.
    pub const TEST_KEY_ID: &str = "test-key-id-0001";

    /// Один ключ RSA 2048 (как у ключей RuStore API) на процесс тестов: генерация
    /// занимает заметное время.
    pub fn rsa_key() -> &'static rsa::PrivateKey {
        static KEY: OnceLock<rsa::PrivateKey> = OnceLock::new();
        KEY.get_or_init(|| {
            rsa::PrivateKey::new(&mut getrandom::SysRng, 2048)
                .unwrap_or_else(|err| panic!("RSA key: {err}"))
        })
    }

    /// Приватный ключ в том виде, как его показывает консоль RuStore: Base64 DER PKCS#8.
    pub fn private_key_base64() -> String {
        let der = rsa_key()
            .to_pkcs8_der()
            .unwrap_or_else(|err| panic!("PKCS#8: {err}"));
        STANDARD.encode(der.as_bytes())
    }

    /// Ключ с [`TEST_KEY_ID`].
    pub fn api_key() -> ApiKey {
        ApiKey::new(TEST_KEY_ID, &private_key_base64())
            .unwrap_or_else(|err| panic!("test key: {err}"))
    }

    /// Открытый ключ тестового ключа.
    pub fn public_key() -> rsa::PublicKey {
        rsa_key().to_public_key()
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey};
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::pkcs8::{EncodePublicKey, LineEnding};
    use rsa::signature::Verifier;

    use super::testing::{TEST_KEY_ID, private_key_base64, public_key, rsa_key};
    use super::*;

    fn verify(message: &str, signature: &str) {
        let bytes = STANDARD.decode(signature).unwrap();
        let signature = Signature::try_from(bytes.as_slice()).unwrap();
        VerifyingKey::<Sha512>::new(public_key())
            .verify(message.as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn signs_key_id_and_timestamp_with_sha512() {
        let key = ApiKey::new(TEST_KEY_ID, &private_key_base64()).unwrap();
        let timestamp = "2024-06-18T11:49:08.290+03:00";
        let signature = key.sign_token_request(timestamp).unwrap();
        verify(&format!("{TEST_KEY_ID}{timestamp}"), &signature);
        // PKCS#1 v1.5 детерминирована: та же строка — та же подпись.
        assert_eq!(*key.sign_token_request(timestamp).unwrap(), *signature);
        assert_eq!(key.bits(), 2048);
    }

    #[test]
    fn accepts_base64_and_pem_in_pkcs8_and_pkcs1() {
        let reference = ApiKey::new(TEST_KEY_ID, &private_key_base64()).unwrap();
        let pkcs1_der = rsa_key().to_pkcs1_der().unwrap();
        let pkcs1_base64 = STANDARD.encode(pkcs1_der.as_bytes());
        let pkcs8_pem = rsa_key().to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
        let pkcs1_pem = rsa_key()
            .to_pkcs1_pem(LineEnding::CRLF)
            .unwrap()
            .to_string();
        // Base64 с переносами строк, как при копировании из консоли.
        let wrapped: String = private_key_base64()
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        for text in [
            private_key_base64(),
            format!("\u{feff} {}\n", private_key_base64()),
            wrapped,
            pkcs1_base64,
            pkcs8_pem,
            pkcs1_pem,
        ] {
            let key = ApiKey::new(&format!(" {TEST_KEY_ID} "), &text).unwrap();
            // Всё приводится к одному виду: PKCS#8 DER в Base64.
            assert_eq!(key, reference);
            assert_eq!(key.expose_private_key_base64(), private_key_base64());
        }
    }

    #[test]
    fn rejects_bad_keys_without_echoing_them() {
        let base64 = private_key_base64();
        let fragment = &base64[100..140];
        let public_pem = public_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap()
            .to_string();
        let public_der = STANDARD.encode(public_key().to_pkcs1_der().unwrap().as_bytes());
        let broken = format!("{}AAAA{}", &base64[..200], &base64[204..]);
        let two = format!(
            "{}{}",
            rsa_key().to_pkcs8_pem(LineEnding::LF).unwrap().as_str(),
            rsa_key().to_pkcs8_pem(LineEnding::LF).unwrap().as_str()
        );
        let encrypted = concat!(
            "-----",
            "BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----",
            "END ENCRYPTED PRIVATE KEY-----\n"
        );
        let cases: Vec<(String, &str)> = vec![
            (String::new(), "empty"),
            ("not base64 at all!".into(), "neither Base64 nor PEM"),
            ("AAAA".into(), "not an RSA private key"),
            (broken, "not an RSA private key"),
            (public_pem, "PEM `PUBLIC KEY`"),
            (public_der, "not an RSA private key"),
            (encrypted.into(), "encrypted"),
            (two, "more than one PEM block"),
            ("{\"keyId\": \"x\"}".into(), "JSON"),
        ];
        for (text, expected) in cases {
            let err = ApiKey::new(TEST_KEY_ID, &text).unwrap_err().to_string();
            assert!(err.contains(expected), "{expected}: {err}");
            assert!(!err.contains(fragment), "{err}");
            assert!(!err.contains(TEST_KEY_ID), "{err}");
        }
    }

    #[test]
    fn rejects_small_keys() {
        let small = rsa::PrivateKey::new(&mut getrandom::SysRng, 1024).unwrap();
        let der = small.to_pkcs8_der().unwrap();
        let err = ApiKey::new(TEST_KEY_ID, &STANDARD.encode(der.as_bytes()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("1024-bit"), "{err}");
    }

    #[test]
    fn validates_key_ids() {
        for bad in [
            "",
            "  ",
            "a b",
            "a\"b",
            "ключ",
            &"x".repeat(MAX_KEY_ID_LEN + 1),
        ] {
            assert!(
                matches!(KeyId::new(bad), Err(KeyError::InvalidKeyId(_))),
                "{bad:?}"
            );
        }
        let err = KeyId::new(&private_key_base64()).unwrap_err().to_string();
        assert!(err.contains("private key, not its ID"), "{err}");
        assert!(!err.contains(&private_key_base64()[..40]), "{err}");
        let id = KeyId::new(" 1234567890abcdef ").unwrap();
        assert_eq!(id.as_str(), "1234567890abcdef");
        assert_eq!(id.masked(), "1234***cdef");
        assert_eq!(KeyId::new("abc").unwrap().masked(), "ab***");
        assert_eq!(format!("{id:?}"), "KeyId(***)");
    }

    #[test]
    fn key_files_and_stored_values() {
        let base64 = private_key_base64();
        // JSON с keyId и privateKey.
        let json = serde_json::json!({"keyId": TEST_KEY_ID, "privateKey": base64}).to_string();
        let key = ApiKey::parse_key_file(&json, None).unwrap().unwrap();
        assert_eq!(key.key_id().as_str(), TEST_KEY_ID);
        assert_eq!(
            ApiKey::parse_key_file(&json, Some(TEST_KEY_ID)).unwrap(),
            Some(key.clone())
        );
        let err = ApiKey::parse_key_file(&json, Some("other-id"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("differs"), "{err}");
        // Файл Gradle-плагина публикации RuStore.
        let gradle =
            serde_json::json!({"key_id": TEST_KEY_ID, "client_secret": base64}).to_string();
        assert_eq!(
            ApiKey::parse_key_file(&gradle, None).unwrap(),
            Some(key.clone())
        );
        // Только приватный ключ: keyId нужен отдельно.
        assert_eq!(ApiKey::parse_key_file(&base64, None).unwrap(), None);
        assert_eq!(
            ApiKey::parse_key_file(&base64, Some(TEST_KEY_ID)).unwrap(),
            Some(key.clone())
        );
        // Сломанный ключ сообщается сразу, а не после вопроса о keyId.
        assert!(ApiKey::parse_key_file("AAAA", None).is_err());
        let no_key = serde_json::json!({"keyId": TEST_KEY_ID}).to_string();
        let err = ApiKey::parse_key_file(&no_key, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("privateKey"), "{err}");
        let err = ApiKey::parse_key_file("{\"keyId\": ", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid JSON"), "{err}");
        let err = ApiKey::parse_key_file(&"x".repeat(MAX_KEY_FILE_BYTES + 1), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("larger than"), "{err}");

        // Запись хранилища.
        let stored = key.to_stored();
        assert!(stored.contains("\"keyId\""), "{}", *stored);
        assert_eq!(ApiKey::from_stored(&stored).unwrap(), key);
        assert!(ApiKey::from_stored("{}").is_err());
        assert!(ApiKey::from_stored("nope").is_err());
    }

    #[test]
    fn debug_hides_values() {
        let key = ApiKey::new(TEST_KEY_ID, &private_key_base64()).unwrap();
        let debug = format!("{key:?}");
        assert!(!debug.contains(TEST_KEY_ID), "{debug}");
        assert!(!debug.contains(&private_key_base64()[..40]), "{debug}");
        assert!(debug.contains("ApiKey"), "{debug}");
    }
}
