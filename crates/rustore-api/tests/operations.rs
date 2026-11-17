//! Инварианты таблицы операций, сгенерированной `build.rs`, и снапшоты характерных операций.

use std::collections::{BTreeMap, BTreeSet};

use rustore_api::spec::{
    AccessLevel, BodyKind, HttpMethod, OperationDef, ParamLocation, ParamType, ResponseKind, find,
    find_toolset, operations, toolsets,
};
use serde_json::{Value, json};

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::from_str(rustore_api::OPENAPI_JSON).expect("спецификация должна быть JSON")
}

/// Операции спецификации по `operationId`.
fn spec_operations(spec: &Value) -> BTreeMap<String, (String, &'static str, Value)> {
    let mut out = BTreeMap::new();
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for method in METHODS {
            if let Some(op) = item.get(method) {
                let id = op["operationId"].as_str().expect("operationId").to_owned();
                out.insert(id, (path.clone(), method, op.clone()));
            }
        }
    }
    out
}

fn schema(op: &OperationDef) -> Value {
    serde_json::from_str(op.input_schema)
        .unwrap_or_else(|err| panic!("{}: input_schema не JSON: {err}", op.tool_name))
}

fn placeholders(path: &str) -> BTreeSet<&str> {
    path.split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect()
}

fn op(name: &str) -> &'static OperationDef {
    find(name).unwrap_or_else(|| panic!("нет операции {name}"))
}

#[test]
fn every_spec_operation_is_generated() {
    let spec = spec();
    let expected = spec_operations(&spec);
    assert!(
        (40..=120).contains(&expected.len()),
        "неожиданное число операций в спецификации: {}",
        expected.len()
    );
    assert_eq!(operations().len(), expected.len());
    let generated: BTreeSet<&str> = operations().iter().map(|op| op.operation_id).collect();
    let in_spec: BTreeSet<&str> = expected.keys().map(String::as_str).collect();
    assert_eq!(generated, in_spec);

    for op in operations() {
        let (path, method, raw) = &expected[op.operation_id];
        assert_eq!(op.path, path, "{}", op.tool_name);
        assert_eq!(
            op.method.as_str(),
            method.to_uppercase(),
            "{}",
            op.tool_name
        );
        assert_eq!(
            Some(op.access.as_str()),
            raw["x-access-level"].as_str(),
            "{}: уровень доступа должен совпадать с x-access-level",
            op.tool_name
        );
        assert_eq!(
            Some(op.docs_url),
            raw["externalDocs"]["url"].as_str(),
            "{}",
            op.tool_name
        );
        assert!(!op.summary.is_empty(), "{}: нет summary", op.tool_name);
        let tag = raw["tags"][0].as_str().expect("тег");
        assert_eq!(
            find_toolset(op.toolset).map(|t| t.tag),
            Some(tag),
            "{}: набор не соответствует тегу",
            op.tool_name
        );
    }
}

#[test]
fn tool_names_are_unique_sorted_and_valid() {
    let names: Vec<&str> = operations().iter().map(|op| op.tool_name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted, "имена должны быть уникальны и отсортированы");
    for name in names {
        assert!(
            !name.is_empty()
                && name.len() <= 40
                && name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "недопустимое имя инструмента {name}"
        );
        assert_eq!(find(name).map(|op| op.tool_name), Some(name));
    }
    assert!(find("no_such_tool").is_none());
    // operationId в спецификации уже в snake_case: имя инструмента совпадает с ним.
    for op in operations() {
        assert_eq!(op.tool_name, op.operation_id);
    }
    assert_eq!(
        op("get_subscription_v5").operation_id,
        "get_subscription_v5"
    );
}

