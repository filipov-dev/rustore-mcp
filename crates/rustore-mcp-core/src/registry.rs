//! Реестр MCP-инструментов сервера.
//!
//! Строится один раз при запуске из `rustore_api::spec::operations()` с учётом
//! [`ServerOptions`]: инструменты выше `max_access` и вне включённых наборов не регистрируются,
//! поэтому не видны в `tools/list` и не вызываются. Для незарегистрированных, но известных
//! инструментов [`ToolRegistry::unavailable_reason`] объясняет, какой флаг их включает.
//!
//! Без [`ServerOptions::local_machine`] (HTTP-режим) не регистрируются операции с файловыми
//! аргументами (загрузка APK, AAB, иконки и скриншотов) и операции с двоичным ответом (скачивание файлов): файлы
//! машины сервера не имеют отношения к агенту.
//!
//! Схема аргументов инструмента — `input_schema` операции и два добавления: необязательный
//! фильтр ответа `jq` ([`crate::jq`]) у операций с JSON-ответом и обязательный аргумент
//! `save_to` (куда сохранить файл) у операций с двоичным ответом.
//!
//! Кроме операций, всегда (независимо от `--toolsets`) регистрируются `api_search` (поиск по
//! всем операциям, только чтение) и `api_request` (запрос к любому пути API в пределах уровня
//! доступа сервера; аннотации — по максимальному уровню).
//!
//! Составные инструменты ([`ServerOptions::composite_tools`], [`crate::composite`])
//! регистрируются так же, как операции: по уровню доступа, набору и признаку локальных
//! файлов; их схема — своя схема инструмента плюс `jq`.
//!
//! Пользовательские инструменты ([`crate::custom_tools`]) регистрируются по уровню доступа
//! своей операции, без учёта наборов (и в динамическом режиме — сразу при запуске); в
//! HTTP-режиме — только если операция не работает с локальными файлами. Совпадение имени со
//! встроенным инструментом — ошибка запуска.
//!
//! В режиме динамических наборов ([`ServerOptions::dynamic_toolsets`]) реестр — снимок для
//! текущего списка включённых наборов: сервер пересобирает его через
//! [`ToolRegistry::with_toolsets`] при `enable_toolset`/`disable_toolset`. Пустой список
//! в этом режиме означает «ни одного набора», а не набор по умолчанию.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use rustore_api::spec::{
    AccessLevel, HttpMethod, OperationDef, ParamType, find, find_toolset, operations,
};
use serde_json::{Value, json};

use crate::ServerOptions;
use crate::api_request::{API_REQUEST_TOOL, api_request_tool};
use crate::api_search::{API_SEARCH_TOOL, api_search_tool};
use crate::composite::CompositeTool;
use crate::custom_tools::check_names;
use crate::dynamic_toolsets::{
    ENABLE_TOOLSET_TOOL, disable_toolset_tool, enable_toolset_tool, list_toolsets_tool,
};
use crate::jq::{JQ_ARG, add_jq_argument, takes_jq};
use crate::request::{SAVE_TO_ARG, takes_save_to};
use crate::toolsets::{effective_toolsets, normalize_toolsets};

/// Что выполняет инструмент.
#[derive(Debug, Clone, Copy)]
pub enum ToolKind {
    /// Сгенерированная операция RuStore API.
    Operation(&'static OperationDef),
    /// Составной инструмент: индекс в `ServerOptions::composite_tools`.
    Composite(usize),
    /// `api_request`: запрос к произвольному пути API.
    ApiRequest,
    /// `api_search`: поиск по таблице операций, без обращения к API.
    ApiSearch,
    /// Пользовательский инструмент: индекс в `ServerOptions::custom_tools`.
    Custom(usize),
    /// `list_toolsets`: наборы инструментов и какие из них включены (динамический режим).
    ListToolsets,
    /// `enable_toolset`: включить набор (динамический режим).
    EnableToolset,
    /// `disable_toolset`: выключить набор (динамический режим).
    DisableToolset,
}

/// Зарегистрированный инструмент: описание для `tools/list` и обработчик.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub tool: Tool,
    pub kind: ToolKind,
    /// Уровень доступа инструмента; у `api_request` — максимальный уровень сервера.
    pub access: AccessLevel,
}

/// Набор инструментов, доступных при заданных [`ServerOptions`].
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    index: HashMap<String, usize>,
    /// Включённые наборы (slug).
    toolsets: Vec<String>,
    max_access: AccessLevel,
    /// Инструменты могут работать с локальными файлами ([`ServerOptions::local_machine`]).
    local_files: bool,
    /// Режим динамических наборов ([`ServerOptions::dynamic_toolsets`]).
    dynamic: bool,
    /// Составные инструменты сервера (зарегистрированные и нет).
    composite: Vec<CompositeTool>,
    /// Зарегистрированные пользовательские инструменты. Наборы их не ограничивают, поэтому
    /// при пересборке реестра (`with_toolsets`) они переносятся как есть.
    custom: Vec<RegisteredTool>,
    /// Пользовательские инструменты, которые не зарегистрированы: имя → причина для агента.
    disabled_custom: HashMap<String, String>,
}

