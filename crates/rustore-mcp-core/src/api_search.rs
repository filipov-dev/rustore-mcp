//! Инструмент `api_search`: поиск по всем операциям RuStore API.
//!
//! Ищет и по незарегистрированным операциям (выше `max_access` или вне `--toolsets`), чтобы
//! агент мог узнать, что операция существует и какой флаг её включает, а не изобретал обходные
//! пути. Работает локально по таблице операций, в API не обращается.
//!
//! Оценка простая и предсказуемая: запрос делится на слова (всё, что не буква и не цифра, —
//! разделитель), у каждого слова есть варианты ([`variants`]), каждый вариант ищется
//! подстрокой без учёта регистра в полях операции с весами ([`score`]); слово получает вес
//! лучшего поля среди вариантов. Сначала идут операции, совпавшие по большему числу слов,
//! затем по сумме весов, затем с более коротким именем, затем по алфавиту.
//!
//! Справочник RuStore API — на русском, а имена, пути и `operationId` — на английском, поэтому
//! варианты слова такие:
//!
//! - само слово (`ё` приводится к `е`);
//! - для русских слов — основа без окончания ([`russian_stem`]): «отзывы» → «отзыв» находит
//!   «отзыва» и «отзывов» в описаниях;
//! - английский перевод русской основы по словарю [`RU_EN`]: «отзыв» → `review` и `comment`
//!   находит пути `/public/v1/application/{packageName}/comment…` и имена инструментов;
//! - для английских слов — единственное число (`reviews` → `review`).
//!
//! Составные инструменты сервера ([`ToolRegistry::composite_tools`]) тоже входят в поиск:
//! по имени, заголовку, описанию и набору; в результате у них `kind: "composite"`.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use rustore_api::spec::{OperationDef, find, operations};
use serde_json::{Map, Value, json};

use crate::composite::CompositeTool;
use crate::dynamic_toolsets::ENABLE_TOOLSET_TOOL;
use crate::error::ToolError;
use crate::registry::{ToolRegistry, access_flag, access_name, composite_tool, uses_local_files};
use crate::toolsets::available_toolsets;

/// Имя инструмента.
pub const API_SEARCH_TOOL: &str = "api_search";

/// Сколько результатов по умолчанию и максимум.
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;

const ARGUMENTS: &[&str] = &["query", "tool_name", "toolset", "limit"];

/// Бонус за точное совпадение всего запроса с именем инструмента или `operationId`.
const EXACT_BONUS: u32 = 100;

/// Минимальная длина русской основы: короче — слово ищется как есть («поле», «цель»).
const MIN_STEM_CHARS: usize = 4;

/// Окончания русских слов, самые длинные первыми. Это не морфология, а грубое отсечение,
/// которого хватает для поиска подстрокой по описаниям справочника.
const RUSSIAN_ENDINGS: &[&str] = &[
    "иями", "ями", "ами", "его", "ого", "ему", "ому", "ыми", "ими", "иях", "ах", "ях", "ов", "ев",
    "ей", "ой", "ий", "ый", "ая", "яя", "ое", "ее", "ые", "ие", "ую", "юю", "ом", "ем", "ам", "ям",
    "а", "я", "о", "е", "ы", "и", "у", "ю", "ь", "й",
];

/// Перевод основ русских слов в слова, которые встречаются в путях и именах операций.
/// Ключ сравнивается с началом основы (или слова) запроса.
const RU_EN: &[(&str, &[&str])] = &[
    ("приложен", &["application", "app"]),
    ("верси", &["version"]),
    ("черновик", &["draft"]),
    ("релиз", &["version", "publish"]),
    ("сборк", &["apk", "aab"]),
    ("загруз", &["upload"]),
    ("файл", &["upload", "file", "export"]),
    ("отзыв", &["review", "comment"]),
    ("коммент", &["comment", "review"]),
    ("ответ", &["answer", "feedback"]),
    ("рейтинг", &["rating", "statistic"]),
    ("оценк", &["rating", "statistic"]),
    ("платеж", &["payment", "invoice"]),
    ("счет", &["invoice", "payment"]),
    ("оплат", &["payment", "invoice"]),
    ("подписк", &["subscription"]),
    ("покупк", &["purchase"]),
    ("возврат", &["refund"]),
    ("скриншот", &["screenshot"]),
    ("снимк", &["screenshot"]),
    ("иконк", &["icon"]),
    ("изображен", &["image", "icon", "screenshot"]),
    ("картинк", &["image", "icon", "screenshot"]),
    ("модерац", &["moderation", "commit"]),
    ("публикац", &["publish"]),
    ("опубликова", &["publish"]),
    ("раскатк", &["rollout", "publish"]),
    ("архив", &["archive"]),
    ("доступ", &["access", "developer"]),
    ("рол", &["role", "access"]),
    ("пользовател", &["user", "developer"]),
    ("продукт", &["product", "catalog"]),
    ("товар", &["product", "catalog"]),
    ("каталог", &["catalog"]),
    ("подтвер", &["confirm", "acknowledge"]),
    ("отмен", &["cancel"]),
    ("удал", &["delete", "revoke"]),
    ("статус", &["status"]),
    ("список", &["list"]),
    ("тестов", &["sandbox"]),
    ("песочниц", &["sandbox"]),
    ("настройк", &["settings"]),
    ("экспорт", &["export", "csv"]),
    ("выгруз", &["export", "csv"]),
    ("сводк", &["summary"]),
    ("обзор", &["summary"]),
    ("поиск", &["search", "list"]),
];

