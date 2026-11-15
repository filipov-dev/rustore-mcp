//! Общие помощники составных инструментов: разбор аргументов, имя пакета (из аргумента или
//! из профиля), фрагменты схем и ошибки.
//!
//! Аргументы приходят уже проверенными по схеме инструмента ([`super::check_arguments`]:
//! неизвестных и пропущенных обязательных нет), поэтому помощники проверяют только тип
//! значения и сообщают агенту, что не так, по-английски.

use rmcp::model::JsonObject;
use serde_json::{Value, json};

use super::Context;
use crate::error::ToolError;

/// Аргумент имени пакета — как у инструментов операций.
pub const PACKAGE_NAME_ARG: &str = "packageName";

/// Обязательный строковый аргумент (пробелы по краям отбрасываются).
pub fn required_str<'a>(args: &'a JsonObject, name: &str) -> Result<&'a str, ToolError> {
    optional_str(args, name)?.ok_or_else(|| missing_argument(name))
}

/// Необязательный строковый аргумент: отсутствие, `null` и пустая строка — `None`.
pub fn optional_str<'a>(args: &'a JsonObject, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim())),
        Some(_) => Err(invalid_argument(name, "a string")),
    }
}

/// Необязательное неотрицательное целое (число или строка с числом).
pub fn optional_u64(args: &JsonObject, name: &str) -> Result<Option<u64>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid_argument(name, "a non-negative integer")),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| invalid_argument(name, "a non-negative integer")),
        Some(_) => Err(invalid_argument(name, "a non-negative integer")),
    }
}

/// Необязательное целое в пределах `min..=max` со значением по умолчанию.
pub fn bounded_u64(
    args: &JsonObject,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ToolError> {
    let value = optional_u64(args, name)?.unwrap_or(default);
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(ToolError::new(format!(
            "argument `{name}` must be from {min} to {max}"
        )))
    }
}

/// Необязательный логический аргумент со значением по умолчанию.
pub fn bool_arg(args: &JsonObject, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(invalid_argument(name, "true or false")),
    }
}

/// Необязательный список строк: JSON-массив строк или одна строка через запятую.
pub fn string_list(args: &JsonObject, name: &str) -> Result<Vec<String>, ToolError> {
    let items: Vec<String> = match args.get(name) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(s) => Ok(s.clone()),
                _ => Err(invalid_argument(name, "an array of strings")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid_argument(name, "an array of strings")),
    };
    Ok(items
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect())
}

/// Имя пакета приложения: аргумент `packageName`, а без него — единственный пакет по
/// умолчанию из профиля. Несколько пакетов в профиле — агент должен выбрать сам.
pub fn package_name(ctx: &Context, args: &JsonObject) -> Result<String, ToolError> {
    if let Some(name) = optional_str(args, PACKAGE_NAME_ARG)? {
        return validate_package_name(name).map(str::to_owned);
    }
    match ctx.default_packages() {
        [only] => validate_package_name(only).map(str::to_owned),
        [] => Err(ToolError::new(format!(
            "pass `{PACKAGE_NAME_ARG}` (like com.example.app); the profile has no default \
             package. `list_apps` or `search_apps` lists the apps of the key"
        ))),
        several => Err(ToolError::new(format!(
            "pass `{PACKAGE_NAME_ARG}`: the profile has several default packages ({})",
            several.join(", ")
        ))),
    }
}

/// Имя пакета Android (`com.example.app`): части из букв, цифр и `_`, начинаются с буквы
/// или `_`, хотя бы две части. Проверка до запроса: имя попадает в путь.
pub fn validate_package_name(value: &str) -> Result<&str, ToolError> {
    let valid = value.len() <= 255
        && value.split('.').count() >= 2
        && value.split('.').all(|part| {
            part.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if valid {
        Ok(value)
    } else {
        Err(ToolError::new(format!(
            "invalid `{PACKAGE_NAME_ARG}` `{value}`: expected an application id like \
             com.example.app"
        )))
    }
}

/// Свойство схемы `packageName`: необязательное, по умолчанию — пакет профиля.
pub fn package_name_property() -> Value {
    json!({
        "type": "string",
        "description": "Package name of the app, like `com.example.app`. Optional when the \
            server profile has exactly one default package; `list_apps` or `search_apps` \
            lists the apps of the key."
    })
}

/// Схема аргументов: объект с `properties`, `required` и `additionalProperties: false`.
pub fn object_schema(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false
    });
    if !required.is_empty()
        && let Value::Object(map) = &mut schema
    {
        map.insert("required".to_owned(), json!(required));
    }
    schema
}