#[test]
fn every_tag_has_toolset() {
    let spec = spec();
    let tags: Vec<&str> = spec["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    let toolset_tags: Vec<&str> = toolsets().iter().map(|t| t.tag).collect();
    assert_eq!(
        toolset_tags, tags,
        "наборы должны повторять tags спецификации"
    );

    let slugs: BTreeSet<&str> = toolsets().iter().map(|t| t.slug).collect();
    assert_eq!(slugs.len(), toolsets().len(), "slug повторяются");
    for toolset in toolsets() {
        assert!(
            toolset
                .slug
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "недопустимый slug {}",
            toolset.slug
        );
        assert!(
            operations().iter().any(|op| op.toolset == toolset.slug),
            "пустой набор {}",
            toolset.slug
        );
    }
    for op in operations() {
        assert!(slugs.contains(op.toolset), "{}", op.tool_name);
    }
    let expected = [
        "apps",
        "releases",
        "listing",
        "reviews",
        "purchases",
        "subscriptions",
        "catalog",
        "access",
        "push",
    ];
    let slugs: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
    assert_eq!(slugs, expected);
}

#[test]
fn input_schemas_are_objects_with_all_params() {
    for op in operations() {
        let schema = schema(op);
        assert_eq!(schema["type"], "object", "{}", op.tool_name);
        assert_eq!(schema["additionalProperties"], false, "{}", op.tool_name);
        assert!(
            !op.input_schema.contains("$ref"),
            "{}: $ref не раскрыт",
            op.tool_name
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{}: нет properties", op.tool_name));
        let param_names: BTreeSet<&str> = op.params.iter().map(|p| p.name).collect();
        let property_names: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(param_names, property_names, "{}", op.tool_name);
        assert_eq!(
            param_names.len(),
            op.params.len(),
            "{}: повтор параметра",
            op.tool_name
        );
        for header in ["Public-Token", "Authorization"] {
            assert!(
                !property_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(header)),
                "{}: заголовок {header} попал в аргументы",
                op.tool_name
            );
        }

        for param in op.params {
            assert!(
                !param.description.is_empty(),
                "{}: {}",
                op.tool_name,
                param.name
            );
            assert_eq!(
                properties[param.name]["description"], param.description,
                "{}: описание {} в схеме и ParamDef различается",
                op.tool_name, param.name
            );
        }

        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let expected: BTreeSet<&str> = op
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name)
            .collect();
        assert_eq!(required, expected, "{}", op.tool_name);

        check_schema(op.tool_name, "", &schema);
    }
}

/// Проверяет, что схема понятна MCP-клиентам: известные типы, имена свойств по шаблону
/// `^[a-zA-Z0-9_.-]{1,64}$` (ограничение API Claude), непустой `required` из имён свойств.
fn check_schema(tool: &str, at: &str, schema: &Value) {
    const TYPES: [&str; 7] = [
        "string", "number", "integer", "boolean", "array", "object", "null",
    ];
    let object = schema
        .as_object()
        .unwrap_or_else(|| panic!("{tool}{at}: схема не объект"));
    for key in object.keys() {
        assert!(
            !key.starts_with("x-") && key != "example" && key != "examples" && key != "$ref",
            "{tool}{at}: лишний ключ {key}"
        );
    }
    match object.get("type") {
        None => {}
        Some(Value::String(kind)) => {
            assert!(TYPES.contains(&kind.as_str()), "{tool}{at}: тип {kind}")
        }
        Some(Value::Array(kinds)) => {
            for kind in kinds {
                assert!(
                    kind.as_str().is_some_and(|k| TYPES.contains(&k)),
                    "{tool}{at}: тип {kind}"
                );
            }
        }
        Some(other) => panic!("{tool}{at}: тип {other}"),
    }
    let is_array = match object.get("type") {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => kinds.iter().any(|k| k == "array"),
        _ => false,
    };
    assert!(
        !is_array || object.contains_key("items"),
        "{tool}{at}: массив без items"
    );
    let properties = object.get("properties").and_then(Value::as_object);
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .unwrap_or_else(|| panic!("{tool}{at}: required не массив"));
        assert!(!required.is_empty(), "{tool}{at}: пустой required");
        for name in required {
            let name = name.as_str().expect("имя в required");
            assert!(
                properties.is_some_and(|p| p.contains_key(name)),
                "{tool}{at}: required {name} нет в properties"
            );
        }
    }
    for (name, property) in properties.into_iter().flatten() {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
            "{tool}{at}: недопустимое имя свойства {name}"
        );
        check_schema(tool, &format!("{at}.{name}"), property);
    }
    if let Some(items) = object.get("items") {
        check_schema(tool, &format!("{at}[]"), items);
    }
    if let Some(additional) = object.get("additionalProperties")
        && additional.is_object()
    {
        check_schema(tool, &format!("{at}{{}}"), additional);
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(variants) = object.get(key) {
            for variant in variants.as_array().expect("варианты") {
                check_schema(tool, &format!("{at}|"), variant);
            }
        }
    }
}

#[test]
fn path_params_match_templates() {
    for op in operations() {
        let declared: BTreeSet<&str> = op.params_in(ParamLocation::Path).map(|p| p.name).collect();
        assert_eq!(placeholders(op.path), declared, "{}", op.tool_name);
        assert!(
            op.params_in(ParamLocation::Path).all(|p| p.required),
            "{}: path-параметр необязателен",
            op.tool_name
        );
        // Пути push — после префикса проекта в адресе сервиса.
        let root = if op.service.is_push() {
            "/"
        } else {
            "/public/"
        };
        assert!(op.path.starts_with(root), "{}: {}", op.tool_name, op.path);
        assert!(
            !op.path.ends_with('/') && op.path != "/public/auth",
            "{}: {}",
            op.tool_name,
            op.path
        );
    }
}

