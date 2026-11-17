//! Проверка и сборка относительных путей RuStore API.
//!
//! Это граница безопасности: всё, что уходит в [`crate::executor::ApiClient::execute`],
//! проходит через [`validate_relative_path`], поэтому запрос не может уйти на другой хост
//! или выйти за пределы API через `..`, в том числе в percent-кодированном виде.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::executor::ApiError;
use crate::spec::ApiService;

/// Первый сегмент любого пути RuStore Public API: `/public/...`.
pub const API_ROOT: &str = "public";

/// Первый сегмент путей Push API после префикса проекта (`/v1/projects/<проект>`):
/// отправка на устройство — только `/messages:send`.
const PUSH_ROOTS: &[&str] = &["messages:send"];
/// Первый сегмент путей API топиков push: `/topics/<топик>/...`.
const PUSH_TOPICS_ROOTS: &[&str] = &["topics"];

/// Путь запроса токена (`/public/auth/`): через исполнитель его вызывать нельзя, токен
/// получает только [`crate::access::KeyTokens`].
const AUTH_SEGMENTS: [&str; 2] = ["public", "auth"];

/// Символы, которые кодируются в значении параметра пути.
///
/// Оставляем только unreserved-символы RFC 3986, поэтому `/`, `?`, `#`, `%`, `:` и прочие
/// разделители внутри значения становятся `%XX` и не могут изменить структуру пути.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Сколько раз раскодировать сегмент при проверке. Защищает от двойного кодирования
/// (`%252e%252e`) на случай, если что-то между нами и API раскодирует путь повторно.
const MAX_DECODE_ROUNDS: usize = 3;

/// Проверяет относительный путь RuStore API, например `/public/v1/application/com.example.app/comment`.
///
/// Требования:
/// - начинается с `/public/` (см. [`API_ROOT`]) и называет ресурс, поэтому не может быть
///   `//`, схемой или хостом;
/// - содержит только символы `pchar` из RFC 3986 и `/`: `?`, `#`, `\`, пробелы,
///   управляющие и не-ASCII символы отклоняются (query передаётся отдельно);
/// - `%` всегда образует корректную тройку `%XX`;
/// - нет пустых сегментов (`//`) кроме завершающего `/`;
/// - ни один сегмент ни в исходном, ни в раскодированном виде не равен `.` или `..`,
///   а раскодированный сегмент не содержит `.`/`..` между `/` или `\` и управляющих символов.
pub fn validate_relative_path(path: &str) -> Result<(), ApiError> {
    validate_service_path(ApiService::PublicApi, path)
}

/// Как [`validate_relative_path`], но для сервиса `service`: у Push API путь задаётся после
/// префикса проекта и начинается с `/messages:send` ([`ApiService::Push`]) или
/// `/topics/` ([`ApiService::PushTopics`]); остальные требования те же.
pub fn validate_service_path(service: ApiService, path: &str) -> Result<(), ApiError> {
    let invalid = |reason: &str| {
        Err(ApiError::InvalidPath(format!(
            "{reason}: `{}`",
            shorten(path)
        )))
    };

    if !path.starts_with('/') {
        return invalid("путь должен начинаться с `/`");
    }
    if path.starts_with("//") {
        return invalid("путь не может начинаться с `//`");
    }
    if let Some(c) = path.chars().find(|c| !is_allowed_char(*c)) {
        return Err(ApiError::InvalidPath(format!(
            "недопустимый символ {c:?} в пути `{}`; query передаётся отдельно, \
             остальные символы кодируйте как %XX",
            shorten(path)
        )));
    }
    if !has_valid_percent_triplets(path) {
        return invalid("некорректная percent-последовательность");
    }

    let segments: Vec<&str> = path[1..].split('/').collect();
    match service {
        ApiService::PublicApi => {
            let root_ok = segments.len() >= 2 && segments[0] == API_ROOT && !segments[1].is_empty();
            if !root_ok {
                return invalid("путь должен начинаться с `/public/` и указывать ресурс");
            }
        }
        ApiService::Push | ApiService::PushTopics => {
            let roots = if service == ApiService::Push {
                PUSH_ROOTS
            } else {
                PUSH_TOPICS_ROOTS
            };
            if !roots.contains(&segments[0]) {
                return invalid("путь не относится к Push API");
            }
        }
    }
    let decoded = |segment: &str| {
        percent_decode_str(segment)
            .decode_utf8_lossy()
            .to_lowercase()
    };
    if segments.len() >= 2
        && decoded(segments[0]) == AUTH_SEGMENTS[0]
        && decoded(segments[1]) == AUTH_SEGMENTS[1]
    {
        return invalid("токен RuStore API получает сервер, а не запрос к API");
    }
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // Пустой сегмент допустим только в конце (`/public/.../products/`).
            if index == last {
                continue;
            }
            return invalid("пустой сегмент пути");
        }
        if !segment_is_safe(segment) {
            return invalid("сегменты `.` и `..` запрещены, в том числе в закодированном виде");
        }
    }
    Ok(())
}

