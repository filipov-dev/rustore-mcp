//! Клиент RuStore Public API (`public-api-m.rustore.ru`).
//!
//! Операции описаны в OpenAPI-спецификации `spec/rustore.openapi.json`, которая
//! собрана из справочника API скриптом `spec/build_openapi.py`.
//!
//! - [`spec`] — операции API, сгенерированные из спецификации при сборке.
//! - [`executor`] — выполнение HTTP-запросов к API.
//! - [`path`] — проверка и сборка относительных путей API.
//! - [`key`] — ключ RuStore API (`keyId` и приватный ключ RSA) и подпись запроса токена.
//! - [`access`] — токены доступа (JWE) по ключу, только в памяти процесса.
//! - [`auth`] — хранение ключей в системном хранилище (Keychain и аналоги).

pub mod access;
pub mod auth;
pub mod executor;
pub mod key;
pub mod path;
pub mod spec;

/// Ключи для тестов других крейтов: генерируются при выполнении.
#[cfg(any(test, feature = "testing"))]
pub use key::testing;

pub use access::{AccessToken, KeyTokens, StaticToken, TokenSource};
pub use executor::DEFAULT_BASE_URL;
pub use key::{ApiKey, KeyId};
pub use spec::{
    AccessLevel, BodyKind, HttpMethod, OperationDef, ParamDef, ParamLocation, ParamType,
    ResponseKind, ToolsetDef,
};

/// OpenAPI-спецификация RuStore API в виде JSON.
pub const OPENAPI_JSON: &str = include_str!("../spec/rustore.openapi.json");
