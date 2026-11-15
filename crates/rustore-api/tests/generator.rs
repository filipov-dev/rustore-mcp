//! Генератор операций: детерминизм (та же спецификация даёт тот же код, и он совпадает с
//! тем, что собрал `build.rs`) и ошибки сборки на нарушениях правил спецификации: имя
//! длиннее 40 символов, повтор имени, операция без набора инструментов, неизвестный
//! заголовок.
//!
//! Тест подключает модули генератора из `build/` напрямую.

#[allow(dead_code)]
#[path = "../build/generate.rs"]
mod generate;
#[allow(dead_code)]
#[path = "../build/naming.rs"]
mod naming;
#[allow(dead_code)]
#[path = "../build/schema.rs"]
mod schema;
#[allow(dead_code)]
#[path = "../build/toolsets.rs"]
mod toolsets;

use serde_json::{Value, json};

const BUILT: &str = include_str!(concat!(env!("OUT_DIR"), "/operations.rs"));

fn spec() -> Value {
    serde_json::from_str(rustore_api::OPENAPI_JSON).expect("спецификация — JSON")
}

/// Операция спецификации по `operationId`.
fn op_mut<'a>(spec: &'a mut Value, operation_id: &str) -> &'a mut Value {
    spec["paths"]
        .as_object_mut()
        .expect("paths")
        .values_mut()
        .flat_map(|item| item.as_object_mut().expect("элемент paths").values_mut())
        .find(|op| op["operationId"] == operation_id)
        .unwrap_or_else(|| panic!("нет операции {operation_id}"))
}

#[test]
fn generation_is_deterministic_and_matches_build() {
    let first = generate::generate(&spec());
    let second = generate::generate(&spec());
    assert_eq!(first, second, "два прогона генератора различаются");
    assert_eq!(
        first, BUILT,
        "код build.rs отличается от прогона генератора"
    );
}

#[test]
#[should_panic(expected = "недопустимое имя инструмента")]
fn tool_name_longer_than_40_is_an_error() {
    let mut spec = spec();
    let name = "list_reviews_with_a_very_long_name_over_40";
    assert!(name.len() > naming::MAX_TOOL_NAME);
    op_mut(&mut spec, "list_reviews")["operationId"] = json!(name);
    generate::generate(&spec);
}

#[test]
fn tool_name_of_40_chars_is_accepted() {
    let mut spec = spec();
    let name = "list_reviews_with_a_name_of_forty_chars_";
    assert_eq!(name.len(), naming::MAX_TOOL_NAME);
    op_mut(&mut spec, "list_reviews")["operationId"] = json!(name);
    assert!(generate::generate(&spec).contains(name));
}

#[test]
#[should_panic(expected = "повторяется")]
fn duplicate_tool_name_is_an_error() {
    let mut spec = spec();
    op_mut(&mut spec, "get_app_rating")["operationId"] = json!("list_reviews");
    generate::generate(&spec);
}

#[test]
#[should_panic(expected = "не объявлен в tags")]
fn operation_without_toolset_is_an_error() {
    let mut spec = spec();
    op_mut(&mut spec, "list_reviews")["tags"] = json!(["Нет такого раздела"]);
    generate::generate(&spec);
}

#[test]
#[should_panic(expected = "нет slug")]
fn tag_without_slug_is_an_error() {
    let mut spec = spec();
    spec["tags"]
        .as_array_mut()
        .expect("tags")
        .push(json!({"name": "Новый раздел", "description": ""}));
    generate::generate(&spec);
}

#[test]
#[should_panic(expected = "неподдерживаемый заголовок X-Custom")]
fn unknown_header_is_an_error() {
    let mut spec = spec();
    let op = op_mut(&mut spec, "list_reviews");
    op["parameters"]
        .as_array_mut()
        .expect("parameters")
        .push(json!({"name": "X-Custom", "in": "header", "schema": {"type": "string"}}));
    generate::generate(&spec);
}

#[test]
fn public_token_header_is_not_an_argument() {
    // Токен доступа добавляет исполнитель: заголовок в спецификации не становится
    // аргументом инструмента, в любом регистре.
    let mut spec = spec();
    let op = op_mut(&mut spec, "list_reviews");
    for name in ["Public-Token", "public-token"] {
        op["parameters"].as_array_mut().expect("parameters").push(
            json!({"name": name, "in": "header", "required": true, "schema": {"type": "string"}}),
        );
    }
    let code = generate::generate(&spec);
    assert_eq!(code, BUILT);
    assert!(!code.to_lowercase().contains("public-token"));
}

#[test]
fn deprecated_operations_are_not_generated() {
    let mut spec = spec();
    op_mut(&mut spec, "get_app_rating")["deprecated"] = json!(true);
    let code = generate::generate(&spec);
    assert!(!code.contains("\"get_app_rating\""));
    assert!(code.contains("\"list_reviews\""));
}

#[test]
#[should_panic(expected = "неизвестный x-access-level")]
fn unknown_access_level_is_an_error() {
    let mut spec = spec();
    op_mut(&mut spec, "list_reviews")["x-access-level"] = json!("admin");
    generate::generate(&spec);
}