/// Ошибка «нет обязательного аргумента».
pub fn missing_argument(name: &str) -> ToolError {
    ToolError::new(format!("missing required argument `{name}`"))
}

/// Ошибка «аргумент не того типа»; `expected` — «a string», «true or false» и т. п.
pub fn invalid_argument(name: &str, expected: &str) -> ToolError {
    ToolError::new(format!("argument `{name}` must be {expected}"))
}

/// Строка поля JSON (строка или число как текст); нет поля или `null` — `None`.
pub fn text_field(value: &Value, field: &str) -> Option<String> {
    match value.get(field)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Оставляет у объекта только поля `fields` (в этом порядке), без `null`; не объект —
/// `null`. Помогает отдавать агенту компактные записи.
pub fn pick(value: &Value, fields: &[&str]) -> Value {
    let Value::Object(map) = value else {
        return Value::Null;
    };
    let mut out = serde_json::Map::new();
    for field in fields {
        if let Some(item) = map.get(*field).filter(|item| !item.is_null()) {
            out.insert((*field).to_owned(), item.clone());
        }
    }
    Value::Object(out)
}

/// Дата `ГГГГ-ММ-ДД` как номер дня от 1970-01-01 (пролептический григорианский
/// календарь). Аргументы периодов RuStore API — даты без времени.
pub fn parse_date(name: &str, value: &str) -> Result<i64, ToolError> {
    let invalid = || invalid_argument(name, "a date in the YYYY-MM-DD format, like 2026-01-31");
    let parts: Vec<&str> = value.trim().split('-').collect();
    let [year, month, day] = parts.as_slice() else {
        return Err(invalid());
    };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return Err(invalid());
    }
    let year: i64 = year.parse().map_err(|_| invalid())?;
    let month: u32 = month.parse().map_err(|_| invalid())?;
    let day: u32 = day.parse().map_err(|_| invalid())?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(invalid());
    }
    Ok(days_from_civil(year, month, day))
}

/// Необязательный аргумент-дата `ГГГГ-ММ-ДД`.
pub fn optional_date(args: &JsonObject, name: &str) -> Result<Option<i64>, ToolError> {
    optional_str(args, name)?
        .map(|value| parse_date(name, value))
        .transpose()
}

/// Номер дня (от 1970-01-01) как `ГГГГ-ММ-ДД`.
pub fn format_date(day: i64) -> String {
    let (year, month, day) = civil_from_days(day);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Сегодняшний день по UTC.
pub fn today_utc() -> i64 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    seconds.div_euclid(86_400)
}