/// Описание инструмента.
pub fn api_search_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Words to search for in operation names, summaries, descriptions, \
                    paths and toolsets, in English or Russian, e.g. `review`, `draft apk`, \
                    `скриншот` or `/public/v1/application/{packageName}/comment`."
            },
            "tool_name": {
                "type": "string",
                "description": "Exact tool name of one operation (from a previous search) to get \
                    its full input schema instead of searching. Mutually exclusive with `query`."
            },
            "toolset": {
                "type": "string",
                "description": "Limit the search to one toolset (e.g. `reviews`). Without `query`, \
                    lists all operations of the toolset."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": format!("Maximum number of results (default {DEFAULT_LIMIT}, max {MAX_LIMIT}).")
            }
        },
        "additionalProperties": false
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    let description = "Search all RuStore API operations, including those whose tools \
        are not enabled on this server. Each result shows the tool name, HTTP method and path, \
        access level, toolset, required arguments, the API reference page, whether the tool is \
        registered here and, if not, which server flag enables it. Pass `tool_name` to get the \
        full input schema of one operation.\n\n\
        If the tool is registered, call it directly. If only its toolset is disabled, enable it \
        with `enable_toolset` when the server offers that tool, or call the same method and \
        path with `api_request`, or ask the user to add the toolset. If it needs a higher \
        access level, ask the user to restart the server with the named flag. Registered tools \
        and `api_request` accept an optional `jq` filter to shrink the response.\n\n\
        Access: read-only (does not call the API).";
    let mut tool = Tool::new(API_SEARCH_TOOL, description, Arc::new(schema));
    tool.title = Some("Search RuStore API operations".to_owned());
    tool.annotations = Some(
        ToolAnnotations::with_title("Search RuStore API operations")
            .read_only(true)
            .open_world(false),
    );
    tool
}

/// Выполняет поиск; результат — компактный JSON.
pub fn search(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    reject_unknown(args)?;
    let query = optional_string(args, "query")?.unwrap_or_default();
    let tool_name = optional_string(args, "tool_name")?;
    let toolset = optional_string(args, "toolset")?.filter(|slug| !slug.is_empty());
    let limit = parse_limit(args.get("limit"))?;

    if let Some(name) = tool_name.filter(|name| !name.is_empty()) {
        if !query.is_empty() {
            return Err(ToolError::new(
                "`query` and `tool_name` are mutually exclusive; pass `tool_name` to inspect one \
                 operation or `query` to search",
            ));
        }
        return describe(name, registry);
    }

    if let Some(slug) = toolset
        && !available_toolsets().iter().any(|t| t.slug == slug)
    {
        let list: Vec<&str> = available_toolsets().iter().map(|t| t.slug).collect();
        return Err(ToolError::new(format!(
            "unknown toolset `{slug}`; available toolsets: {}",
            list.join(", ")
        )));
    }
    if query.is_empty() && toolset.is_none() {
        return Err(ToolError::new(
            "pass `query` (words to search for), `toolset`, or `tool_name`",
        ));
    }

    let entries = operations()
        .iter()
        .map(Entry::Operation)
        .chain(registry.composite_tools().iter().map(Entry::Composite))
        .collect();
    let hits = rank_entries(entries, query, toolset);
    let total = hits.len();
    let results: Vec<Value> = hits
        .into_iter()
        .take(limit)
        .map(|(entry, _)| match entry {
            Entry::Operation(op) => summary_entry(op, registry),
            Entry::Composite(tool) => composite_entry(tool, registry),
        })
        .collect();
    let mut out = Map::new();
    out.insert("total_matches".into(), json!(total));
    out.insert("returned".into(), json!(results.len()));
    out.insert("results".into(), Value::Array(results));
    if total > limit {
        out.insert(
            "note".into(),
            json!("More operations match: refine the query, filter by `toolset` or raise `limit`."),
        );
    } else if total == 0 {
        out.insert(
            "note".into(),
            json!("Nothing matches: try other words (English API names such as `version`, `review`, `screenshot` work best) or list a toolset with `toolset`."),
        );
    }
    Ok(Value::Object(out).to_string())
}