#[test]
fn body_kind_matches_params() {
    for op in operations() {
        let bodies: Vec<_> = op.params_in(ParamLocation::Body).collect();
        let form: Vec<_> = op.params_in(ParamLocation::FormData).collect();
        match op.body {
            None => assert!(bodies.is_empty() && form.is_empty(), "{}", op.tool_name),
            Some(BodyKind::Json) => {
                assert!(form.is_empty(), "{}", op.tool_name);
                assert!(
                    matches!(bodies.as_slice(), [body] if body.name == "body" && body.value_type == ParamType::Object),
                    "{}",
                    op.tool_name
                );
            }
            Some(BodyKind::Multipart) => {
                assert!(bodies.is_empty(), "{}", op.tool_name);
                assert!(!form.is_empty(), "{}", op.tool_name);
            }
        }
        for param in op.params {
            assert_eq!(
                param.value_type == ParamType::File,
                param.location == ParamLocation::FormData,
                "{}: {}",
                op.tool_name,
                param.name
            );
        }
    }
}

#[test]
fn access_levels_and_kinds() {
    for op in operations() {
        if op.method == HttpMethod::Get {
            assert_eq!(op.access, AccessLevel::Read, "{}", op.tool_name);
        }
    }
    // Чтение.
    for name in [
        "list_apps",
        "get_version",
        "list_reviews",
        "get_payment",
        "get_subscription",
    ] {
        assert_eq!(op(name).access, AccessLevel::Read, "{name}");
    }
    // Изменения черновика версии — запись, в том числе удаление черновика и скриншота.
    for name in [
        "create_draft_version",
        "update_draft_version",
        "upload_apk",
        "upload_aab",
        "upload_icon",
        "upload_screenshot",
        "upload_screenshot_v2",
        "delete_screenshot",
        "delete_draft_version",
    ] {
        assert_eq!(op(name).access, AccessLevel::Write, "{name}");
    }
    // Последствия для пользователей RuStore и денег — уровень удаления.
    for name in [
        "submit_draft_for_moderation",
        // Тем же вызовом меняется процент частичной публикации опубликованной версии.
        "update_publish_settings",
        "publish_version",
        "archive_version",
        "reply_to_review",
        "update_review_answer",
        "delete_review_answer",
        "refund_payment",
        "confirm_purchase",
        "cancel_purchase",
        "acknowledge_subscription",
        "cancel_subscription",
        "grant_app_access",
        "revoke_app_access",
    ] {
        assert_eq!(op(name).access, AccessLevel::Destructive, "{name}");
    }
    // Варианты для тестовых платежей — с тем же уровнем, что у основного метода.
    for op in operations() {
        if let Some(base) = op.tool_name.strip_suffix("_sandbox") {
            let base = find(base).unwrap_or_else(|| panic!("{}: нет {base}", op.tool_name));
            assert_eq!(op.access, base.access, "{}", op.tool_name);
            assert_eq!(op.method, base.method, "{}", op.tool_name);
            assert!(op.path.starts_with("/public/sandbox/"), "{}", op.path);
        }
    }

    let binary: BTreeSet<&str> = operations()
        .iter()
        .filter(|op| op.response == ResponseKind::Binary)
        .map(|op| op.tool_name)
        .collect();
    assert_eq!(binary, BTreeSet::from(["export_reviews_csv"]));

    let multipart: BTreeSet<&str> = operations()
        .iter()
        .filter(|op| op.body == Some(BodyKind::Multipart))
        .map(|op| op.tool_name)
        .collect();
    assert_eq!(
        multipart,
        BTreeSet::from([
            "upload_aab",
            "upload_apk",
            "upload_icon",
            "upload_screenshot",
            "upload_screenshot_v2",
        ])
    );
    for name in multipart {
        let file = op(name)
            .param("file")
            .unwrap_or_else(|| panic!("{name}: нет параметра file"));
        assert_eq!(file.location, ParamLocation::FormData, "{name}");
        assert_eq!(file.value_type, ParamType::File, "{name}");
        assert!(file.required, "{name}");
    }
}

#[test]
fn token_endpoint_and_deprecated_methods_are_excluded() {
    // Токен доступа выпускает исполнитель, устаревшие методы справочника не становятся
    // инструментами.
    for op in operations() {
        assert!(!op.path.starts_with("/public/auth"), "{}", op.tool_name);
        assert!(
            !op.path.contains("/glike/"),
            "{}: устаревший метод",
            op.tool_name
        );
        assert!(
            !op.summary.to_lowercase().contains("deprecated"),
            "{}",
            op.tool_name
        );
    }
    let spec = spec();
    let pages: Vec<&str> = spec["info"]["x-source-pages"]
        .as_array()
        .expect("x-source-pages")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(pages.contains(&"api-authorization-token"), "{pages:?}");
    assert!(pages.contains(&"api-subscription-payment/api-token-payment-method"));
}