/// Максимальная длина имени инструмента: клиенты добавляют к имени префикс сервера
/// (`mcp__<server>__`) и ограничивают итог 64 символами.
pub const MAX_TOOL_NAME_LEN: usize = 40;

/// Работает ли инструмент операции с локальными файлами: загрузка файла (multipart) или
/// сохранение двоичного ответа.
pub fn uses_local_files(op: &OperationDef) -> bool {
    takes_save_to(op)
        || op
            .params
            .iter()
            .any(|param| param.value_type == ParamType::File)
}

impl ToolRegistry {
    /// Строит реестр. Ошибка — неизвестный набор инструментов или некорректная схема
    /// в сгенерированной таблице (последнее — ошибка генератора).
    pub fn new(options: &ServerOptions) -> Result<Self, String> {
        let selected = match &options.toolsets {
            Some(names) => Some(normalize_toolsets(names)?),
            None => None,
        };
        let toolsets = if options.dynamic_toolsets {
            selected.unwrap_or_default()
        } else {
            effective_toolsets(selected.as_deref())
        };
        check_names(&options.custom_tools)?;
        check_composite(&options.composite_tools, options)?;
        let mut custom = Vec::new();
        let mut disabled_custom = HashMap::new();
        for (index, tool) in options.custom_tools.iter().enumerate() {
            let op = tool.operation;
            if !options.local_machine && uses_local_files(op) {
                disabled_custom.insert(
                    tool.name.clone(),
                    format!(
                        "custom tool `{}` is not available on this server: its operation `{}` \
                         reads or writes a local file, and this server runs on another machine \
                         (HTTP mode)",
                        tool.name, op.tool_name
                    ),
                );
            } else if !options.max_access.allows(op.access) {
                disabled_custom.insert(
                    tool.name.clone(),
                    format!(
                        "custom tool `{}` is disabled: it {} data in RuStore and the server runs \
                         with {} access. Ask the user to {} if this is intended",
                        tool.name,
                        access_verb(op.access),
                        access_name(options.max_access),
                        access_hint(op.access, options.local_machine),
                    ),
                );
            } else {
                custom.push(RegisteredTool {
                    tool: tool.tool()?,
                    kind: ToolKind::Custom(index),
                    access: op.access,
                });
            }
        }
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets,
            max_access: options.max_access,
            local_files: options.local_machine,
            dynamic: options.dynamic_toolsets,
            composite: options.composite_tools.clone(),
            custom,
            disabled_custom,
        }
        .populate()
    }

    /// Тот же реестр с другим списком включённых наборов (динамический режим).
    /// Ошибка — неизвестный набор.
    pub fn with_toolsets(&self, toolsets: &[String]) -> Result<Self, String> {
        let toolsets = normalize_toolsets(toolsets)?;
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets: if self.dynamic {
                toolsets
            } else {
                effective_toolsets(Some(&toolsets))
            },
            max_access: self.max_access,
            local_files: self.local_files,
            dynamic: self.dynamic,
            composite: self.composite.clone(),
            custom: self.custom.clone(),
            disabled_custom: self.disabled_custom.clone(),
        }
        .populate()
    }

    /// Заполняет пустой реестр инструментами по его настройкам.
    fn populate(self) -> Result<Self, String> {
        let mut registry = self;
        let max_access = registry.max_access;
        for op in operations() {
            if registry.operation_enabled(op) {
                let tool = operation_tool(op)?;
                registry.push(RegisteredTool {
                    tool,
                    kind: ToolKind::Operation(op),
                    access: op.access,
                });
            }
        }
        for (index, composite) in registry.composite.clone().iter().enumerate() {
            if registry.composite_enabled(composite) {
                registry.push(RegisteredTool {
                    tool: composite_tool(composite)?,
                    kind: ToolKind::Composite(index),
                    access: composite.access,
                });
            }
        }
        for name in [API_REQUEST_TOOL, API_SEARCH_TOOL] {
            if find(name).is_some() {
                return Err(format!(
                    "operation `{name}` clashes with the built-in tool of the same name"
                ));
            }
        }
        registry.push(RegisteredTool {
            tool: api_request_tool(max_access, registry.local_files),
            kind: ToolKind::ApiRequest,
            access: max_access,
        });
        registry.push(RegisteredTool {
            tool: api_search_tool(),
            kind: ToolKind::ApiSearch,
            access: AccessLevel::Read,
        });
        if registry.dynamic {
            // Управление наборами меняет только список инструментов этого сервера,
            // данных RuStore не касается — уровень чтения.
            for (tool, kind) in [
                (list_toolsets_tool(), ToolKind::ListToolsets),
                (enable_toolset_tool(), ToolKind::EnableToolset),
                (disable_toolset_tool(), ToolKind::DisableToolset),
            ] {
                registry.push(RegisteredTool {
                    tool,
                    kind,
                    access: AccessLevel::Read,
                });
            }
        }
        // Пользовательские инструменты пользователь настроил явно: они регистрируются
        // всегда, в том числе в динамическом режиме, и не входят ни в один набор.
        for tool in registry.custom.clone() {
            registry.push(tool);
        }
        Ok(registry)
    }

    fn operation_enabled(&self, op: &OperationDef) -> bool {
        self.max_access.allows(op.access)
            && self.toolset_enabled(op.toolset)
            && (self.local_files || !uses_local_files(op))
    }

    fn composite_enabled(&self, tool: &CompositeTool) -> bool {
        self.max_access.allows(tool.access)
            && self.toolset_enabled(tool.toolset)
            && (self.local_files || !tool.uses_local_files)
    }

    /// Составные инструменты сервера (зарегистрированные и нет).
    pub fn composite_tools(&self) -> &[CompositeTool] {
        &self.composite
    }

    /// Включён ли набор `slug`.
    pub fn toolset_enabled(&self, slug: &str) -> bool {
        self.toolsets.iter().any(|name| name == slug)
    }

    /// Включённые наборы (slug).
    pub fn enabled_toolsets(&self) -> &[String] {
        &self.toolsets
    }

    /// Максимальный уровень доступа сервера.
    pub fn max_access(&self) -> AccessLevel {
        self.max_access
    }

    /// Могут ли инструменты работать с локальными файлами (см. [`ServerOptions::local_machine`]).
    pub fn local_files(&self) -> bool {
        self.local_files
    }

    /// Режим динамических наборов: наборы включаются инструментом `enable_toolset`.
    pub fn is_dynamic(&self) -> bool {
        self.dynamic
    }

    fn push(&mut self, tool: RegisteredTool) {
        self.index
            .insert(tool.tool.name.to_string(), self.tools.len());
        self.tools.push(tool);
    }

    /// Зарегистрированные инструменты в порядке регистрации: операции по имени, составные,
    /// затем `api_request`, `api_search`, инструменты динамических наборов и пользовательские.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredTool> {
        self.tools.iter()
    }

    /// Описания инструментов для `tools/list`.
    pub fn tools(&self) -> Vec<Tool> {
        self.tools.iter().map(|t| t.tool.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.index.get(name).and_then(|&i| self.tools.get(i))
    }

    /// Почему известный инструмент не зарегистрирован; `None` — инструмент неизвестен или
    /// зарегистрирован.
    pub fn unavailable_reason(&self, name: &str) -> Option<String> {
        if self.get(name).is_some() {
            return None;
        }
        if let Some(reason) = self.disabled_custom.get(name) {
            return Some(reason.clone());
        }
        if let Some(tool) = self.composite.iter().find(|tool| tool.name == name) {
            return self.composite_unavailable(tool);
        }
        let op = find(name)?;
        if !self.local_files && uses_local_files(op) {
            return Some(format!(
                "tool `{name}` is not available on this server: it reads or writes a local file, \
                 and this server runs on another machine (HTTP mode) and cannot access the \
                 agent's files. Ask the user to upload or download the file in the RuStore console, or to \
                 use a local (stdio) `rustore-mcp serve`"
            ));
        }
        if !self.max_access.allows(op.access) {
            return Some(format!(
                "tool `{name}` is disabled: it {} data in RuStore and the server runs with {} \
                 access. Ask the user to {} if this change is intended",
                access_verb(op.access),
                access_name(self.max_access),
                access_hint(op.access, self.local_files),
            ));
        }
        if !self.toolset_enabled(op.toolset) {
            if self.dynamic {
                return Some(format!(
                    "tool `{name}` belongs to toolset `{slug}`, which is not enabled: call \
                     `{ENABLE_TOOLSET_TOOL}` with `toolset: \"{slug}\"` first, or call the same \
                     operation with `api_request` ({} {})",
                    op.method.as_str(),
                    op.path,
                    slug = op.toolset,
                ));
            }
            return Some(format!(
                "tool `{name}` belongs to toolset `{}`, which is not enabled; ask the user to add \
                 it to --toolsets (enabled: {})",
                op.toolset,
                self.toolsets.join(", ")
            ));
        }
        None
    }
}