/// Подставляет параметры в шаблон пути вида `/public/v1/application/{packageName}/feedback/{feedbackId}`
/// и проверяет результат через [`validate_relative_path`].
///
/// Значение параметра кодируется как один сегмент пути: `a/b` превращается в `a%2Fb`,
/// поэтому значение не может добавить сегменты или выйти из пути. Отсутствующий или
/// пустой параметр даёт [`ApiError::MissingPathParam`]. Значения `.` и `..` отклоняются
/// проверкой итогового пути.
pub fn render_path<F>(template: &str, lookup: F) -> Result<String, ApiError>
where
    F: FnMut(&str) -> Option<String>,
{
    render_service_path(ApiService::PublicApi, template, lookup)
}

/// Как [`render_path`], но итоговый путь проверяется для сервиса `service`
/// ([`validate_service_path`]).
pub fn render_service_path<F>(
    service: ApiService,
    template: &str,
    mut lookup: F,
) -> Result<String, ApiError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(ApiError::InvalidPath(format!(
                "лишняя `}}` в шаблоне `{}`",
                shorten(template)
            )));
        }
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(ApiError::InvalidPath(format!(
                "незакрытая `{{` в шаблоне `{}`",
                shorten(template)
            )));
        };
        let name = &after[..close];
        if name.is_empty() || name.contains('{') {
            return Err(ApiError::InvalidPath(format!(
                "некорректный параметр в шаблоне `{}`",
                shorten(template)
            )));
        }
        let value = lookup(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::MissingPathParam(name.to_string()))?;
        out.extend(utf8_percent_encode(&value, SEGMENT));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    validate_service_path(service, &out)?;
    Ok(out)
}

/// Сегмент пути, за которым идёт идентификатор пользователя (VK ID в путях доступов
/// `/public/v1/application/{packageName}/developer/{devVkId}/...`).
const USER_SEGMENT: &str = "developer";

