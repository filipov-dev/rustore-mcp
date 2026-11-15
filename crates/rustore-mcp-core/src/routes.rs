//! Сопоставление произвольного запроса (`api_request`) с операциями спецификации.
//!
//! Уровень доступа `api_request` определяется не только HTTP-методом: если запрос совпадает
//! с известной операцией, действует её уровень `x-access-level` (отправка на модерацию через
//! `POST …/commit` и ответ на отзыв — удаление, удаление черновика — запись и т. д.).
//!
//! Правила сопоставления:
//!
//! - метод запроса должен совпадать с методом операции;
//! - сравнивается шаблон [`OperationDef::path`]; один завершающий `/` в пути запроса
//!   игнорируется;
//! - число сегментов совпадает; литеральный сегмент шаблона сравнивается без учёта регистра;
//! - `{param}` совпадает с одним непустым сегментом; литеральные префикс и суффикс вокруг
//!   параметра обязательны, а значение между ними не может быть пустым;
//! - сегмент запроса сравнивается и в исходном, и в percent-раскодированном виде
//!   (до нескольких раундов), чтобы `publi%73h` не обходил совпадение с `publish`.
//!
//! Если совпало несколько операций (например, литеральный сегмент и шаблон с параметром на
//! том же месте), требуется максимальный уровень доступа среди них: решение не зависит от
//! того, как маршрутизирует запрос сервер RuStore. Если не совпало ничего — уровень по
//! методу ([`default_access`]).

use std::borrow::Cow;

use percent_encoding::percent_decode_str;
use rustore_api::spec::{AccessLevel, HttpMethod, OperationDef, operations};

/// Сколько раундов percent-раскодирования проверять (как в `validate_relative_path`).
const MAX_DECODE_ROUNDS: usize = 3;

/// Операция, с которой совпал запрос.
#[derive(Debug, Clone, Copy)]
pub struct RouteMatch {
    pub op: &'static OperationDef,
    /// Число литеральных символов шаблона, совпавших с запросом: чем больше, тем точнее.
    pub specificity: usize,
}

/// Уровень доступа запроса к пути, которого нет в спецификации: `GET` — чтение, `DELETE` —
/// удаление, остальные методы — запись.
pub fn default_access(method: HttpMethod) -> AccessLevel {
    match method {
        HttpMethod::Get => AccessLevel::Read,
        HttpMethod::Delete => AccessLevel::Destructive,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch => AccessLevel::Write,
    }
}

/// Все операции с методом `method`, шаблон которых совпадает с `path`.
///
/// Порядок: сначала самые точные совпадения (больше литеральных символов), затем по имени.
pub fn match_operations(method: HttpMethod, path: &str) -> Vec<RouteMatch> {
    let segments = request_segments(path);
    let mut matches: Vec<RouteMatch> = operations()
        .iter()
        .filter(|op| op.method == method)
        .filter_map(|op| {
            template_specificity(op.path, &segments)
                .map(|specificity| RouteMatch { op, specificity })
        })
        .collect();
    matches.sort_by(|a, b| {
        b.specificity
            .cmp(&a.specificity)
            .then_with(|| a.op.tool_name.cmp(b.op.tool_name))
    });
    matches
}

/// Решение о доступе для запроса `api_request`.
#[derive(Debug, Clone)]
pub struct AccessDecision {
    /// Требуемый уровень доступа.
    pub required: AccessLevel,
    /// Совпавшие операции, самые точные первыми; пусто — уровень взят по методу.
    pub matches: Vec<RouteMatch>,
}

impl AccessDecision {
    /// Самая точная совпавшая операция.
    pub fn operation(&self) -> Option<&'static OperationDef> {
        self.matches.first().map(|m| m.op)
    }

    /// Совпала ли операция с именем инструмента `tool_name`.
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        self.matches.iter().any(|m| m.op.tool_name == tool_name)
    }

    /// Операция, на которой требуемый уровень достигается (самая точная из таких).
    pub fn strictest(&self) -> Option<&'static OperationDef> {
        self.matches
            .iter()
            .find(|m| m.op.access == self.required)
            .map(|m| m.op)
    }
}

/// Какой уровень доступа нужен запросу `method path`.
pub fn required_access(method: HttpMethod, path: &str) -> AccessDecision {
    let matches = match_operations(method, path);
    let required = matches
        .iter()
        .map(|m| m.op.access)
        .max()
        .unwrap_or_else(|| default_access(method));
    AccessDecision { required, matches }
}

/// Сегменты пути без ведущего и одного завершающего `/`.
fn request_segments(path: &str) -> Vec<&str> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('/').collect()
}