/// Запись поиска: операция спецификации или составной инструмент.
#[derive(Debug, Clone, Copy)]
enum Entry<'a> {
    Operation(&'static OperationDef),
    Composite(&'a CompositeTool),
}

impl Entry<'_> {
    fn name(&self) -> &str {
        match self {
            Self::Operation(op) => op.tool_name,
            Self::Composite(tool) => tool.name,
        }
    }

    fn toolset(&self) -> &str {
        match self {
            Self::Operation(op) => op.toolset,
            Self::Composite(tool) => tool.toolset,
        }
    }

    /// Точное совпадение всего запроса: имя инструмента или `operationId`.
    fn is_exact(&self, whole: &str) -> bool {
        match self {
            Self::Operation(op) => whole == op.tool_name || whole == op.operation_id.to_lowercase(),
            Self::Composite(tool) => whole == tool.name,
        }
    }

    /// Поля для поиска с весами (см. [`score`]).
    fn fields(&self) -> Vec<(u32, String)> {
        match self {
            Self::Operation(op) => vec![
                (6, op.tool_name.to_lowercase()),
                (4, op.operation_id.to_lowercase()),
                (3, op.path.to_lowercase()),
                (3, toolset_text(op.toolset)),
                (2, normalize(op.summary)),
                (
                    1,
                    format!("{} {}", normalize(op.description), op.method.as_str()).to_lowercase(),
                ),
            ],
            Self::Composite(tool) => vec![
                (6, tool.name.to_lowercase()),
                (3, toolset_text(tool.toolset)),
                (2, normalize(tool.title)),
                (1, normalize(tool.description)),
            ],
        }
    }
}

/// Операции, отсортированные по релевантности, с оценкой.
pub fn rank(query: &str, toolset: Option<&str>) -> Vec<(&'static OperationDef, u32)> {
    let entries = operations().iter().map(Entry::Operation).collect();
    rank_entries(entries, query, toolset)
        .into_iter()
        .filter_map(|(entry, score)| match entry {
            Entry::Operation(op) => Some((op, score)),
            Entry::Composite(_) => None,
        })
        .collect()
}

/// Записи, отсортированные по релевантности, с оценкой.
fn rank_entries<'a>(
    entries: Vec<Entry<'a>>,
    query: &str,
    toolset: Option<&str>,
) -> Vec<(Entry<'a>, u32)> {
    let tokens = tokenize(query);
    let whole = normalize(query.trim());
    let mut hits: Vec<(Entry<'a>, usize, u32)> = entries
        .into_iter()
        .filter(|entry| toolset.is_none_or(|slug| entry.toolset() == slug))
        .filter_map(|entry| {
            if tokens.is_empty() {
                return Some((entry, 0, 0));
            }
            let (matched, mut score) = score(&entry.fields(), &tokens);
            if matched == 0 {
                return None;
            }
            if entry.is_exact(&whole) {
                score += EXACT_BONUS;
            }
            Some((entry, matched, score))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            // При равной оценке короче имя — меньше посторонних слов. Без запроса (только
            // набор) список идёт по алфавиту.
            .then_with(|| {
                if tokens.is_empty() {
                    std::cmp::Ordering::Equal
                } else {
                    a.0.name().len().cmp(&b.0.name().len())
                }
            })
            .then_with(|| a.0.name().cmp(b.0.name()))
    });
    hits.into_iter()
        .map(|(entry, _, score)| (entry, score))
        .collect()
}

/// Нижний регистр и `ё` → `е`.
fn normalize(text: &str) -> String {
    text.to_lowercase().replace('ё', "е")
}

/// Слова запроса без повторов, у каждого — список вариантов (см. описание модуля).
fn tokenize(query: &str) -> Vec<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    for word in query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = normalize(word);
        if !words.contains(&word) {
            words.push(word);
        }
    }
    words.iter().map(|word| variants(word)).collect()
}

/// Варианты одного слова: само слово, русская основа и её перевод, английское единственное
/// число.
fn variants(word: &str) -> Vec<String> {
    let mut out = vec![word.to_owned()];
    let mut push = |value: &str| {
        if !value.is_empty() && !out.iter().any(|known| known == value) {
            out.push(value.to_owned());
        }
    };
    if word.chars().any(is_cyrillic) {
        let stem = russian_stem(word);
        push(&stem);
        for (key, translations) in RU_EN {
            if word.starts_with(key) || stem.starts_with(key) {
                for translation in *translations {
                    push(translation);
                }
            }
        }
    } else if word.len() > 3 {
        if let Some(stem) = word.strip_suffix("ies") {
            push(&format!("{stem}y"));
        } else if let Some(singular) = word.strip_suffix('s') {
            push(singular);
        }
    }
    out
}

fn is_cyrillic(c: char) -> bool {
    matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')
}

/// Основа русского слова: без самого длинного подходящего окончания, если остаётся не меньше
/// [`MIN_STEM_CHARS`] букв.
fn russian_stem(word: &str) -> String {
    let length = word.chars().count();
    for ending in RUSSIAN_ENDINGS {
        if let Some(stem) = word.strip_suffix(ending)
            && length - ending.chars().count() >= MIN_STEM_CHARS
        {
            return stem.to_owned();
        }
    }
    word.to_owned()
}