#[test]
fn responses_use_shared_envelope() {
    let spec = spec();
    let envelope = &spec["components"]["schemas"]["Envelope"];
    for field in ["code", "message", "body", "timestamp"] {
        assert!(envelope["properties"][field].is_object(), "{field}");
    }
    assert_eq!(
        spec["components"]["securitySchemes"]["PublicToken"]["name"],
        "Public-Token"
    );
    assert_eq!(spec["servers"][0]["url"], "https://public-api-m.rustore.ru");
    for (id, (_, _, raw)) in spec_operations(&spec) {
        let ok = &raw["responses"]["200"]["content"];
        let text = ok.to_string();
        if raw["tags"][0] == "Push-уведомления" {
            // Push API — свой сервис: без конверта, со своим секретом и страницами SDK.
            assert!(!text.contains("Envelope"), "{id}");
            assert_eq!(raw["security"][0]["PushServiceToken"], json!([]), "{id}");
            assert!(
                raw["servers"][0]["url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with("https://vkpns")
                        && url.ends_with("/v1/projects/{project_id}"))
            );
            assert!(
                raw["externalDocs"]["url"]
                    .as_str()
                    .is_some_and(|url| url
                        .starts_with("https://www.rustore.ru/help/sdk/push-notifications/")),
                "{id}"
            );
            continue;
        }
        if id == "export_reviews_csv" {
            assert!(ok["application/octet-stream"].is_object(), "{id}");
        } else {
            assert!(text.contains("#/components/schemas/Envelope"), "{id}");
        }
        assert!(
            raw["externalDocs"]["url"].as_str().is_some_and(
                |url| url.starts_with("https://www.rustore.ru/help/work-with-rustore-api/")
            ),
            "{id}"
        );
    }
}

/// Снапшот операции. Ключи сортируются, чтобы порядок не зависел от фичи
/// `preserve_order` у `serde_json`, которую может включить любая зависимость.
macro_rules! snapshot {
    ($name:expr) => {
        insta::with_settings!({ sort_maps => true }, {
            insta::assert_json_snapshot!(snapshot_view(op($name)));
        })
    };
}

/// Представление операции для снапшота: все поля плюс разобранная JSON Schema.
fn snapshot_view(op: &OperationDef) -> Value {
    let params: Vec<Value> = op
        .params
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "location": format!("{:?}", p.location),
                "required": p.required,
                "value_type": format!("{:?}", p.value_type),
                "description": p.description,
            })
        })
        .collect();
    json!({
        "tool_name": op.tool_name,
        "operation_id": op.operation_id,
        "toolset": op.toolset,
        "summary": op.summary,
        "description": op.description,
        "docs_url": op.docs_url,
        "method": op.method.as_str(),
        "path": op.path,
        "access": format!("{:?}", op.access),
        "params": params,
        "body": op.body.map(|kind| format!("{kind:?}")),
        "response": format!("{:?}", op.response),
        "input_schema": schema(op),
    })
}

#[test]
fn snapshot_list_reviews() {
    // GET с параметрами пути и запроса.
    snapshot!("list_reviews");
}

#[test]
fn snapshot_create_draft_version() {
    // JSON-тело с вложенными объектами.
    snapshot!("create_draft_version");
}

#[test]
fn snapshot_upload_apk() {
    // multipart-загрузка файла.
    snapshot!("upload_apk");
}

#[test]
fn snapshot_export_reviews_csv() {
    // Скачивание файла: бинарный ответ.
    snapshot!("export_reviews_csv");
}

#[test]
fn snapshot_delete_draft_version() {
    // DELETE черновика — уровень записи.
    snapshot!("delete_draft_version");
}

#[test]
fn snapshot_send_push_notification() {
    // Push API: другой сервис, тело с вложенным сообщением, уровень удаления.
    assert_eq!(
        op("send_push_notification").service,
        rustore_api::spec::ApiService::Push
    );
    snapshot!("send_push_notification");
}

#[test]
fn snapshot_reply_to_review() {
    // POST уровня удаления: публичный ответ на отзыв.
    snapshot!("reply_to_review");
}

#[test]
fn snapshot_confirm_purchase() {
    // Параметр с литеральным суффиксом в сегменте пути (`{purchaseId}:confirm`).
    snapshot!("confirm_purchase");
}

#[test]
fn snapshot_get_subscription_sandbox() {
    // Вариант для тестовых платежей.
    snapshot!("get_subscription_sandbox");
}
