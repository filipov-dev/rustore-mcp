//! Представление ответа API в результате инструмента.
//!
//! - JSON без постраничной выдачи — компактный JSON ответа как есть;
//! - JSON-страница выдачи (`content`, `pageNumber`, `totalPages`) —
//!   `{"result": <ответ>, "pagination": {...}}`: агент видит номер страницы, число страниц и
//!   записей и номер следующей страницы с именем аргумента (`next_page`, `pass_as`);
//! - текст — как есть (данные выдачи — отдельной строкой после текста);
//! - сохранённый файл — `{"saved_to", "bytes", "content_type"}`;
//! - пустое тело — `{"status"}`.

use rmcp::model::{CallToolResult, ContentBlock};
use rustore_api::executor::{ApiResponse, PAGE_PARAM, Pagination, ResponseBody};
use rustore_api::spec::OperationDef;
use serde_json::{Value, json};

use crate::error::ToolError;

/// Текст успешного ответа (см. описание модуля).
pub fn response_text(response: &ApiResponse) -> String {
    let pagination =
        (!response.pagination.is_empty()).then(|| pagination_json(&response.pagination));
    match (&response.body, pagination) {
        (ResponseBody::Json(value), None) => value.to_string(),
        (ResponseBody::Json(value), Some(pagination)) => {
            json!({"result": value, "pagination": pagination}).to_string()
        }
        (ResponseBody::Text(text), None) => text.clone(),
        (ResponseBody::Text(text), Some(pagination)) => {
            format!("{text}\n\npagination: {pagination}")
        }
        (
            ResponseBody::SavedToFile {
                path,
                bytes,
                content_type,
            },
            _,
        ) => json!({
            "saved_to": path.display().to_string(),
            "bytes": bytes,
            "content_type": content_type,
        })
        .to_string(),
        (ResponseBody::Empty, None) => json!({"status": response.status}).to_string(),
        (ResponseBody::Empty, Some(pagination)) => {
            json!({"status": response.status, "pagination": pagination}).to_string()
        }
    }
}

/// Аргумент номера страницы у операции: `page` у большинства списков, `pageNumber` у
/// платежей по дате. `None` — операция не листается по номеру страницы.
pub fn page_argument(op: &OperationDef) -> Option<&'static str> {
    [PAGE_PARAM, "pageNumber"]
        .into_iter()
        .find_map(|name| op.param(name).map(|param| param.name))
}

/// Исполнитель не знает операцию и всегда предлагает `page`; здесь имя аргумента берётся из
/// схемы операции. Операция без номера страницы следующую страницу так не получит — тогда
/// `next_page` убирается, итоги выдачи остаются. Без операции (путь `api_request` вне
/// спецификации) выдача не меняется.
pub fn adjust_pagination(op: Option<&OperationDef>, pagination: &mut Pagination) {
    let Some(op) = op else {
        return;
    };
    if pagination.next_page.is_none() {
        return;
    }
    match page_argument(op) {
        Some(name) => pagination.pass_as = Some(name),
        None => {
            pagination.next_page = None;
            pagination.pass_as = None;
        }
    }
}

fn pagination_json(pagination: &Pagination) -> Value {
    // Сериализация `Pagination` не может завершиться ошибкой (только строки и числа).
    serde_json::to_value(pagination).unwrap_or(Value::Null)
}

/// Успешный результат инструмента.
pub fn success(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Результат инструмента с `isError: true`.
pub fn failure(err: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn response(body: ResponseBody, pagination: Pagination) -> ApiResponse {
        ApiResponse {
            status: 204,
            body,
            pagination,
        }
    }

    #[test]
    fn formats_bodies_without_pagination() {
        let none = Pagination::default;
        assert_eq!(
            response_text(&response(ResponseBody::Json(json!({"a": [1, 2]})), none())),
            r#"{"a":[1,2]}"#
        );
        assert_eq!(
            response_text(&response(ResponseBody::Text("plain".into()), none())),
            "plain"
        );
        assert_eq!(
            response_text(&response(ResponseBody::Empty, none())),
            r#"{"status":204}"#
        );
        let saved = response_text(&response(
            ResponseBody::SavedToFile {
                path: PathBuf::from("/tmp/x.bin"),
                bytes: 42,
                content_type: Some("image/png".into()),
            },
            none(),
        ));
        assert_eq!(
            saved,
            r#"{"bytes":42,"content_type":"image/png","saved_to":"/tmp/x.bin"}"#
        );
    }

    #[test]
    fn shows_pagination() {
        let body = json!({"content": [{"packageName": "a"}], "pageNumber": 0, "totalPages": 3});
        let pagination = Pagination::from_body(&body);
        let text = response_text(&response(
            ResponseBody::Json(body.clone()),
            pagination.clone(),
        ));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["result"], body);
        assert_eq!(value["pagination"]["page"], 0);
        assert_eq!(value["pagination"]["total_pages"], 3);
        assert_eq!(value["pagination"]["next_page"], 1);
        assert_eq!(value["pagination"]["pass_as"], PAGE_PARAM);
        assert!(value["pagination"].get("total_elements").is_none());

        let text = response_text(&response(ResponseBody::Text("t".into()), pagination));
        assert!(text.starts_with("t\n\npagination: {"), "{text}");
    }

    #[test]
    fn pass_as_follows_the_operation() {
        let body = json!({"content": [], "pageNumber": 0, "totalPages": 2});
        let find = |name| rustore_api::spec::find(name).unwrap();
        let mut versions = Pagination::from_body(&body);
        adjust_pagination(Some(find("list_versions")), &mut versions);
        assert_eq!(versions.pass_as, Some(PAGE_PARAM));
        let mut payments = Pagination::from_body(&body);
        adjust_pagination(Some(find("list_payments_by_date")), &mut payments);
        assert_eq!(payments.pass_as, Some("pageNumber"));
        assert_eq!(payments.next_page, Some(1));
        // У списка приложений нет номера страницы: следующей страницы в выдаче нет.
        let mut apps = Pagination::from_body(&body);
        adjust_pagination(Some(find("list_apps")), &mut apps);
        assert_eq!((apps.next_page, apps.pass_as), (None, None));
        assert_eq!(apps.total_pages, Some(2));
        let mut unknown = Pagination::from_body(&body);
        adjust_pagination(None, &mut unknown);
        assert_eq!(unknown.pass_as, Some(PAGE_PARAM));
    }

    #[test]
    fn error_result_is_flagged() {
        let result = failure(&ToolError::new("boom"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(success("ok".into()).is_error, Some(false));
    }
}