/// Сколько слов совпало и сумма весов лучших полей. Веса полей: имя инструмента — 6,
/// `operationId` — 4, путь и набор — 3, заголовок — 2, описание — 1.
fn score(fields: &[(u32, String)], tokens: &[Vec<String>]) -> (usize, u32) {
    let mut matched = 0;
    let mut total = 0;
    for variants in tokens {
        let best = variants
            .iter()
            .flat_map(|needle| {
                fields
                    .iter()
                    .map(move |(weight, text)| field_score(text, needle, *weight))
            })
            .max()
            .unwrap_or(0);
        if best > 0 {
            matched += 1;
            total += best;
        }
    }
    (matched, total)
}

/// Вес совпадения слова с полем: начало слова поля (`version` в `get_version`, `/version/`) —
/// двойной вес, середина слова (`version` в `listversions`) — одинарный, нет совпадения — 0.
fn field_score(text: &str, needle: &str, weight: u32) -> u32 {
    let mut best = 0;
    for (index, _) in text.match_indices(needle) {
        let word_start = text[..index]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if word_start {
            return weight * 2;
        }
        best = weight;
    }
    best
}

/// Slug и название раздела справочника.
fn toolset_text(slug: &str) -> String {
    let tag = available_toolsets()
        .into_iter()
        .find(|t| t.slug == slug)
        .map(|t| t.tag)
        .unwrap_or_default();
    normalize(&format!("{slug} {tag}"))
}

/// Краткая запись операции для результатов поиска.
fn summary_entry(op: &'static OperationDef, registry: &ToolRegistry) -> Value {
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(op.tool_name));
    entry.insert("method".into(), json!(op.method.as_str()));
    entry.insert("path".into(), json!(op.path));
    entry.insert("summary".into(), json!(op.summary.trim()));
    entry.insert("access".into(), json!(access_name(op.access)));
    entry.insert("toolset".into(), json!(op.toolset));
    entry.insert("required_params".into(), json!(required_params(op)));
    if !op.docs_url.is_empty() {
        entry.insert("reference".into(), json!(op.docs_url));
    }
    insert_availability(&mut entry, Availability::of_operation(op), registry);
    Value::Object(entry)
}

/// Краткая запись составного инструмента для результатов поиска.
fn composite_entry(tool: &CompositeTool, registry: &ToolRegistry) -> Value {
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(tool.name));
    entry.insert("kind".into(), json!("composite"));
    entry.insert("summary".into(), json!(tool.title.trim()));
    entry.insert("access".into(), json!(access_name(tool.access)));
    entry.insert("toolset".into(), json!(tool.toolset));
    let schema = (tool.input_schema)();
    let required: Vec<Value> = schema
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    entry.insert("required_params".into(), Value::Array(required));
    insert_availability(&mut entry, Availability::of_composite(tool), registry);
    Value::Object(entry)
}

/// Что нужно знать о записи, чтобы сказать, как её включить.
struct Availability<'a> {
    name: &'a str,
    access: rustore_api::AccessLevel,
    toolset: &'a str,
    local_files: bool,
    /// Можно вызвать тем же методом и путём через `api_request` (у составных — нет).
    via_api_request: bool,
}

impl<'a> Availability<'a> {
    fn of_operation(op: &'a OperationDef) -> Self {
        Self {
            name: op.tool_name,
            access: op.access,
            toolset: op.toolset,
            local_files: uses_local_files(op),
            via_api_request: true,
        }
    }

    fn of_composite(tool: &'a CompositeTool) -> Self {
        Self {
            name: tool.name,
            access: tool.access,
            toolset: tool.toolset,
            local_files: tool.uses_local_files,
            via_api_request: false,
        }
    }
}

/// Полное описание одной операции со схемой аргументов.
fn describe(name: &str, registry: &ToolRegistry) -> Result<String, ToolError> {
    if let Some(tool) = registry
        .composite_tools()
        .iter()
        .find(|tool| tool.name == name)
    {
        return describe_composite(tool, registry);
    }
    let Some(op) = find(name) else {
        let similar: Vec<&str> = rank(name, None)
            .into_iter()
            .take(5)
            .map(|(op, _)| op.tool_name)
            .collect();
        let hint = if similar.is_empty() {
            "use `query` to search".to_owned()
        } else {
            format!("similar: {}", similar.join(", "))
        };
        return Err(ToolError::new(format!(
            "unknown operation `{name}`; {hint}"
        )));
    };
    let schema: Value = serde_json::from_str(op.input_schema).unwrap_or(Value::Null);
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(op.tool_name));
    entry.insert("operation_id".into(), json!(op.operation_id));
    entry.insert("method".into(), json!(op.method.as_str()));
    entry.insert("path".into(), json!(op.path));
    entry.insert("summary".into(), json!(op.summary.trim()));
    entry.insert("description".into(), json!(op.description.trim()));
    entry.insert("access".into(), json!(access_name(op.access)));
    entry.insert("toolset".into(), json!(op.toolset));
    if !op.docs_url.is_empty() {
        entry.insert("reference".into(), json!(op.docs_url));
    }
    insert_availability(&mut entry, Availability::of_operation(op), registry);
    entry.insert("input_schema".into(), schema);
    Ok(Value::Object(entry).to_string())
}