impl ToolRegistry {
    /// Почему составной инструмент не зарегистрирован.
    fn composite_unavailable(&self, tool: &CompositeTool) -> Option<String> {
        let name = tool.name;
        if !self.local_files && tool.uses_local_files {
            return Some(format!(
                "tool `{name}` is not available on this server: it reads or writes local files, \
                 and this server runs on another machine (HTTP mode)"
            ));
        }
        if !self.max_access.allows(tool.access) {
            return Some(format!(
                "tool `{name}` is disabled: it {} data in RuStore and the server runs with {} \
                 access. Ask the user to {} if this change is intended",
                access_verb(tool.access),
                access_name(self.max_access),
                access_hint(tool.access, self.local_files),
            ));
        }
        if !self.toolset_enabled(tool.toolset) {
            let slug = tool.toolset;
            if self.dynamic {
                return Some(format!(
                    "tool `{name}` belongs to toolset `{slug}`, which is not enabled: call \
                     `{ENABLE_TOOLSET_TOOL}` with `toolset: \"{slug}\"` first"
                ));
            }
            return Some(format!(
                "tool `{name}` belongs to toolset `{slug}`, which is not enabled; ask the user to \
                 add it to --toolsets (enabled: {})",
                self.toolsets.join(", ")
            ));
        }
        None
    }
}