/// Точность совпадения шаблона с сегментами запроса; `None` — не совпадает.
fn template_specificity(template: &str, segments: &[&str]) -> Option<usize> {
    let template_segments = request_segments(template);
    if template_segments.len() != segments.len() || segments.is_empty() {
        return None;
    }
    let mut specificity = 0;
    for (pattern, segment) in template_segments.iter().zip(segments) {
        specificity += segment_specificity(pattern, segment)?;
    }
    Some(specificity)
}

/// Совпадение одного сегмента шаблона: литерал или `префикс{param}суффикс`.
fn segment_specificity(pattern: &str, segment: &str) -> Option<usize> {
    if segment.is_empty() {
        return None;
    }
    let variants = decoded_variants(segment);
    let Some((prefix, rest)) = pattern.split_once('{') else {
        return variants
            .iter()
            .any(|value| value.eq_ignore_ascii_case(pattern))
            .then_some(pattern.len());
    };
    // В шаблонах спецификации не больше одного параметра на сегмент (проверяется тестом).
    let suffix = rest.split_once('}').map_or("", |(_, suffix)| suffix);
    variants
        .iter()
        .any(|value| {
            value.len() > prefix.len() + suffix.len()
                && starts_with_ignore_case(value, prefix)
                && ends_with_ignore_case(value, suffix)
        })
        .then_some(prefix.len() + suffix.len())
}