/// Полное описание составного инструмента со схемой аргументов (с `jq`).
fn describe_composite(tool: &CompositeTool, registry: &ToolRegistry) -> Result<String, ToolError> {
    let described = composite_tool(tool).map_err(ToolError::new)?;
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(tool.name));
    entry.insert("kind".into(), json!("composite"));
    entry.insert("summary".into(), json!(tool.title.trim()));
    entry.insert("description".into(), json!(described.description));
    entry.insert("access".into(), json!(access_name(tool.access)));
    entry.insert("toolset".into(), json!(tool.toolset));
    insert_availability(&mut entry, Availability::of_composite(tool), registry);
    entry.insert(
        "input_schema".into(),
        Value::Object((*described.input_schema).clone()),
    );
    Ok(Value::Object(entry).to_string())
}

/// `registered`, а для незарегистрированных — флаги (`enable_with`) и что делать (`note`).
fn insert_availability(
    entry: &mut Map<String, Value>,
    item: Availability<'_>,
    registry: &ToolRegistry,
) {
    let registered = registry.get(item.name).is_some();
    entry.insert("registered".into(), json!(registered));
    if registered {
        return;
    }
    if !registry.local_files() && item.local_files {
        entry.insert(
            "note".into(),
            json!("Not available on this server: it reads or writes a local file, and this server runs on another machine (HTTP mode). Ask the user to upload or download the file in the RuStore console, or to use a local (stdio) server."),
        );
        return;
    }
    let mut flags = Vec::new();
    let access_ok = registry.max_access().allows(item.access);
    if !access_ok {
        flags.push(access_flag(item.access).to_owned());
    }
    let alternative = if item.via_api_request {
        ", or call the same method and path with `api_request`"
    } else {
        ""
    };
    if registry.is_dynamic() {
        // Набор агент включает сам; флаг нужен только для уровня доступа.
        if access_ok {
            entry.insert(
                "enable_with".into(),
                json!(format!(
                    "{ENABLE_TOOLSET_TOOL} {{\"toolset\": \"{}\"}}",
                    item.toolset
                )),
            );
            entry.insert(
                "note".into(),
                json!(format!(
                    "Its toolset is not enabled: call `{ENABLE_TOOLSET_TOOL}` with this \
                     toolset{alternative}."
                )),
            );
            return;
        }
    } else if !registry.toolset_enabled(item.toolset) {
        flags.push(format!("--toolsets {}", item.toolset));
    }
    entry.insert("enable_with".into(), json!(flags.join(" ")));
    let note = if access_ok && item.via_api_request {
        "Its toolset is not enabled: call the same method and path with `api_request`, or ask \
         the user to add the toolset to --toolsets."
    } else if access_ok {
        "Its toolset is not enabled: ask the user to add the toolset to --toolsets."
    } else {
        "Needs a higher access level than this server allows: ask the user to restart the server \
         with the flag in `enable_with` if the change is intended."
    };
    entry.insert("note".into(), json!(note));
}

/// Имена обязательных параметров операции.
fn required_params(op: &OperationDef) -> Vec<&'static str> {
    op.params
        .iter()
        .filter(|param| param.required)
        .map(|param| param.name)
        .collect()
}

fn reject_unknown(args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !ARGUMENTS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: `query`, `tool_name`, `toolset`, `limit`",
        unknown.join(", ")
    )))
}

fn optional_string<'a>(args: &'a JsonObject, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim())),
        Some(_) => Err(ToolError::new(format!(
            "argument `{name}` must be a string"
        ))),
    }
}

fn parse_limit(value: Option<&Value>) -> Result<usize, ToolError> {
    let limit = match value {
        None | Some(Value::Null) => return Ok(DEFAULT_LIMIT),
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        Some(_) => None,
    };
    match limit {
        Some(0) | None => Err(ToolError::new(format!(
            "argument `limit` must be a positive integer (max {MAX_LIMIT})"
        ))),
        Some(n) => Ok(usize::try_from(n).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)),
    }
}

#[cfg(test)]
mod tests {
    use rustore_api::AccessLevel;
    use serde_json::json;

    use super::*;
    use crate::ServerOptions;

    fn registry(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ToolRegistry {
        ToolRegistry::new(&ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            ..ServerOptions::default()
        })
        .unwrap()
    }

    fn run(value: Value, registry: &ToolRegistry) -> Result<Value, ToolError> {
        let Value::Object(args) = value else {
            panic!("не объект")
        };
        search(&args, registry).map(|text| serde_json::from_str(&text).unwrap())
    }