/// Проверяет составные инструменты: имена уникальны, не совпадают с операциями, встроенными
/// и пользовательскими инструментами, не длиннее [`MAX_TOOL_NAME_LEN`], набор существует,
/// схема — объект без `jq`.
fn check_composite(tools: &[CompositeTool], options: &ServerOptions) -> Result<(), String> {
    let mut seen: Vec<&str> = BUILT_IN_TOOLS.to_vec();
    seen.extend(options.custom_tools.iter().map(|tool| tool.name.as_str()));
    for tool in tools {
        let name = tool.name;
        if seen.contains(&name) {
            return Err(format!(
                "composite tool `{name}` is defined twice or clashes with a built-in or custom \
                 tool"
            ));
        }
        if find(name).is_some() {
            return Err(format!(
                "composite tool `{name}` clashes with the operation tool of the same name"
            ));
        }
        if name.is_empty() || name.len() > MAX_TOOL_NAME_LEN {
            return Err(format!(
                "composite tool name `{name}` must be 1-{MAX_TOOL_NAME_LEN} characters long"
            ));
        }
        if find_toolset(tool.toolset).is_none() {
            return Err(format!(
                "composite tool `{name}` belongs to an unknown toolset `{}`",
                tool.toolset
            ));
        }
        seen.push(name);
    }
    Ok(())
}

/// Встроенные инструменты ядра (не операции).
pub const BUILT_IN_TOOLS: &[&str] = &[
    API_REQUEST_TOOL,
    API_SEARCH_TOOL,
    crate::dynamic_toolsets::LIST_TOOLSETS_TOOL,
    ENABLE_TOOLSET_TOOL,
    crate::dynamic_toolsets::DISABLE_TOOLSET_TOOL,
];

/// Описание MCP-инструмента для составного инструмента: его схема с аргументом `jq`,
/// описание с уровнем доступа, заголовок и аннотации по уровню.
pub fn composite_tool(composite: &CompositeTool) -> Result<Tool, String> {
    let name = composite.name;
    let Value::Object(mut schema) = (composite.input_schema)() else {
        return Err(format!(
            "composite tool `{name}`: the input schema is not a JSON object"
        ));
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!(
            "composite tool `{name}`: the input schema must have `type: object`"
        ));
    }
    if schema
        .get("properties")
        .and_then(|properties| properties.get(JQ_ARG))
        .is_some()
    {
        return Err(format!(
            "{name}: parameter `{JQ_ARG}` clashes with the response filter argument"
        ));
    }
    add_jq_argument(&mut schema);
    let mut description = composite.description.trim().to_owned();
    if composite.uses_local_files {
        description.push_str(
            "\n\nFile arguments take paths inside the directories allowed by the server (see \
             the server instructions); existing files are never overwritten.",
        );
    }
    description.push_str("\n\nAccess: ");
    description.push_str(access_description(composite.access));
    let mut tool = Tool::new(name, description, Arc::new(schema));
    let title = composite.title.trim();
    if !title.is_empty() {
        tool.title = Some(title.to_owned());
    }
    tool.annotations = Some(access_annotations(title, composite.access, None));
    Ok(tool)
}

/// Описание MCP-инструмента для операции.
///
/// Схема — `input_schema` операции плюс `jq` (у операций с двоичным ответом — вместо него
/// `save_to`), описание — `summary`, `description`, ссылка на справочник и уровень доступа.
pub fn operation_tool(op: &'static OperationDef) -> Result<Tool, String> {
    let mut schema: JsonObject = serde_json::from_str(op.input_schema)
        .map_err(|err| format!("{}: invalid input schema: {err}", op.tool_name))?;
    if takes_save_to(op) {
        add_save_to(op, &mut schema)?;
    }
    if takes_jq(op) {
        if schema
            .get("properties")
            .and_then(|properties| properties.get(JQ_ARG))
            .is_some()
        {
            return Err(format!(
                "{}: parameter `{JQ_ARG}` clashes with the response filter argument",
                op.tool_name
            ));
        }
        add_jq_argument(&mut schema);
    }

    let summary = op.summary.trim();
    let mut description = summary.to_owned();
    let details = op.description.trim();
    if !details.is_empty() && details != summary {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(details);
    }
    if !op.docs_url.is_empty() {
        description.push_str("\n\nReference: ");
        description.push_str(op.docs_url);
    }
    if op.params.iter().any(|p| p.value_type == ParamType::File) {
        description.push_str(
            "\n\nFile arguments take a path to a local file inside the directories allowed by the \
             server (see the server instructions).",
        );
    }
    if takes_save_to(op) {
        description.push_str(
            "\n\nThe response is a file: it is saved to `save_to` (a new file inside the \
             allowed directories) and the result reports its path and size.",
        );
    }
    description.push_str("\n\nAccess: ");
    description.push_str(access_description(op.access));

    let mut tool = Tool::new(op.tool_name, description, Arc::new(schema));
    if !summary.is_empty() {
        tool.title = Some(summary.to_owned());
    }
    tool.annotations = Some(annotations(op));
    Ok(tool)
}