/// День из начала строки даты или времени (`2026-01-31`, `2026-01-31T10:00:00Z`,
/// `2026-01-31 10:00:00.000`); не дата — `None`.
pub fn day_of(text: &str) -> Option<i64> {
    text.get(..10)
        .and_then(|date| parse_date("date", date).ok())
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Алгоритм `days_from_civil` Говарда Хиннанта.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = i64::from(month);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Обратный к [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Проверяет `save_to` до запросов к API, чтобы не читать данные впустую: путь внутри
/// разрешённых каталогов, файла с таким именем нет.
pub fn check_new_file(ctx: &Context, name: &str, save_to: &str) -> Result<(), ToolError> {
    ctx.files()
        .check_write(save_to)
        .map(|_| ())
        .map_err(|err| ToolError::new(format!("argument `{name}`: {err}")))
}

/// Записывает новый локальный файл `save_to` (аргумент `name`) и возвращает его
/// канонический путь.
///
/// Путь проверяет [`crate::files::FilePolicy`]; файл создаётся с `create_new` (существующий
/// файл и символическая ссылка не перезаписываются). Как в исполнителе запросов, после
/// создания сверяется, что путь по-прежнему канонический и ведёт к созданному файлу: если
/// каталог подменили символической ссылкой между проверкой и созданием, данные не пишутся.
pub fn write_new_file(
    ctx: &Context,
    name: &str,
    save_to: &str,
    contents: &[u8],
) -> Result<std::path::PathBuf, ToolError> {
    use std::io::Write;

    let path = ctx
        .files()
        .check_write(save_to)
        .map_err(|err| ToolError::new(format!("argument `{name}`: {err}")))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|err| ToolError::new(format!("cannot create `{save_to}`: {err}")))?;
    let changed = || {
        ToolError::new(format!(
            "`{save_to}` changed while it was being created (a directory was replaced); nothing \
             was written"
        ))
    };
    let created = file.metadata().map_err(|_| changed())?;
    let canonical = std::fs::canonicalize(&path).map_err(|_| changed())?;
    let current = std::fs::symlink_metadata(&path).map_err(|_| changed())?;
    if canonical != path || current.file_type().is_symlink() || !same_file(&current, &created) {
        return Err(changed());
    }
    if let Err(err) = file.write_all(contents).and_then(|()| file.flush()) {
        drop(file);
        // Частично записанный файл вводит в заблуждение — удаляем.
        let _ = std::fs::remove_file(&path);
        return Err(ToolError::new(format!("cannot write `{save_to}`: {err}")));
    }
    Ok(path)
}

/// Один и тот же файл: устройство и inode.
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Без inode (Windows) сравниваются тип, размер и время изменения; символическую ссылку
/// отсекает проверка канонического пути.
#[cfg(not(unix))]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.is_file() == b.is_file() && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

/// Поле CSV по RFC 4180: в кавычках, если есть разделитель, кавычка или перевод строки.
pub fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// Строка CSV из полей, с переводом строки в конце.
pub fn csv_line<S: AsRef<str>>(fields: &[S]) -> String {
    let mut line = fields
        .iter()
        .map(|field| csv_field(field.as_ref()))
        .collect::<Vec<_>>()
        .join(",");
    line.push_str("\r\n");
    line
}

#[cfg(test)]
mod tests {
    use rustore_api::spec::AccessLevel;

    use super::*;
    use crate::composite::testing;
    use crate::files::FilePolicy;

    fn args(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap()
    }

    fn ctx(packages: &[&str]) -> Context {
        testing::context(
            "http://127.0.0.1:9",
            AccessLevel::Read,
            packages,
            FilePolicy::disabled(),
        )
        .unwrap()
    }

    #[test]
    fn argument_helpers() {
        let a = args(json!({
            "s": " x ", "empty": "", "n": 5, "ns": "7", "neg": -1, "b": true,
            "list": ["a", " b ", ""], "csv": "a, b,,c", "bad": [1]
        }));
        assert_eq!(optional_str(&a, "s").unwrap(), Some("x"));
        assert_eq!(optional_str(&a, "empty").unwrap(), None);
        assert!(optional_str(&a, "n").is_err());
        assert!(required_str(&a, "missing").is_err());
        assert_eq!(optional_u64(&a, "n").unwrap(), Some(5));
        assert_eq!(optional_u64(&a, "ns").unwrap(), Some(7));
        assert!(optional_u64(&a, "neg").is_err());
        assert!(optional_u64(&a, "s").is_err());
        assert_eq!(bounded_u64(&a, "n", 1, 1, 10).unwrap(), 5);
        assert_eq!(bounded_u64(&a, "missing", 3, 1, 10).unwrap(), 3);
        assert!(bounded_u64(&a, "n", 1, 6, 10).is_err());
        assert!(bool_arg(&a, "b", false).unwrap());
        assert!(bool_arg(&a, "missing", true).unwrap());
        assert!(bool_arg(&a, "s", true).is_err());
        assert_eq!(string_list(&a, "list").unwrap(), ["a", "b"]);
        assert_eq!(string_list(&a, "csv").unwrap(), ["a", "b", "c"]);
        assert!(string_list(&a, "missing").unwrap().is_empty());
        assert!(string_list(&a, "bad").is_err());
    }

    #[test]
    fn package_name_comes_from_argument_or_profile() {
        let given = args(json!({"packageName": "com.example.app"}));
        let none = args(json!({}));
        assert_eq!(package_name(&ctx(&[]), &given).unwrap(), "com.example.app");
        assert_eq!(
            package_name(&ctx(&["com.example.default"]), &none).unwrap(),
            "com.example.default"
        );
        // Аргумент важнее профиля.
        assert_eq!(
            package_name(&ctx(&["com.example.default"]), &given).unwrap(),
            "com.example.app"
        );
        let err = package_name(&ctx(&[]), &none).unwrap_err();
        assert!(err.message().contains("pass `packageName`"), "{err}");
        let err = package_name(&ctx(&["com.example.one", "com.example.two"]), &none).unwrap_err();
        assert!(
            err.message().contains("com.example.one, com.example.two"),
            "{err}"
        );
        for bad in ["app", "com..x", "com.1x", "com/x.y", "../x.y", "com.x y"] {
            let err = package_name(&ctx(&[]), &args(json!({"packageName": bad}))).unwrap_err();
            assert!(
                err.message().contains("invalid `packageName`"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn schema_and_field_helpers() {
        let schema = object_schema(json!({"packageName": package_name_property()}), &[]);
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema.get("required").is_none());
        let schema = object_schema(json!({"q": {"type": "string"}}), &["q"]);
        assert_eq!(schema["required"], json!(["q"]));

        let value = json!({"a": 1, "b": null, "c": "x", "d": true});
        assert_eq!(
            pick(&value, &["c", "b", "a", "z"]),
            json!({"c": "x", "a": 1})
        );
        assert_eq!(pick(&json!([1]), &["a"]), Value::Null);
        assert_eq!(text_field(&value, "a").as_deref(), Some("1"));
        assert_eq!(text_field(&value, "c").as_deref(), Some("x"));
        assert_eq!(text_field(&value, "d").as_deref(), Some("true"));
        assert_eq!(text_field(&value, "b"), None);
    }

    #[test]
    fn date_helpers() {
        assert_eq!(parse_date("d", "1970-01-01").unwrap(), 0);
        assert_eq!(
            parse_date("d", "2026-03-01").unwrap() - parse_date("d", "2026-02-28").unwrap(),
            1
        );
        assert_eq!(
            parse_date("d", "2024-03-01").unwrap() - parse_date("d", "2024-02-28").unwrap(),
            2
        );
        for day in [-1000, 0, 19_000, 20_500, 100_000] {
            assert_eq!(parse_date("d", &format_date(day)).unwrap(), day);
        }
        assert_eq!(format_date(20_454), "2026-01-01");
        for bad in [
            "2026-1-01",
            "2026-02-30",
            "2025-02-29",
            "2026-13-01",
            "x",
            "2026-01-01T",
        ] {
            let err = parse_date("from", bad).unwrap_err();
            assert!(err.message().contains("`from`"), "{bad}: {err}");
        }
        assert_eq!(
            day_of("2026-01-31T10:00:00Z"),
            parse_date("d", "2026-01-31").ok()
        );
        assert_eq!(day_of("2026-01-31 10:00:00.000"), day_of("2026-01-31"));
        assert_eq!(day_of("soon"), None);
        assert!(today_utc() > 20_000);
        let a = args(json!({"from": "2026-01-02", "bad": "02.01.2026"}));
        assert_eq!(optional_date(&a, "from").unwrap(), Some(20_455));
        assert_eq!(optional_date(&a, "to").unwrap(), None);
        assert!(optional_date(&a, "bad").is_err());
    }

    #[test]
    fn csv_helpers() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\"\nbye"), "\"say \"\"hi\"\"\nbye\"");
        assert_eq!(csv_line(&["1", "x,y", ""]), "1,\"x,y\",\r\n");
    }

    #[test]
    fn new_files_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let local = testing::context(
            "http://127.0.0.1:9",
            AccessLevel::Read,
            &[],
            FilePolicy::new(std::slice::from_ref(&root)).unwrap(),
        )
        .unwrap();
        let target = root.join("out.txt");
        let target_text = target.display().to_string();
        check_new_file(&local, "save_to", &target_text).unwrap();
        let written = write_new_file(&local, "save_to", &target_text, b"first").unwrap();
        assert_eq!(written, target);
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        for err in [
            check_new_file(&local, "save_to", &target_text).unwrap_err(),
            write_new_file(&local, "save_to", &target_text, b"second").unwrap_err(),
        ] {
            assert!(err.message().contains("already exists"), "{err}");
            assert!(err.message().contains("`save_to`"), "{err}");
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        let outside = tempfile::tempdir().unwrap();
        let err = write_new_file(
            &local,
            "save_to",
            &outside.path().join("x.txt").display().to_string(),
            b"x",
        )
        .unwrap_err();
        assert!(err.message().contains("outside the allowed"), "{err}");
        // HTTP-режим: локальных файлов нет.
        let remote = ctx(&[]);
        let err = write_new_file(&remote, "save_to", &target_text, b"x").unwrap_err();
        assert!(
            err.message().contains("local files are not available"),
            "{err}"
        );
    }
}