    fn names(result: &Value) -> Vec<&str> {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["tool_name"].as_str().unwrap())
            .collect()
    }

    fn ranked(query: &str) -> Vec<&'static str> {
        rank(query, None)
            .into_iter()
            .map(|(op, _)| op.tool_name)
            .collect()
    }

    #[test]
    fn tokenize_splits_dedupes_and_adds_variants() {
        let tokens = tokenize("Reviews  draft-Draft ОТЗЫВ отзывы Счёт");
        let first: Vec<&str> = tokens.iter().map(|v| v[0].as_str()).collect();
        assert_eq!(first, ["reviews", "draft", "отзыв", "отзывы", "счет"]);
        assert!(tokens[0].contains(&"review".to_owned()));
        assert!(tokens[2].contains(&"review".to_owned()));
        assert!(tokens[3].contains(&"отзыв".to_owned()));
        assert!(tokens[3].contains(&"review".to_owned()));
        assert!(tokens[4].contains(&"invoice".to_owned()));
        assert!(tokenize("  ").is_empty());
    }

    #[test]
    fn word_starts_weigh_more() {
        assert_eq!(field_score("get_version", "version", 3), 6);
        assert_eq!(
            field_score("/public/v1/application/{packagename}/version", "version", 3),
            6
        );
        assert_eq!(field_score("list_subversions", "version", 3), 3);
        assert_eq!(field_score("получение статуса версий", "верси", 2), 4);
        assert_eq!(field_score("get_app_rating", "version", 3), 0);
    }

    #[test]
    fn russian_stems() {
        assert_eq!(russian_stem("версии"), "верси");
        assert_eq!(russian_stem("черновика"), "черновик");
        assert_eq!(russian_stem("комментарий"), "комментар");
        assert_eq!(russian_stem("опубликованные"), "опубликованн");
        // Короткие слова не обрезаются.
        assert_eq!(russian_stem("поле"), "поле");
        assert_eq!(russian_stem("цель"), "цель");
    }

    #[test]
    fn reviews_in_russian_find_review_operations() {
        for query in ["отзыв", "отзывы", "Отзывов", "review", "reviews"] {
            let hits = ranked(query);
            assert!(!hits.is_empty(), "{query}");
            for name in [
                "list_reviews",
                "export_reviews_csv",
                "reply_to_review",
                "update_review_answer",
                "delete_review_answer",
            ] {
                assert!(hits.contains(&name), "{query}: {name} нет в {hits:?}");
            }
            // Лучшие совпадения — операции отзывов.
            for name in hits.iter().take(5) {
                let op = find(name).unwrap();
                assert_eq!(op.toolset, "reviews", "{query}: {name}");
            }
        }
        let hits = ranked("ответ на отзыв");
        assert_eq!(find(hits[0]).unwrap().toolset, "reviews");
        assert!(
            hits.iter()
                .take(3)
                .any(|name| name.contains("answer") || name.contains("reply")),
            "{hits:?}"
        );
    }

    #[test]
    fn draft_finds_version_operations() {
        let commit = find("submit_draft_for_moderation").unwrap();
        assert_eq!(
            commit.path,
            "/public/v1/application/{packageName}/version/{versionId}/commit"
        );
        for query in ["draft", "drafts", "черновик", "черновика"] {
            let hits = ranked(query);
            let top: Vec<&str> = hits.iter().take(6).copied().collect();
            for name in [
                "create_draft_version",
                "update_draft_version",
                "delete_draft_version",
                "submit_draft_for_moderation",
            ] {
                assert!(top.contains(&name), "{query}: {name} нет в {top:?}");
            }
        }
        // Поиск по пути.
        let hits = ranked("/public/v1/application/{packageName}/version/{versionId}/commit");
        assert_eq!(hits[0], "submit_draft_for_moderation", "{hits:?}");
        // Перевод русских слов: модерация, скриншот, подписка.
        assert_eq!(ranked("модерация")[0], "submit_draft_for_moderation");
        assert!(
            ranked("скриншоты")
                .iter()
                .take(4)
                .all(|name| name.contains("screenshot"))
        );
        assert_eq!(
            find(ranked("подписка")[0]).unwrap().toolset,
            "subscriptions"
        );
    }

    #[test]
    fn russian_store_words_are_translated() {
        // Основные понятия справочника RuStore находят операции своего раздела.
        for (query, toolset) in [
            ("версия", "releases"),
            ("черновик", "releases"),
            ("модерация", "releases"),
            ("публикация", "releases"),
            ("отзыв", "reviews"),
            ("ответ", "reviews"),
            ("платёж", "purchases"),
            ("подписка", "subscriptions"),
            ("покупка", "purchases"),
            ("возврат", "purchases"),
            ("скриншот", "listing"),
            ("иконка", "listing"),
            ("доступ", "access"),
            ("роль", "access"),
            ("каталог", "catalog"),
        ] {
            let hits = ranked(query);
            assert!(!hits.is_empty(), "{query}");
            assert_eq!(find(hits[0]).unwrap().toolset, toolset, "{query}: {hits:?}");
        }
    }

    #[test]
    fn ranking_prefers_exact_names_and_paths() {
        let hits = rank("list_reviews", None);
        assert_eq!(hits[0].0.tool_name, "list_reviews");
        assert!(hits[0].1 >= EXACT_BONUS);
        assert_eq!(
            ranked("/public/v1/application/{packageName}/comment/export")[0],
            "export_reviews_csv"
        );
        assert!(rank("zzzqqq", None).is_empty());
    }

    #[test]
    fn results_show_registration_and_flags() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"query": "черновик", "limit": 10}), &read).unwrap();
        let entries = result["results"].as_array().unwrap();
        let create = entries
            .iter()
            .find(|r| r["tool_name"] == "create_draft_version")
            .unwrap();
        assert_eq!(create["method"], "POST");
        assert_eq!(
            create["path"],
            "/public/v1/application/{packageName}/version"
        );
        assert_eq!(create["access"], "write");
        assert_eq!(create["toolset"], "releases");
        assert_eq!(create["registered"], false);
        assert_eq!(create["enable_with"], "--allow-write");
        assert!(
            create["reference"]
                .as_str()
                .unwrap()
                .starts_with("https://")
        );
        assert!(
            create["required_params"]
                .as_array()
                .unwrap()
                .contains(&json!("packageName"))
        );
        // Удаление черновика — запись, отправка на модерацию — удаление.
        let delete = entries
            .iter()
            .find(|r| r["tool_name"] == "delete_draft_version")
            .unwrap();
        assert_eq!(delete["enable_with"], "--allow-write");
        let commit = entries
            .iter()
            .find(|r| r["tool_name"] == "submit_draft_for_moderation")
            .unwrap();
        assert_eq!(commit["enable_with"], "--allow-destructive");
        let list = run(json!({"tool_name": "list_versions"}), &read).unwrap();
        assert_eq!(list["registered"], true);
        assert!(list.get("enable_with").is_none());

        // Набор выключен: флаг набора и подсказка про `api_request`.
        let catalog = registry(AccessLevel::Read, Some(&["catalog"]));
        let result = run(json!({"query": "отзыв"}), &catalog).unwrap();
        let first = &result["results"][0];
        assert_eq!(first["registered"], false);
        let flags = first["enable_with"].as_str().unwrap();
        assert!(flags.contains("--toolsets reviews"), "{first}");
        let reads = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["access"] == "read-only")
            .unwrap();
        assert_eq!(reads["enable_with"], "--toolsets reviews");
        assert!(reads["note"].as_str().unwrap().contains("api_request"));

        // Выше уровня и вне набора — оба флага.
        let result = run(json!({"tool_name": "delete_review_answer"}), &catalog).unwrap();
        assert_eq!(
            result["enable_with"],
            "--allow-destructive --toolsets reviews"
        );
    }

    #[test]
    fn dynamic_mode_suggests_enable_toolset() {
        let dynamic = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Read,
            dynamic_toolsets: true,
            ..ServerOptions::default()
        })
        .unwrap();
        let result = run(json!({"tool_name": "get_app_rating"}), &dynamic).unwrap();
        assert_eq!(result["registered"], false);
        assert_eq!(
            result["enable_with"],
            r#"enable_toolset {"toolset": "reviews"}"#
        );
        assert!(
            result["note"]
                .as_str()
                .unwrap()
                .contains("`enable_toolset`")
        );

        // Выше уровня доступа — только флаг сервера, без `--toolsets`.
        let result = run(json!({"tool_name": "create_draft_version"}), &dynamic).unwrap();
        assert_eq!(result["enable_with"], "--allow-write");
    }

    #[test]
    fn remote_server_marks_file_tools() {
        let remote = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Destructive,
            toolsets: Some(vec!["all".into()]),
            local_machine: false,
            ..ServerOptions::default()
        })
        .unwrap();
        let result = run(json!({"tool_name": "upload_apk"}), &remote).unwrap();
        assert_eq!(result["registered"], false);
        assert!(result["note"].as_str().unwrap().contains("HTTP mode"));
    }

    #[test]
    fn limits_and_toolset_filter() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"query": "application"}), &read).unwrap();
        assert_eq!(result["returned"], DEFAULT_LIMIT);
        assert!(result["total_matches"].as_u64().unwrap() > DEFAULT_LIMIT as u64);
        assert!(result["note"].is_string());

        let result = run(json!({"query": "application", "limit": 1000}), &read).unwrap();
        assert_eq!(
            result["returned"].as_u64(),
            Some(
                result["total_matches"]
                    .as_u64()
                    .unwrap()
                    .min(MAX_LIMIT as u64)
            )
        );
        let result = run(json!({"query": "application", "limit": "2"}), &read).unwrap();
        assert_eq!(result["returned"], 2);

        for bad in [json!(0), json!(-1), json!(1.5), json!("x"), json!(true)] {
            let err = run(json!({"query": "application", "limit": bad}), &read).unwrap_err();
            assert!(err.message().contains("`limit`"), "{err}");
        }

        // Только набор: все его операции по имени.
        let result = run(json!({"toolset": "reviews", "limit": 100}), &read).unwrap();
        let listed = names(&result);
        assert!(!listed.is_empty());
        assert!(listed.windows(2).all(|w| w[0] <= w[1]));
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["toolset"] == "reviews")
        );

        let result = run(json!({"query": "answer", "toolset": "reviews"}), &read).unwrap();
        assert!(!names(&result).is_empty());
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["toolset"] == "reviews")
        );

        let err = run(json!({"toolset": "Отзывы"}), &read).unwrap_err();
        assert!(err.message().contains("unknown toolset `Отзывы`"), "{err}");

        let result = run(json!({"query": "zzzqqq"}), &read).unwrap();
        assert_eq!(result["total_matches"], 0);
        assert!(result["note"].as_str().unwrap().contains("Nothing matches"));
    }

    #[test]
    fn tool_name_returns_full_schema() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"tool_name": "create_draft_version"}), &read).unwrap();
        assert_eq!(result["tool_name"], "create_draft_version");
        assert_eq!(result["operation_id"], "create_draft_version");
        assert_eq!(result["method"], "POST");
        assert_eq!(result["registered"], false);
        assert_eq!(result["enable_with"], "--allow-write");
        assert_eq!(result["input_schema"]["type"], "object");
        assert!(result["input_schema"]["properties"]["body"].is_object());

        let err = run(json!({"tool_name": "get_versio"}), &read).unwrap_err();
        assert!(
            err.message().contains("unknown operation `get_versio`"),
            "{err}"
        );
        assert!(err.message().contains("get_version"), "{err}");
    }

    #[test]
    fn argument_errors() {
        let read = registry(AccessLevel::Read, None);
        assert!(
            run(json!({}), &read)
                .unwrap_err()
                .message()
                .contains("pass `query`")
        );
        assert!(
            run(json!({"query": "x", "tool_name": "list_reviews"}), &read)
                .unwrap_err()
                .message()
                .contains("mutually exclusive")
        );
        assert!(
            run(json!({"query": 1}), &read)
                .unwrap_err()
                .message()
                .contains("must be a string")
        );
        assert!(
            run(json!({"q": "x"}), &read)
                .unwrap_err()
                .message()
                .contains("unknown argument(s) `q`")
        );
    }

    #[test]
    fn tool_is_read_only() {
        let tool = api_search_tool();
        let a = tool.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(true));
        assert_eq!(tool.input_schema["additionalProperties"], false);
    }

    fn with_fakes(max_access: AccessLevel) -> ToolRegistry {
        use crate::composite::testing::fake_tool;
        ToolRegistry::new(&ServerOptions {
            max_access,
            composite_tools: vec![
                fake_tool(
                    "summarize_fake_releases",
                    "releases",
                    AccessLevel::Read,
                    false,
                ),
                fake_tool("fake_catalog_sync", "catalog", AccessLevel::Write, false),
            ],
            ..ServerOptions::default()
        })
        .unwrap()
    }

    #[test]
    fn composite_tools_are_found_and_described() {
        let registry = with_fakes(AccessLevel::Read);
        let result = run(json!({"query": "summarize fake releases"}), &registry).unwrap();
        let first = &result["results"][0];
        assert_eq!(first["tool_name"], "summarize_fake_releases", "{result}");
        assert_eq!(first["kind"], "composite");
        assert_eq!(first["registered"], true);
        assert_eq!(first["summary"], "Fake composite tool");

        // Набор и уровень выше разрешённого: подсказка с флагами, без `api_request`.
        let result = run(json!({"query": "fake_catalog_sync"}), &registry).unwrap();
        let first = &result["results"][0];
        assert_eq!(first["tool_name"], "fake_catalog_sync", "{result}");
        assert_eq!(first["registered"], false);
        let enable = first["enable_with"].as_str().unwrap();
        assert!(enable.contains("--allow-write"), "{enable}");
        assert!(enable.contains("--toolsets catalog"), "{enable}");
        assert!(!first["note"].as_str().unwrap().contains("api_request"));

        // Фильтр по набору работает и для составных.
        let result = run(json!({"query": "fake", "toolset": "releases"}), &registry).unwrap();
        assert!(
            names(&result).contains(&"summarize_fake_releases"),
            "{result}"
        );
        assert!(!names(&result).contains(&"fake_catalog_sync"), "{result}");

        // `tool_name` описывает составной инструмент со схемой и аргументом `jq`.
        let described = run(json!({"tool_name": "summarize_fake_releases"}), &registry).unwrap();
        assert_eq!(described["kind"], "composite");
        assert!(described["input_schema"]["properties"]["jq"].is_object());
        assert!(described["input_schema"]["properties"]["text"].is_object());
        assert!(
            described["description"]
                .as_str()
                .unwrap()
                .contains("Access: read-only.")
        );
    }
}