/// Аннотации по уровню доступа. `idempotentHint` имеет смысл только для изменяющих
/// инструментов: `PUT` и `DELETE` идемпотентны, `POST` и `PATCH` — нет (повторная
/// отправка создаёт ещё один черновик, ответ или возврат).
fn annotations(op: &OperationDef) -> ToolAnnotations {
    let idempotent = matches!(op.method, HttpMethod::Put | HttpMethod::Delete);
    access_annotations(op.summary.trim(), op.access, Some(idempotent))
}

/// Аннотации по уровню доступа; `idempotent` — для изменяющих инструментов, если известно.
fn access_annotations(
    title: &str,
    access: AccessLevel,
    idempotent: Option<bool>,
) -> ToolAnnotations {
    let base = if title.is_empty() {
        ToolAnnotations::new()
    } else {
        ToolAnnotations::with_title(title)
    }
    .open_world(true);
    let changing = |base: ToolAnnotations, destructive: bool| {
        let base = base.read_only(false).destructive(destructive);
        match idempotent {
            Some(idempotent) => base.idempotent(idempotent),
            None => base,
        }
    };
    match access {
        AccessLevel::Read => base.read_only(true),
        AccessLevel::Write => changing(base, false),
        AccessLevel::Destructive => changing(base, true),
    }
}

/// Строка уровня доступа в конце описания инструмента.
pub fn access_description(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only.",
        AccessLevel::Write => "write (creates or changes data).",
        AccessLevel::Destructive => {
            "destructive (deletes data, reaches users or affects money, or makes irreversible \
             changes)."
        }
    }
}

/// Добавляет обязательный аргумент `save_to` в схему операции с двоичным ответом.
fn add_save_to(op: &OperationDef, schema: &mut JsonObject) -> Result<(), String> {
    let properties = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(JsonObject::new()));
    let Value::Object(properties) = properties else {
        return Err(format!("{}: `properties` is not an object", op.tool_name));
    };
    if properties.contains_key(SAVE_TO_ARG) {
        return Err(format!(
            "{}: parameter `{SAVE_TO_ARG}` clashes with the file download argument",
            op.tool_name
        ));
    }
    properties.insert(
        SAVE_TO_ARG.to_owned(),
        json!({
            "type": "string",
            "description": "Path of a new local file to save the downloaded content to \
                (absolute, or relative to the server's working directory). The directory must \
                exist and be inside the directories allowed by the server; existing files are \
                never overwritten."
        }),
    );
    let required = schema
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(required) = required else {
        return Err(format!("{}: `required` is not an array", op.tool_name));
    };
    required.push(Value::String(SAVE_TO_ARG.to_owned()));
    Ok(())
}

pub(crate) fn access_verb(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "reads",
        AccessLevel::Write => "creates or changes",
        AccessLevel::Destructive => "deletes or irreversibly changes",
    }
}

/// Название уровня доступа для сообщений и инструкций.
pub fn access_name(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only",
        AccessLevel::Write => "write",
        AccessLevel::Destructive => "destructive",
    }
}

/// Флаг сервера, который включает уровень доступа `access`. Действует только в HTTP-режиме:
/// там уровень сервера задают флаги оператора; локальному серверу (stdio) права даёт профиль.
pub fn access_flag(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "no extra flag",
        AccessLevel::Write => "--allow-write",
        AccessLevel::Destructive => "--allow-destructive",
    }
}

/// Что пользователь делает, чтобы включить уровень `access`. Фраза продолжает «Ask the user
/// to …».
///
/// Локальный сервер (`local`, stdio): права — настройка профиля, флаги регистрации их не
/// расширяют, а на macOS повышение ещё и одобряется Touch ID; поэтому подсказка — про
/// `rustore-mcp ui`. HTTP-сервер: уровень задают флаги оператора при запуске.
pub fn access_hint(access: AccessLevel, local: bool) -> String {
    match (access, local) {
        (AccessLevel::Read, _) => "keep the read-only access".to_owned(),
        (level, true) => format!(
            "allow `{}` access for the server's profile with `rustore-mcp ui` and restart the \
             MCP client",
            access_name(level)
        ),
        (level, false) => format!("restart the server with {}", access_flag(level)),
    }
}

#[cfg(test)]
mod tests {
    use rustore_api::spec::toolsets;

    use super::*;
    use crate::composite::testing::fake_tool;
    use crate::toolsets::DEFAULT_TOOLSETS;