/// Исходный сегмент и его percent-раскодированные варианты.
fn decoded_variants(segment: &str) -> Vec<Cow<'_, str>> {
    let mut variants = vec![Cow::Borrowed(segment)];
    for _ in 0..MAX_DECODE_ROUNDS {
        let Some(last) = variants.last() else { break };
        let decoded = percent_decode_str(last).decode_utf8_lossy().into_owned();
        if decoded == **last {
            break;
        }
        variants.push(Cow::Owned(decoded));
    }
    variants
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ends_with_ignore_case(value: &str, suffix: &str) -> bool {
    value
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| value.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(method: HttpMethod, path: &str) -> Vec<&'static str> {
        match_operations(method, path)
            .iter()
            .map(|m| m.op.tool_name)
            .collect()
    }

    fn first(method: HttpMethod, path: &str) -> Option<&'static str> {
        required_access(method, path)
            .operation()
            .map(|op| op.tool_name)
    }

    fn access(method: HttpMethod, path: &str) -> AccessLevel {
        required_access(method, path).required
    }

    /// Шаблон операции с подставленными значениями параметров.
    fn concrete(template: &str) -> String {
        let mut out = String::new();
        let mut rest = template;
        while let Some((head, tail)) = rest.split_once('{') {
            out.push_str(head);
            out.push_str("X1");
            rest = tail.split_once('}').map_or("", |(_, after)| after);
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn templates_have_one_param_per_segment_and_the_public_root() {
        for op in operations() {
            let segments = request_segments(op.path);
            assert_eq!(
                segments[0],
                rustore_api::path::API_ROOT,
                "{}: {}",
                op.tool_name,
                op.path
            );
            for segment in segments {
                assert!(
                    segment.matches('{').count() <= 1,
                    "{}: {}",
                    op.tool_name,
                    op.path
                );
            }
        }
    }

    #[test]
    fn every_operation_matches_its_own_template() {
        for op in operations() {
            let path = concrete(op.path);
            let decision = required_access(op.method, &path);
            assert!(
                decision.matches_tool(op.tool_name),
                "{} не совпала с {path}",
                op.tool_name
            );
            assert!(decision.required >= op.access, "{}", op.tool_name);
        }
    }

    #[test]
    fn literal_segments_and_suffixes_tell_operations_apart() {
        let app = "/public/v1/application/com.example.app";
        assert_eq!(
            first(HttpMethod::Get, "/public/v1/application"),
            Some("list_apps")
        );
        assert_eq!(
            first(HttpMethod::Get, &format!("{app}/comment")),
            Some("list_reviews")
        );
        assert_eq!(
            first(HttpMethod::Get, &format!("{app}/comment/export")),
            Some("export_reviews_csv")
        );
        assert_eq!(
            first(HttpMethod::Get, &format!("{app}/comment/statistic")),
            Some("get_app_rating")
        );
        assert_eq!(
            first(HttpMethod::Post, &format!("{app}/version/7/publish")),
            Some("publish_version")
        );
        assert_eq!(
            first(
                HttpMethod::Post,
                &format!("{app}/version/7/publish-settings")
            ),
            Some("update_publish_settings")
        );
        // Действие после двоеточия — литеральный суффикс шаблона `{purchaseId}:confirm`.
        let purchases = "/public/applications/1/purchases";
        assert_eq!(
            names(HttpMethod::Put, &format!("{purchases}/abc:confirm")),
            ["confirm_purchase"]
        );
        assert_eq!(
            names(HttpMethod::Put, &format!("{purchases}/abc:cancel")),
            ["cancel_purchase"]
        );
        // Пустое значение параметра перед суффиксом не совпадает.
        assert!(names(HttpMethod::Put, &format!("{purchases}/:confirm")).is_empty());
        assert!(names(HttpMethod::Put, &format!("{purchases}/abc")).is_empty());
    }

    #[test]
    fn sandbox_and_versions_are_separate_operations() {
        assert_eq!(
            first(HttpMethod::Get, "/public/v2/invoices/1"),
            Some("get_payment")
        );
        assert_eq!(
            first(HttpMethod::Get, "/public/sandbox/v2/invoices/1"),
            Some("get_payment_sandbox")
        );
        assert_eq!(
            first(
                HttpMethod::Get,
                "/public/v5/subscription/com.example.app/sub/p1"
            ),
            Some("get_subscription_v5")
        );
        // Версии в пути не взаимозаменяемы: другой версии нет в спецификации.
        assert!(names(HttpMethod::Get, "/public/v3/invoices/1").is_empty());
    }

    #[test]
    fn unknown_paths_and_methods() {
        assert!(names(HttpMethod::Get, "/public/v1/no/such/endpoint").is_empty());
        assert!(names(HttpMethod::Get, "/").is_empty());
        assert!(names(HttpMethod::Get, "").is_empty());
        // Путь операции, но другой метод: совпадения нет.
        assert!(names(HttpMethod::Put, "/public/v1/application/x/comment").is_empty());
    }

    #[test]
    fn trailing_slash_case_and_encoding() {
        let app = "/public/v1/application/com.example.app";
        assert_eq!(
            first(HttpMethod::Get, &format!("{app}/comment/")),
            Some("list_reviews")
        );
        assert_eq!(
            first(
                HttpMethod::Get,
                "/PUBLIC/V1/Application/com.example.app/COMMENT"
            ),
            Some("list_reviews")
        );
        assert_eq!(
            first(
                HttpMethod::Delete,
                "/public/v1/application/com%252Eexample/feedback/1"
            ),
            Some("delete_review_answer")
        );
        assert_eq!(
            first(HttpMethod::Post, &format!("{app}/version/7/publi%73h")),
            Some("publish_version")
        );
        assert_eq!(
            first(HttpMethod::Post, &format!("{app}/version/7/publi%2573h")),
            Some("publish_version")
        );
        // Только один завершающий `/`.
        assert_eq!(first(HttpMethod::Get, "/public/v1/application//"), None);
        assert_eq!(
            first(HttpMethod::Get, "/public/v1/application/"),
            Some("list_apps")
        );
    }

    #[test]
    fn access_comes_from_operation_then_method() {
        let app = "/public/v1/application/com.example.app";
        assert_eq!(
            access(HttpMethod::Post, &format!("{app}/version")),
            AccessLevel::Write
        );
        assert_eq!(
            access(HttpMethod::Post, &format!("{app}/version/7/commit")),
            AccessLevel::Destructive
        );
        assert_eq!(
            access(HttpMethod::Post, &format!("{app}/feedback")),
            AccessLevel::Destructive
        );
        // Удаление черновика — запись: уровень операции ниже уровня метода DELETE.
        assert_eq!(
            access(HttpMethod::Delete, &format!("{app}/version/7")),
            AccessLevel::Write
        );
        // Неизвестные пути — по методу.
        for (method, expected) in [
            (HttpMethod::Get, AccessLevel::Read),
            (HttpMethod::Post, AccessLevel::Write),
            (HttpMethod::Put, AccessLevel::Write),
            (HttpMethod::Patch, AccessLevel::Write),
            (HttpMethod::Delete, AccessLevel::Destructive),
        ] {
            assert_eq!(
                access(method, "/public/v1/unknown/thing"),
                expected,
                "{method:?}"
            );
            assert_eq!(default_access(method), expected);
        }
    }

    #[test]
    fn strictest_names_the_operation_that_sets_the_level() {
        let decision = required_access(
            HttpMethod::Delete,
            "/public/v1/application/com.example.app/feedback/1",
        );
        assert_eq!(
            decision.strictest().map(|op| op.tool_name),
            Some("delete_review_answer")
        );
        assert!(
            required_access(HttpMethod::Get, "/public/v1/unknown")
                .strictest()
                .is_none()
        );
    }
}