/// Путь запроса для логов: идентификатор пользователя после `/developer/` заменён на
/// `{devVkId}`. Идентификаторы пользователей кабинета — персональные данные, в stderr и
/// tracing они не пишутся.
pub fn path_for_log(path: &str) -> String {
    let mut masked = false;
    path.split('/')
        .map(|segment| {
            if masked && !segment.is_empty() {
                masked = false;
                return "{devVkId}";
            }
            masked = segment == USER_SEGMENT;
            segment
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `pchar` из RFC 3986 плюс разделитель `/`; `%` проверяется отдельно.
fn is_allowed_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '/'
                | '%'
        )
}

fn has_valid_percent_triplets(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let ok = bytes
                .get(i + 1..i + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
            if !ok {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Проверяет сегмент и его раскодированные варианты.
///
/// Закодированный `/` внутри значения (`a%2Fb`) допустим — так `render_path` передаёт
/// значения со слешем. Но если сервер раскодирует `%2F` до маршрутизации, `..%2F..`
/// превратится в обход каталога, поэтому раскодированный сегмент дополнительно делим
/// по `/` и `\` и ищем `.`/`..`.
fn segment_is_safe(segment: &str) -> bool {
    let mut current = segment.to_string();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if current
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..")
        {
            return false;
        }
        if current.chars().any(char::is_control) {
            return false;
        }
        if !current.contains('%') {
            return true;
        }
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            return true;
        }
        current = decoded;
    }
    // Слишком глубокое вложенное кодирование — подозрительно, отклоняем.
    false
}

/// Укорачивает путь для текста ошибки, чтобы не раздувать ответ MCP.
fn shorten(path: &str) -> String {
    const MAX: usize = 200;
    if path.len() <= MAX {
        return path.escape_debug().to_string();
    }
    let mut end = MAX;
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", path[..end].escape_debug())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn log_paths_hide_user_ids() {
        assert_eq!(
            path_for_log("/public/v1/application/com.example.app/developer/743103/role/DEV"),
            "/public/v1/application/com.example.app/developer/{devVkId}/role/DEV"
        );
        assert_eq!(
            path_for_log("/public/v1/application/com.example.app/developer"),
            "/public/v1/application/com.example.app/developer"
        );
        assert_eq!(
            path_for_log("/public/v1/application/com.example.app/comment"),
            "/public/v1/application/com.example.app/comment"
        );
    }

    #[test]
    fn accepts_valid_paths() {
        for path in [
            "/public/v1/application",
            "/public/v1/application/com.example.app/comment",
            "/public/v1/application/com.example.app/version/1/publish",
            "/public/applications/123/catalog/products/",
            "/public/v1/application/com.example.app/version/1/image/screenshot/PORTRAIT/0%20x.png",
            "/public/a/b..c/.hidden/d.",
            "/public/v1/application/id:1@x",
        ] {
            assert!(
                validate_relative_path(path).is_ok(),
                "{path} должен проходить"
            );
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "/",
            "/public",
            "/public/",
            // Токен получает только сервер.
            "/public/auth/",
            "/public/AUTH",
            "/public/%61uth/",
            "/v1/application",
            "/v2/application",
            "/Public/v1/application",
            "/application",
            "public/v1/application",
            "https://evil.example/public/v1/application",
            "http://evil.example",
            "//evil.example/public/v1/application",
            "///evil",
            "/public//v1",
            "/public/v1/application/../admin",
            "/public/../v2/application",
            "/public/./x",
            "/public/v1/application/..",
            "/public/v1/application/%2e%2e/admin",
            "/public/v1/application/%2E%2E/admin",
            "/public/v1/application/.%2e/admin",
            "/public/v1/application/%2e/admin",
            "/public/v1/application/..%2fadmin",
            "/public/v1/application/x%2F..%2Fadmin",
            "/public/v1/application/..%5cadmin",
            "/public/v1/application/%252e%252e/admin",
            "/public/v1/application/%25252e%25252e/admin",
            "/public/v1/application?x=1",
            "/public/v1/application#frag",
            "/public/v1/application\\..\\admin",
            "/public/v1/application/a b",
            "/public/v1/application/\n",
            "/public/v1/application/%00",
            "/public/v1/application/%0a",
            "/public/v1/application/%zz",
            "/public/v1/application/%2",
            "/public/v1/application/{key}",
            "/public/v1/application/кириллица",
            "/public/v1/application/x\u{0}",
        ] {
            assert!(
                matches!(validate_relative_path(path), Err(ApiError::InvalidPath(_))),
                "{path:?} должен отклоняться"
            );
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_and_encodes() {
        let p = params(&[
            ("packageName", "com.example.app"),
            ("versionId", "a/b"),
            ("orientation", "K?#% x"),
        ]);
        let rendered = render_path(
            "/public/v1/application/{packageName}/version/{versionId}/image/screenshot/{orientation}",
            |n| p.get(n).cloned(),
        )
        .unwrap();
        assert_eq!(
            rendered,
            "/public/v1/application/com.example.app/version/a%2Fb/image/screenshot/K%3F%23%25%20x"
        );
    }

    #[test]
    fn render_encodes_non_ascii() {
        let p = params(&[("name", "файл")]);
        let rendered = render_path("/public/files/{name}", |n| p.get(n).cloned()).unwrap();
        assert_eq!(rendered, "/public/files/%D1%84%D0%B0%D0%B9%D0%BB");
    }

    #[test]
    fn render_reports_missing_and_empty_params() {
        let p = params(&[("packageName", "com.example.app"), ("feedbackId", "")]);
        let lookup = |n: &str| p.get(n).cloned();
        assert!(matches!(
            render_path("/public/v1/application/{packageName}/{missing}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "missing"
        ));
        assert!(matches!(
            render_path("/public/v1/application/{packageName}/feedback/{feedbackId}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "feedbackId"
        ));
    }

    #[test]
    fn render_rejects_traversal_values() {
        for value in ["..", ".", "../admin", "..\\x"] {
            let result = render_path("/public/v1/application/{x}/comments", |_| {
                Some(value.to_string())
            });
            assert!(
                matches!(result, Err(ApiError::InvalidPath(_))),
                "значение {value:?} должно отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_malformed_templates() {
        for template in [
            "/public/v1/application/{key",
            "/public/v1/application/key}",
            "/public/v1/application/{}",
            "/public/v1/application/{{x}}",
        ] {
            assert!(
                matches!(
                    render_path(template, |_| Some("v".into())),
                    Err(ApiError::InvalidPath(_))
                ),
                "{template} должен отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_absolute_template() {
        assert!(render_path("https://evil/{x}", |_| Some("v".into())).is_err());
        // Значение параметра не может подменить корень пути API.
        assert!(render_path("/{root}/v1/application", |_| Some("v1".into())).is_err());
    }

    #[test]
    fn push_paths_are_checked_per_service() {
        let topic = |name: &str| {
            render_service_path(ApiService::PushTopics, "/topics/{topic}/publish", |_| {
                Some(name.to_owned())
            })
        };
        assert_eq!(topic("news").unwrap(), "/topics/news/publish");
        // Значение топика — один сегмент: разделители кодируются, `..` отклоняется.
        assert_eq!(topic("a/b").unwrap(), "/topics/a%2Fb/publish");
        assert!(topic("..").is_err());
        assert!(validate_service_path(ApiService::Push, "/messages:send").is_ok());
        // Пути другого сервиса не проходят: push не уходит на пути Public API и наоборот.
        for (service, path) in [
            (ApiService::Push, "/topics/news/publish"),
            (ApiService::Push, "/public/v1/application"),
            (ApiService::PushTopics, "/messages:send"),
            (ApiService::PublicApi, "/messages:send"),
            (ApiService::Push, "//vkpns.rustore.ru/messages:send"),
        ] {
            assert!(
                validate_service_path(service, path).is_err(),
                "{service:?} {path}"
            );
        }
    }
}