    /// Настройки без составных инструментов: счётчики ниже считают операции; составные
    /// проверяют отдельные тесты с поддельными инструментами.
    fn options(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
        ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            composite_tools: Vec::new(),
            ..ServerOptions::default()
        }
    }

    fn with_composites(options: ServerOptions, tools: Vec<CompositeTool>) -> ServerOptions {
        ServerOptions {
            composite_tools: tools,
            ..options
        }
    }

    fn fakes() -> Vec<CompositeTool> {
        vec![
            fake_tool("fake_read", "apps", AccessLevel::Read, false),
            fake_tool("fake_write", "releases", AccessLevel::Write, false),
            fake_tool("fake_files", "releases", AccessLevel::Write, true),
            fake_tool("fake_catalog", "catalog", AccessLevel::Read, false),
        ]
    }

    fn all() -> Option<&'static [&'static str]> {
        Some(&["all"])
    }

    /// Число инструментов: операции уровня и наборов плюс `api_request` и `api_search`.
    fn count(access: AccessLevel, toolsets: &[&str]) -> usize {
        operations()
            .iter()
            .filter(|op| access.allows(op.access) && toolsets.contains(&op.toolset))
            .count()
            + BUILT_IN
    }

    const BUILT_IN: usize = 2;

    fn all_slugs() -> Vec<&'static str> {
        toolsets().iter().map(|t| t.slug).collect()
    }

    #[test]
    fn schemas_match_operations() {
        assert!(!operations().is_empty());
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            assert_eq!(tool.name, op.tool_name);
            let expected: Value = serde_json::from_str(op.input_schema).unwrap();
            let mut actual = Value::Object((*tool.input_schema).clone());
            // Фильтр `jq` есть у всех операций с JSON-ответом и только у них.
            let jq = actual["properties"].as_object_mut().unwrap().remove(JQ_ARG);
            assert_eq!(jq.is_some(), !takes_save_to(op), "{}", op.tool_name);
            if takes_save_to(op) {
                // Единственное отличие от спецификации — аргумент `save_to`.
                let props = actual["properties"].as_object_mut().unwrap();
                assert!(props.remove(SAVE_TO_ARG).is_some(), "{}", op.tool_name);
                let required = actual["required"].as_array_mut().unwrap();
                assert_eq!(required.pop(), Some(json!(SAVE_TO_ARG)), "{}", op.tool_name);
                if required.is_empty() && expected.get("required").is_none() {
                    actual.as_object_mut().unwrap().remove("required");
                }
            }
            assert_eq!(actual, expected, "{}", op.tool_name);
            assert_eq!(actual["type"], "object", "{}", op.tool_name);
            assert_eq!(actual["additionalProperties"], false, "{}", op.tool_name);

            let description = tool.description.as_deref().unwrap();
            assert!(description.contains("Access: "), "{}", op.tool_name);
            assert!(description.contains(op.docs_url), "{}", op.tool_name);
            assert_eq!(
                tool.title.as_deref(),
                Some(op.summary.trim()),
                "{}",
                op.tool_name
            );
        }
    }

    #[test]
    fn annotations_follow_access_level() {
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            let a = tool.annotations.as_ref().unwrap();
            assert_eq!(
                a.title.as_deref(),
                Some(op.summary.trim()),
                "{}",
                op.tool_name
            );
            assert_eq!(a.open_world_hint, Some(true));
            match op.access {
                AccessLevel::Read => {
                    assert_eq!(a.read_only_hint, Some(true), "{}", op.tool_name);
                    assert_eq!(a.destructive_hint, None, "{}", op.tool_name);
                }
                AccessLevel::Write | AccessLevel::Destructive => {
                    assert_eq!(a.read_only_hint, Some(false), "{}", op.tool_name);
                    assert_eq!(
                        a.destructive_hint,
                        Some(op.access == AccessLevel::Destructive),
                        "{}",
                        op.tool_name
                    );
                    assert_eq!(
                        a.idempotent_hint,
                        Some(matches!(op.method, HttpMethod::Put | HttpMethod::Delete)),
                        "{}",
                        op.tool_name
                    );
                }
            }
        }
    }

    #[test]
    fn access_filtering() {
        let slugs = all_slugs();
        let read = ToolRegistry::new(&options(AccessLevel::Read, all())).unwrap();
        assert_eq!(read.len(), count(AccessLevel::Read, &slugs));
        assert!(read.iter().all(|t| t.access == AccessLevel::Read));
        assert!(read.get("get_app_rating").is_some());
        assert!(read.get("list_reviews").is_some());
        assert!(read.get("create_draft_version").is_none());
        assert!(read.get("delete_review_answer").is_none());

        let write = ToolRegistry::new(&options(AccessLevel::Write, all())).unwrap();
        assert_eq!(write.len(), count(AccessLevel::Write, &slugs));
        assert!(write.get("create_draft_version").is_some());
        // Удаление черновика — запись, удаление ответа на отзыв — удаление.
        assert!(write.get("delete_draft_version").is_some());
        assert!(write.get("delete_review_answer").is_none());

        let destructive = ToolRegistry::new(&options(AccessLevel::Destructive, all())).unwrap();
        assert_eq!(destructive.len(), operations().len() + BUILT_IN);
        let delete = destructive.get("delete_review_answer").unwrap();
        assert_eq!(delete.access, AccessLevel::Destructive);
        assert!(read.len() < write.len() && write.len() < destructive.len());
    }

    #[test]
    fn toolset_filtering() {
        let default = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        assert_eq!(default.enabled_toolsets(), DEFAULT_TOOLSETS);
        assert_eq!(
            default.len(),
            count(AccessLevel::Destructive, DEFAULT_TOOLSETS)
        );
        // Пустой список — тоже набор по умолчанию.
        let empty = ToolRegistry::new(&options(AccessLevel::Destructive, Some(&[]))).unwrap();
        assert_eq!(empty.len(), default.len());

        let chosen =
            ToolRegistry::new(&options(AccessLevel::Read, Some(&["catalog", " access "]))).unwrap();
        assert!(!chosen.is_empty());
        assert!(chosen.iter().all(|t| match t.kind {
            ToolKind::Operation(op) => op.toolset == "catalog" || op.toolset == "access",
            ToolKind::ApiRequest | ToolKind::ApiSearch => true,
            ToolKind::Composite(_)
            | ToolKind::Custom(_)
            | ToolKind::ListToolsets
            | ToolKind::EnableToolset
            | ToolKind::DisableToolset => false,
        }));
        assert!(chosen.get("list_reviews").is_none());
        // Встроенные инструменты не зависят от наборов.
        assert!(chosen.get("api_request").is_some());
        assert!(chosen.get("api_search").is_some());

        let err = ToolRegistry::new(&options(AccessLevel::Read, Some(&["Отзывы"]))).unwrap_err();
        assert!(err.contains("unknown toolset `Отзывы`"), "{err}");
    }

    #[test]
    fn built_in_tools_follow_server_access() {
        for (access, read_only, destructive) in [
            (AccessLevel::Read, Some(true), None),
            (AccessLevel::Write, Some(false), Some(false)),
            (AccessLevel::Destructive, Some(false), Some(true)),
        ] {
            let registry = ToolRegistry::new(&options(access, None)).unwrap();
            let request = registry.get("api_request").unwrap();
            assert!(matches!(request.kind, ToolKind::ApiRequest));
            assert_eq!(request.access, access);
            let a = request.tool.annotations.as_ref().unwrap();
            assert_eq!(
                (a.read_only_hint, a.destructive_hint),
                (read_only, destructive),
                "{access:?}"
            );
            let search = registry.get("api_search").unwrap();
            assert!(matches!(search.kind, ToolKind::ApiSearch));
            assert_eq!(search.access, AccessLevel::Read);
            assert_eq!(
                search.tool.annotations.as_ref().unwrap().read_only_hint,
                Some(true)
            );
        }
    }

    #[test]
    fn unavailable_reasons_name_the_access() {
        // Локальный сервер: права — у профиля, подсказка про `rustore-mcp ui`.
        let read = ToolRegistry::new(&options(AccessLevel::Read, None)).unwrap();
        let reason = read.unavailable_reason("create_draft_version").unwrap();
        assert!(reason.contains("`write` access"), "{reason}");
        assert!(reason.contains("rustore-mcp ui"), "{reason}");
        assert!(!reason.contains("--allow"), "{reason}");
        let reason = read.unavailable_reason("delete_review_answer").unwrap();
        assert!(reason.contains("`destructive` access"), "{reason}");
        assert!(read.unavailable_reason("get_app_rating").is_none());
        assert!(read.unavailable_reason("no_such_tool").is_none());

        let write = ToolRegistry::new(&options(AccessLevel::Write, None)).unwrap();
        let reason = write.unavailable_reason("delete_review_answer").unwrap();
        assert!(reason.contains("`destructive` access") && reason.contains("write access"));

        // HTTP-сервер: уровень задают флаги оператора.
        let remote = ToolRegistry::new(&ServerOptions {
            local_machine: false,
            ..options(AccessLevel::Read, None)
        })
        .unwrap();
        let reason = remote.unavailable_reason("create_draft_version").unwrap();
        assert!(
            reason.contains("restart the server with --allow-write"),
            "{reason}"
        );
        assert_eq!(
            access_hint(AccessLevel::Read, true),
            "keep the read-only access"
        );

        let catalog = ToolRegistry::new(&options(AccessLevel::Read, Some(&["catalog"]))).unwrap();
        let reason = catalog.unavailable_reason("get_app_rating").unwrap();
        assert!(reason.contains("toolset `reviews`"), "{reason}");
        assert!(reason.contains("--toolsets"), "{reason}");
    }

    #[test]
    fn remote_server_has_no_local_file_tools() {
        let remote = ToolRegistry::new(&ServerOptions {
            local_machine: false,
            ..options(AccessLevel::Destructive, all())
        })
        .unwrap();
        assert!(!remote.local_files());
        let file_tools: Vec<&str> = operations()
            .iter()
            .filter(|op| uses_local_files(op))
            .map(|op| op.tool_name)
            .collect();
        assert!(file_tools.contains(&"upload_apk"));
        assert!(file_tools.contains(&"export_reviews_csv"));
        for name in &file_tools {
            assert!(remote.get(name).is_none(), "{name}");
            let reason = remote.unavailable_reason(name).unwrap();
            assert!(reason.contains("HTTP mode"), "{reason}");
        }
        assert_eq!(
            remote.len(),
            operations().len() - file_tools.len() + BUILT_IN
        );
        for tool in remote.iter() {
            let description = tool.tool.description.as_deref().unwrap_or_default();
            assert!(!description.contains("local file"), "{}", tool.tool.name);
        }

        let local = ToolRegistry::new(&options(AccessLevel::Destructive, all())).unwrap();
        assert!(file_tools.iter().all(|name| local.get(name).is_some()));
    }

    #[test]
    fn server_composite_tools_register() {
        // Настоящие составные инструменты проходят проверки и регистрируются на всех уровнях.
        for access in [
            AccessLevel::Read,
            AccessLevel::Write,
            AccessLevel::Destructive,
        ] {
            let registry = ToolRegistry::new(&ServerOptions {
                max_access: access,
                toolsets: Some(vec!["all".into()]),
                ..ServerOptions::default()
            })
            .unwrap();
            for tool in crate::composite::all() {
                assert_eq!(
                    registry.get(tool.name).is_some(),
                    access.allows(tool.access),
                    "{}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn composite_tools_follow_access_toolsets_and_files() {
        let read =
            ToolRegistry::new(&with_composites(options(AccessLevel::Read, None), fakes())).unwrap();
        let tool = read.get("fake_read").unwrap();
        assert!(matches!(tool.kind, ToolKind::Composite(0)));
        assert_eq!(tool.access, AccessLevel::Read);
        let a = tool.tool.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(true));
        assert_eq!(tool.tool.title.as_deref(), Some("Fake composite tool"));
        let description = tool.tool.description.as_deref().unwrap();
        assert!(description.ends_with("Access: read-only."), "{description}");
        assert!(tool.tool.input_schema["properties"].get(JQ_ARG).is_some());
        assert_eq!(read.len(), count(AccessLevel::Read, DEFAULT_TOOLSETS) + 1);

        // Уровень выше разрешённого и чужой набор — не регистрируются, причина с уровнем.
        assert!(read.get("fake_write").is_none());
        let reason = read.unavailable_reason("fake_write").unwrap();
        assert!(reason.contains("`write` access"), "{reason}");
        assert!(read.get("fake_catalog").is_none());
        let reason = read.unavailable_reason("fake_catalog").unwrap();
        assert!(reason.contains("toolset `catalog`"), "{reason}");

        let write = ToolRegistry::new(&with_composites(options(AccessLevel::Write, None), fakes()))
            .unwrap();
        let a = write
            .get("fake_write")
            .unwrap()
            .tool
            .annotations
            .clone()
            .unwrap();
        assert_eq!(
            (a.read_only_hint, a.destructive_hint),
            (Some(false), Some(false))
        );
        let files = write.get("fake_files").unwrap();
        let description = files.tool.description.as_deref().unwrap();
        assert!(description.contains("File arguments"), "{description}");

        // В HTTP-режиме инструменты с локальными файлами не регистрируются.
        let remote = ToolRegistry::new(&ServerOptions {
            local_machine: false,
            ..with_composites(options(AccessLevel::Write, None), fakes())
        })
        .unwrap();
        assert!(remote.get("fake_files").is_none());
        assert!(remote.get("fake_write").is_some());
        let reason = remote.unavailable_reason("fake_files").unwrap();
        assert!(reason.contains("HTTP mode"), "{reason}");

        // Набор включается и для составных инструментов.
        let catalog = read.with_toolsets(&["catalog".into()]).unwrap();
        assert!(catalog.get("fake_catalog").is_some());
        assert!(catalog.get("fake_read").is_none());
    }

    #[test]
    fn composite_names_are_checked() {
        let clash = |name: &'static str| {
            ToolRegistry::new(&with_composites(
                options(AccessLevel::Read, None),
                vec![fake_tool(name, "apps", AccessLevel::Read, false)],
            ))
            .unwrap_err()
        };
        assert!(clash("api_request").contains("built-in"));
        assert!(clash("enable_toolset").contains("built-in"));
        assert!(clash("list_reviews").contains("operation tool"));
        assert!(clash("").contains("characters long"));
        let long: &'static str = "x".repeat(MAX_TOOL_NAME_LEN + 1).leak();
        assert!(clash(long).contains("characters long"));

        let twice = ToolRegistry::new(&with_composites(
            options(AccessLevel::Read, None),
            vec![
                fake_tool("fake_one", "apps", AccessLevel::Read, false),
                fake_tool("fake_one", "reviews", AccessLevel::Read, false),
            ],
        ))
        .unwrap_err();
        assert!(twice.contains("defined twice"), "{twice}");

        let unknown = ToolRegistry::new(&with_composites(
            options(AccessLevel::Read, None),
            vec![fake_tool("fake_one", "payments", AccessLevel::Read, false)],
        ))
        .unwrap_err();
        assert!(unknown.contains("unknown toolset `payments`"), "{unknown}");
    }

    #[test]
    fn composite_schema_is_checked() {
        let mut tool = fake_tool("fake_jq", "apps", AccessLevel::Read, false);
        tool.input_schema = || json!({"type": "object", "properties": {"jq": {"type": "string"}}});
        assert!(composite_tool(&tool).unwrap_err().contains("clashes"));
        tool.input_schema = || json!({"type": "array"});
        assert!(composite_tool(&tool).unwrap_err().contains("type: object"));
        tool.input_schema = || json!([]);
        assert!(
            composite_tool(&tool)
                .unwrap_err()
                .contains("not a JSON object")
        );
    }
}
