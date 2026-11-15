//! Имена MCP-инструментов.

/// Предел длины имени инструмента. Клиенты ограничивают `mcp__<server>__<tool>` 64 символами,
/// имя сервера — до 17 (`rustore-<profile>`), на инструмент остаётся 40.
pub const MAX_TOOL_NAME: usize = 40;

/// Имя инструмента: `operationId` в snake_case (`listApps` → `list_apps`, `list_apps` без изменений).
///
/// Переименований нет: имена задаёт скрипт сборки спецификации, генератор их не меняет.
pub fn tool_name(operation_id: &str) -> String {
    to_snake_case(operation_id)
}

/// camelCase → snake_case с учётом аббревиатур: `getVersion` → `get_version`,
/// `getURLInfo` → `get_url_info`, `getSubscriptionV5` → `get_subscription_v5`.
pub fn to_snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(chars.len() + 8);
    for (index, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            let prev = chars[index - 1];
            let next_is_lower = chars.get(index + 1).is_some_and(|c| c.is_ascii_lowercase());
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_is_lower)
            {
                out.push('_');
            }
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Имя инструмента: `^[a-z0-9_]{1,40}$` и начинается с буквы.
pub fn is_valid_tool_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= MAX_TOOL_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
