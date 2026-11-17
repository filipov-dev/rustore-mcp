//! Составные инструменты группы: доступы пользователей (набор `access`).
//!
//! API доступов работает по одному приложению: `list_app_access` — пользователи и роли
//! приложения, `grant_app_access` и `revoke_app_access` — роль одного пользователя в одном
//! приложении. Инструменты обходят приложения кабинета:
//!
//! - `access_summary` — пользователи и роли по всем приложениям ключа (или по списку
//!   `packageNames`), сгруппированные по пользователю; ошибка по приложению (чаще всего 403:
//!   ключ видит не все приложения кабинета) уходит в `warnings` и не теряет остальные;
//! - `access_apply` — выдача, смена и отзыв роли одного пользователя по списку приложений,
//!   с `dry_run`. Перед изменением читается текущий доступ приложения: план считается по нему,
//!   а `dry_run` не делает запросов записи. Роли владельца не выдаются и не отзываются, а
//!   доступ владельца приложения не меняется.
//!
//! Имена и VK ID пользователей уходят агенту (инструменты для этого), но не в логи: в
//! сообщениях ошибок их нет, а пути запросов пишутся в лог через
//! [`rustore_api::path::path_for_log`].
//!
//! Как добавить инструмент — в описании модуля [`super`].

use std::collections::BTreeMap;

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::common::{
    bool_arg, invalid_argument, missing_argument, object_schema, optional_str, string_list,
    text_field, validate_package_name,
};
use super::{Collect, CompositeTool, Context, ToolFuture};
use crate::error::ToolError;

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[
    CompositeTool {
        name: "access_summary",
        toolset: "access",
        access: AccessLevel::Read,
        title: "Users and roles across apps",
        description: "Answers who has which access in the RuStore developer account: reads \
            `list_app_access` (all pages) for every app of the key, or for the apps in \
            `packageNames`, and groups the result by user: `users` lists each user (devVkId, \
            firstName, lastName), the distinct `roles` and `access` (packageName and role per \
            app); `by_app` gives the number of users per app. An app that fails (for example \
            HTTP 403, when the key has no access to that app) goes to `warnings` and the other \
            apps are still read. `devVkId` and `role` narrow the result. The response holds \
            names of people: do not copy them into logs, commits or wiki pages.",
        input_schema: summary_schema,
        uses_local_files: false,
        run: access_summary,
    },
    CompositeTool {
        name: "access_apply",
        toolset: "access",
        access: AccessLevel::Destructive,
        title: "Grant, change or revoke a role in several apps",
        description: "Grants, changes or revokes the role of one user (`devVkId`, the VK ID \
            from `access_summary` or `list_app_access`) in every app of `packageNames`. First \
            it reads the current access of each app (`list_app_access`) and plans per app: \
            `action: grant` with `role` gives the role to a user without access (`grant`), \
            replaces another role (`change`) or does nothing when the user already has it \
            (`none`); `action: revoke` removes `role`, or without `role` every role of the \
            user in the app (`revoke`). Then it calls `grant_app_access` or \
            `revoke_app_access`. Owner roles (OWNER, NON_RESIDENT_OWNER, INDIVIDUAL_OWNER) \
            are rejected before any request, and apps where the user is an owner are skipped. \
            Run with `dry_run: true` first and show the plan to the user: it reads the access \
            but sends no changes. An app that fails is reported in its entry and does not stop \
            the others. Changing access affects real people: confirm with the user.",
        input_schema: apply_schema,
        uses_local_files: false,
        run: access_apply,
    },
];

/// Роли владельца: их нельзя выдать через API, а отзыв лишил бы приложение владельца.
pub(super) const OWNER_ROLES: &[&str] = &["OWNER", "NON_RESIDENT_OWNER", "INDIVIDUAL_OWNER"];
/// Роли, которые выдаёт `grant_app_access`.
pub(super) const GRANTABLE_ROLES: &[&str] = &[
    "ADMIN",
    "RELEASE_MANAGER",
    "DEV",
    "FINANCIAL_MANAGER",
    "SUPPORT",
];

/// Сколько приложений обходит инструмент за один вызов: по запросу (и больше) на каждое.
pub(super) const MAX_APPS: usize = 200;
/// Сколько приложений ключа просматривается в `list_apps`.
const APPS_SCAN_LIMIT: usize = 5000;
/// Размер страницы `list_app_access` (максимум по справочнику).
const ACCESS_PAGE_SIZE: u64 = 100;
/// Предел пользователей одного приложения.
const ACCESS_MAX_ITEMS: usize = 5000;

fn summary_schema() -> Value {
    object_schema(
        json!({
            "packageNames": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Package names of the apps to read, like \
                    [\"com.example.app\"]. Omit to read all apps of the key (`list_apps`, at \
                    most 200 apps per call)."
            },
            "devVkId": {
                "type": "string",
                "description": "Only this user (the VK ID, `devVkId`)."
            },
            "role": {
                "type": "string",
                "enum": all_roles(),
                "description": "Only entries with this role."
            }
        }),
        &[],
    )
}

fn apply_schema() -> Value {
    object_schema(
        json!({
            "devVkId": {
                "type": "string",
                "description": "VK ID of the user (`devVkId` of `access_summary` or \
                    `list_app_access`), digits only."
            },
            "action": {
                "type": "string",
                "enum": ["grant", "revoke"],
                "description": "`grant` gives `role` (or replaces the user's other role), \
                    `revoke` removes `role` or, without `role`, every role of the user."
            },
            "role": {
                "type": "string",
                "enum": GRANTABLE_ROLES,
                "description": "Role to grant (required for `grant`) or to revoke. Owner \
                    roles are not accepted."
            },
            "packageNames": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1,
                "maxItems": MAX_APPS,
                "description": "Package names of the apps to change, like \
                    [\"com.example.app\", \"com.example.other\"]."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Read the current access and return the plan without changing \
                    anything (default false)."
            }
        }),
        &["devVkId", "action", "packageNames"],
    )
}

fn all_roles() -> Vec<&'static str> {
    OWNER_ROLES
        .iter()
        .chain(GRANTABLE_ROLES)
        .copied()
        .collect()
}

/// HTTP-статус из текста ошибки шага (`RuStore API returned HTTP 403 ...`).
pub(super) fn http_status(err: &ToolError) -> Option<u16> {
    let (_, rest) = err.message().split_once("returned HTTP ")?;
    rest.get(..3)?.parse().ok()
}

/// Приложение для обхода: имя пакета, а из `list_apps` — ещё `appId` и название.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AppRef {
    pub package: String,
    pub app_id: Option<u64>,
    pub name: Option<String>,
}

/// Выбранные приложения и предупреждения выбора.
#[derive(Debug, Default)]
pub(super) struct Selection {
    pub apps: Vec<AppRef>,
    pub warnings: Vec<Value>,
    pub notes: Vec<String>,
}

/// Имена пакетов из аргумента `packageNames`, проверенные и без повторов.
pub(super) fn package_list(args: &JsonObject) -> Result<Vec<String>, ToolError> {
    let mut packages: Vec<String> = Vec::new();
    for package in string_list(args, "packageNames")? {
        let package = validate_package_name(&package)?.to_owned();
        if !packages.contains(&package) {
            packages.push(package);
        }
    }
    if packages.len() > MAX_APPS {
        return Err(ToolError::new(format!(
            "`packageNames` has {} apps; pass at most {MAX_APPS} per call",
            packages.len()
        )));
    }
    Ok(packages)
}

/// Приложения для обхода. Без `packages` — все приложения ключа из `list_apps` (не больше
/// [`MAX_APPS`]). С `packages` и `resolve: false` запросов нет; с `resolve: true` `appId`
/// находится в `list_apps`, а пакет, которого там нет, уходит в `warnings`.
pub(super) async fn select_apps(
    ctx: &Context,
    packages: &[String],
    resolve: bool,
) -> Result<Selection, ToolError> {
    let mut selection = Selection::default();
    if !packages.is_empty() && !resolve {
        selection.apps = packages
            .iter()
            .map(|package| AppRef {
                package: package.clone(),
                app_id: None,
                name: None,
            })
            .collect();
        return Ok(selection);
    }
    let listed = ctx
        .collect(
            "list_apps",
            json!({}),
            Collect {
                page_size: 100,
                max_items: APPS_SCAN_LIMIT,
            },
        )
        .await?;
    let apps: Vec<AppRef> = listed
        .items
        .iter()
        .filter_map(|app| {
            Some(AppRef {
                package: text_field(app, "packageName")?,
                app_id: text_field(app, "appId").and_then(|id| id.parse().ok()),
                name: text_field(app, "appName"),
            })
        })
        .collect();
    if !listed.complete {
        selection.notes.push(format!(
            "only the first {} apps of the key were listed",
            listed.items.len()
        ));
    }
    if packages.is_empty() {
        if apps.len() > MAX_APPS {
            selection.notes.push(format!(
                "the key has {} apps; only the first {MAX_APPS} were read, pass `packageNames` \
                 for the others",
                apps.len()
            ));
        }
        selection.apps = apps.into_iter().take(MAX_APPS).collect();
        return Ok(selection);
    }
    for package in packages {
        match apps.iter().find(|app| &app.package == package) {
            Some(app) => selection.apps.push(app.clone()),
            None => selection.warnings.push(json!({
                "packageName": package,
                "error": "the app is not among the apps of the key (`list_apps`)"
            })),
        }
    }
    Ok(selection)
}

/// Все пользователи и роли приложения (`list_app_access`, все страницы).
async fn read_access(ctx: &Context, package: &str) -> Result<(Vec<Value>, bool), ToolError> {
    let collected = ctx
        .collect(
            "list_app_access",
            json!({"packageName": package}),
            Collect {
                page_size: ACCESS_PAGE_SIZE,
                max_items: ACCESS_MAX_ITEMS,
            },
        )
        .await?;
    Ok((collected.items, collected.complete))
}

/// VK ID пользователя: строка или число из цифр. Значение в ошибку не попадает: ошибки
/// инструмента пишутся в лог.
fn vk_id(value: Option<&Value>, name: &str) -> Result<Option<String>, ToolError> {
    let text = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) if n.is_u64() => n.to_string(),
        Some(_) => return Err(invalid_argument(name, "a VK ID of digits")),
    };
    if text.is_empty() {
        return Ok(None);
    }
    if text.len() > 20 || !text.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid_argument(name, "a VK ID of digits"));
    }
    Ok(Some(text))
}

/// Роль из аргумента заглавными; не роль RuStore — ошибка.
fn role_arg(args: &JsonObject, name: &str) -> Result<Option<String>, ToolError> {
    let Some(role) = optional_str(args, name)? else {
        return Ok(None);
    };
    let role = role.to_uppercase().replace('-', "_");
    if all_roles().contains(&role.as_str()) {
        Ok(Some(role))
    } else {
        Err(ToolError::new(format!(
            "unknown role `{role}`; roles: {}",
            all_roles().join(", ")
        )))
    }
}

/// Роли пользователя `user` в записях `list_app_access`, без повторов.
fn roles_of(entries: &[Value], user: &str) -> Vec<String> {
    let mut roles: Vec<String> = Vec::new();
    for entry in entries {
        if text_field(entry, "devVkId").as_deref() == Some(user)
            && let Some(role) = text_field(entry, "role")
            && !roles.contains(&role)
        {
            roles.push(role);
        }
    }
    roles
}

fn is_owner_role(role: &str) -> bool {
    OWNER_ROLES.contains(&role)
}

/// Пользователь в сводке: имя и доступы по приложениям.
#[derive(Default)]
struct UserAccess {
    first_name: Option<String>,
    last_name: Option<String>,
    access: Vec<(String, String)>,
}

fn access_summary<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let packages = package_list(&args)?;
        let only_user = vk_id(args.get("devVkId"), "devVkId")?;
        let only_role = role_arg(&args, "role")?;
        let selection = select_apps(ctx, &packages, false).await?;
        let mut warnings = selection.warnings;
        let mut notes = selection.notes;

        let mut users: BTreeMap<String, UserAccess> = BTreeMap::new();
        let mut by_app = Vec::new();
        let mut read = 0usize;
        for app in &selection.apps {
            let (entries, complete) = match read_access(ctx, &app.package).await {
                Ok(found) => found,
                Err(err) => {
                    warnings.push(json!({"packageName": app.package, "error": err.message()}));
                    continue;
                }
            };
            read += 1;
            if !complete {
                warnings.push(json!({
                    "packageName": app.package,
                    "error": format!("only the first {} users were read", entries.len())
                }));
            }
            for entry in &entries {
                let Some(id) = text_field(entry, "devVkId") else {
                    continue;
                };
                let role = text_field(entry, "role").unwrap_or_default();
                if only_user.as_deref().is_some_and(|wanted| wanted != id)
                    || only_role.as_deref().is_some_and(|wanted| wanted != role)
                {
                    continue;
                }
                let user = users.entry(id.clone()).or_default();
                user.first_name = user
                    .first_name
                    .take()
                    .or_else(|| text_field(entry, "firstName"));
                user.last_name = user
                    .last_name
                    .take()
                    .or_else(|| text_field(entry, "lastName"));
                let pair = (app.package.clone(), role);
                if !user.access.contains(&pair) {
                    user.access.push(pair);
                }
            }
            let mut record = Map::new();
            record.insert("packageName".into(), json!(app.package));
            if let Some(name) = &app.name {
                record.insert("appName".into(), json!(name));
            }
            record.insert("users".into(), json!(count_users(&entries, &only_user, &only_role)));
            by_app.push(Value::Object(record));
        }

        let mut list: Vec<(String, UserAccess)> = users.into_iter().collect();
        // По фамилии и имени, при равенстве — по VK ID (порядок BTreeMap).
        list.sort_by(|(_, a), (_, b)| {
            (a.last_name.as_deref(), a.first_name.as_deref())
                .cmp(&(b.last_name.as_deref(), b.first_name.as_deref()))
        });
        let users: Vec<Value> = list
            .into_iter()
            .map(|(id, user)| {
                let mut roles: Vec<&str> = user.access.iter().map(|(_, r)| r.as_str()).collect();
                roles.sort_unstable();
                roles.dedup();
                let mut record = Map::new();
                record.insert("devVkId".into(), json!(id));
                if let Some(first) = &user.first_name {
                    record.insert("firstName".into(), json!(first));
                }
                if let Some(last) = &user.last_name {
                    record.insert("lastName".into(), json!(last));
                }
                record.insert("roles".into(), json!(roles));
                record.insert("apps".into(), json!(user.access.len()));
                record.insert(
                    "access".into(),
                    Value::Array(
                        user.access
                            .iter()
                            .map(|(package, role)| json!({"packageName": package, "role": role}))
                            .collect(),
                    ),
                );
                Value::Object(record)
            })
            .collect();

        let mut result = Map::new();
        result.insert("apps".into(), json!(selection.apps.len()));
        result.insert("apps_read".into(), json!(read));
        result.insert("users_total".into(), json!(users.len()));
        result.insert("users".into(), Value::Array(users));
        result.insert("by_app".into(), Value::Array(by_app));
        if !warnings.is_empty() {
            notes.push(format!(
                "{} app(s) could not be read completely, see `warnings`",
                warnings.len()
            ));
            result.insert("warnings".into(), Value::Array(warnings));
        }
        if !notes.is_empty() {
            result.insert("note".into(), json!(notes.join("; ")));
        }
        Ok(Value::Object(result))
    })
}

/// Число разных пользователей приложения после фильтров.
fn count_users(entries: &[Value], only_user: &Option<String>, only_role: &Option<String>) -> usize {
    let mut ids: Vec<String> = entries
        .iter()
        .filter(|entry| {
            only_role
                .as_deref()
                .is_none_or(|role| text_field(entry, "role").as_deref() == Some(role))
        })
        .filter_map(|entry| text_field(entry, "devVkId"))
        .filter(|id| only_user.as_deref().is_none_or(|wanted| wanted == id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

/// Действие `access_apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Grant,
    Revoke,
}

fn access_apply<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let user = vk_id(args.get("devVkId"), "devVkId")?.ok_or_else(|| missing_argument("devVkId"))?;
        let action = match optional_str(&args, "action")?.map(str::to_lowercase).as_deref() {
            Some("grant") => Action::Grant,
            Some("revoke") => Action::Revoke,
            Some(_) => return Err(invalid_argument("action", "`grant` or `revoke`")),
            None => return Err(missing_argument("action")),
        };
        let role = role_arg(&args, "role")?;
        // Роли владельца отклоняются до любых запросов.
        if let Some(role) = role.as_deref().filter(|role| is_owner_role(role)) {
            return Err(ToolError::new(format!(
                "the owner role `{role}` cannot be granted or revoked with this tool; owner \
                 access is changed only in the RuStore console"
            )));
        }
        if action == Action::Grant && role.is_none() {
            return Err(ToolError::new(format!(
                "`action: grant` needs `role`: one of {}",
                GRANTABLE_ROLES.join(", ")
            )));
        }
        let packages = package_list(&args)?;
        if packages.is_empty() {
            return Err(missing_argument("packageNames"));
        }
        let dry_run = bool_arg(&args, "dry_run", false)?;

        let mut entries = Vec::new();
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for package in &packages {
            let entry = apply_to_app(ctx, package, &user, action, role.as_deref(), dry_run).await;
            let outcome = outcome_of(&entry);
            *counts.entry(outcome).or_default() += 1;
            entries.push(entry);
        }

        let mut result = Map::new();
        result.insert("devVkId".into(), json!(user));
        result.insert(
            "action".into(),
            json!(match action {
                Action::Grant => "grant",
                Action::Revoke => "revoke",
            }),
        );
        if let Some(role) = &role {
            result.insert("role".into(), json!(role));
        }
        result.insert("dry_run".into(), json!(dry_run));
        result.insert("summary".into(), json!(counts));
        result.insert("apps".into(), Value::Array(entries));
        let pending = counts.get("planned").copied().unwrap_or(0);
        if dry_run && pending > 0 {
            result.insert(
                "note".into(),
                json!(format!(
                    "dry run: {pending} app(s) would change; show the plan to the user and call \
                     again without `dry_run` to apply it"
                )),
            );
        } else if counts.get("failed").copied().unwrap_or(0) > 0 {
            result.insert(
                "note".into(),
                json!("some apps failed, see `error` in their entries; the other apps were \
                       processed"),
            );
        }
        Ok(Value::Object(result))
    })
}

/// Итог записи приложения для `summary`: `planned`, `applied`, `unchanged`, `skipped`,
/// `failed`.
fn outcome_of(entry: &Value) -> &'static str {
    if entry.get("error").is_some() {
        "failed"
    } else {
        match entry["change"].as_str() {
            Some("none") => "unchanged",
            Some("skip") => "skipped",
            _ if entry["applied"] == true => "applied",
            _ => "planned",
        }
    }
}

/// План и, без `dry_run`, изменение доступа пользователя в одном приложении.
async fn apply_to_app(
    ctx: &Context,
    package: &str,
    user: &str,
    action: Action,
    role: Option<&str>,
    dry_run: bool,
) -> Value {
    let mut entry = Map::new();
    entry.insert("packageName".into(), json!(package));
    let current = match read_access(ctx, package).await {
        Ok((entries, _)) => entries,
        Err(err) => {
            entry.insert("change".into(), json!("unknown"));
            entry.insert("applied".into(), json!(false));
            entry.insert("error".into(), json!(err.message()));
            return Value::Object(entry);
        }
    };
    let before = roles_of(&current, user);
    entry.insert("roles_before".into(), json!(before));
    if before.iter().any(|role| is_owner_role(role)) {
        entry.insert("change".into(), json!("skip"));
        entry.insert("applied".into(), json!(false));
        entry.insert(
            "reason".into(),
            json!("the user is an owner of the app; owner access is not changed"),
        );
        return Value::Object(entry);
    }

    // Что меняем: роль для PUT или роли для DELETE.
    let (change, grant, revoke): (&str, Option<&str>, Vec<String>) = match action {
        Action::Grant => {
            let role = role.unwrap_or_default();
            if before.len() == 1 && before[0] == role {
                ("none", None, Vec::new())
            } else if before.is_empty() {
                ("grant", Some(role), Vec::new())
            } else {
                ("change", Some(role), Vec::new())
            }
        }
        Action::Revoke => {
            let targets: Vec<String> = match role {
                Some(role) => before.iter().filter(|r| *r == role).cloned().collect(),
                None => before.clone(),
            };
            if targets.is_empty() {
                ("none", None, Vec::new())
            } else {
                ("revoke", None, targets)
            }
        }
    };
    let expected: Vec<String> = match (grant, change) {
        (Some(role), _) => vec![role.to_owned()],
        (None, "revoke") => before
            .iter()
            .filter(|r| !revoke.contains(r))
            .cloned()
            .collect(),
        _ => before.clone(),
    };
    entry.insert("change".into(), json!(change));
    entry.insert("roles_after".into(), json!(expected));
    if change == "none" {
        entry.insert("applied".into(), json!(false));
        entry.insert(
            "reason".into(),
            json!(match action {
                Action::Grant => "the user already has this role",
                Action::Revoke => "the user has no such role in the app",
            }),
        );
        return Value::Object(entry);
    }
    if dry_run {
        entry.insert("applied".into(), json!(false));
        return Value::Object(entry);
    }

    if let Some(role) = grant {
        let args = json!({"packageName": package, "devVkId": user, "roleName": role});
        match ctx.call("grant_app_access", args).await {
            Ok(body) => {
                // Роли после выдачи — из ответа, если он их перечисляет.
                let content = body
                    .get("content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let after = roles_of(&content, user);
                if !after.is_empty() {
                    entry.insert("roles_after".into(), json!(after));
                }
                entry.insert("applied".into(), json!(true));
            }
            Err(err) => {
                entry.insert("applied".into(), json!(false));
                entry.insert("error".into(), json!(err.message()));
            }
        }
        return Value::Object(entry);
    }

    let mut revoked: Vec<String> = Vec::new();
    for role in &revoke {
        let args = json!({"packageName": package, "devVkId": user, "roleName": role});
        if let Err(err) = ctx.call("revoke_app_access", args).await {
            let left: Vec<String> = before
                .iter()
                .filter(|r| !revoked.contains(r))
                .cloned()
                .collect();
            entry.insert("roles_after".into(), json!(left));
            entry.insert("applied".into(), json!(!revoked.is_empty()));
            entry.insert("error".into(), json!(err.message()));
            return Value::Object(entry);
        }
        revoked.push(role.clone());
    }
    entry.insert("applied".into(), json!(true));
    Value::Object(entry)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::testing;
    use crate::files::FilePolicy;

    const APP_A: &str = "com.example.alpha";
    const APP_B: &str = "com.example.beta";
    const APP_C: &str = "com.example.gamma";

    fn envelope(body: Value) -> Value {
        json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
    }

    fn ok(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(envelope(body))
    }

    fn ctx(server: &MockServer, access: AccessLevel) -> Context {
        testing::context(&server.uri(), access, &[], FilePolicy::disabled()).unwrap()
    }

    async fn run(name: &str, ctx: &Context, args: Value) -> Result<Value, ToolError> {
        let tool = TOOLS.iter().find(|tool| tool.name == name).unwrap();
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        super::super::check_arguments(&(tool.input_schema)(), &args)?;
        (tool.run)(ctx, args).await
    }

    fn user(id: &str, role: &str, first: &str, last: &str) -> Value {
        json!({"devVkId": id, "role": role, "firstName": first, "lastName": last})
    }

    fn access_path(package: &str) -> String {
        format!("/public/v1/application/{package}/developer")
    }

    async fn mount_apps(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .respond_with(ok(json!({"content": [
                {"packageName": APP_A, "appName": "Alpha", "appId": 1},
                {"packageName": APP_B, "appName": "Beta", "appId": 2},
                {"packageName": APP_C, "appName": "Gamma", "appId": 3},
            ]})))
            .mount(server)
            .await;
    }

    /// Доступы: у Alpha две страницы, Beta — 403 (чужое приложение), у Gamma одна.
    async fn mount_access(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(access_path(APP_A)))
            .and(query_param("pageSize", "100"))
            .and(query_param_is_missing("pageToken"))
            .respond_with(ok(json!({
                "content": [user("100", "OWNER", "Ivan", "Petrov"), user("200", "DEV", "Anna", "Smirnova")],
                "pageToken": "page-2"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(access_path(APP_A)))
            .and(query_param("pageToken", "page-2"))
            .respond_with(ok(json!({"content": [user("300", "SUPPORT", "Oleg", "Abramov")]})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(access_path(APP_B)))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({"code": "FORBIDDEN", "message": "Access denied", "body": null}),
            ))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(access_path(APP_C)))
            .respond_with(ok(json!({"content": [
                user("200", "ADMIN", "Anna", "Smirnova"),
                user("100", "OWNER", "Ivan", "Petrov"),
            ]})))
            .mount(server)
            .await;
    }

    #[test]
    fn tools_have_expected_levels() {
        let level = |name: &str| TOOLS.iter().find(|tool| tool.name == name).unwrap().access;
        assert_eq!(level("access_summary"), AccessLevel::Read);
        assert_eq!(level("access_apply"), AccessLevel::Destructive);
        assert!(TOOLS.iter().all(|tool| tool.toolset == "access"));
        assert!(TOOLS.iter().all(|tool| !tool.uses_local_files));
        // Выдача и отзыв — уровень удаления: инструмент не ниже своих операций.
        for op in ["grant_app_access", "revoke_app_access"] {
            assert_eq!(
                super::super::operation(op).unwrap().access,
                AccessLevel::Destructive
            );
        }
    }

    #[test]
    fn http_status_is_parsed_from_step_errors() {
        let err = ToolError::new("`x` failed: RuStore API returned HTTP 403 (access denied).");
        assert_eq!(http_status(&err), Some(403));
        assert_eq!(http_status(&ToolError::new("no status")), None);
    }

    #[tokio::test]
    async fn summary_groups_users_across_apps_and_keeps_going_after_403() {
        let server = MockServer::start().await;
        mount_apps(&server).await;
        mount_access(&server).await;
        let ctx = ctx(&server, AccessLevel::Read);

        let summary = run("access_summary", &ctx, json!({})).await.unwrap();
        assert_eq!(summary["apps"], 3);
        assert_eq!(summary["apps_read"], 2);
        assert_eq!(summary["users_total"], 3);
        // По фамилии: Abramov, Petrov, Smirnova.
        let users = summary["users"].as_array().unwrap();
        let ids: Vec<&str> = users.iter().map(|u| u["devVkId"].as_str().unwrap()).collect();
        assert_eq!(ids, ["300", "100", "200"]);
        let anna = &users[2];
        assert_eq!(anna["firstName"], "Anna");
        assert_eq!(anna["roles"], json!(["ADMIN", "DEV"]));
        assert_eq!(anna["apps"], 2);
        assert_eq!(
            anna["access"],
            json!([
                {"packageName": APP_A, "role": "DEV"},
                {"packageName": APP_C, "role": "ADMIN"}
            ])
        );
        assert_eq!(users[1]["roles"], json!(["OWNER"]));
        assert_eq!(
            summary["by_app"],
            json!([
                {"packageName": APP_A, "appName": "Alpha", "users": 3},
                {"packageName": APP_C, "appName": "Gamma", "users": 2}
            ])
        );
        let warnings = summary["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["packageName"], APP_B);
        assert!(
            warnings[0]["error"].as_str().unwrap().contains("HTTP 403"),
            "{warnings:?}"
        );
        assert!(summary["note"].as_str().unwrap().contains("warnings"));
    }

    #[tokio::test]
    async fn summary_reads_given_packages_with_filters() {
        let server = MockServer::start().await;
        mount_access(&server).await;
        // `list_apps` с `packageNames` не нужен.
        Mock::given(method("GET"))
            .and(path("/public/v1/application"))
            .respond_with(ok(json!({"content": []})))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = ctx(&server, AccessLevel::Read);

        let summary = run(
            "access_summary",
            &ctx,
            json!({"packageNames": [APP_A, APP_C, APP_B, APP_A], "devVkId": "200"}),
        )
        .await
        .unwrap();
        assert_eq!(summary["apps"], 3);
        assert_eq!(summary["users_total"], 1);
        assert_eq!(summary["users"][0]["devVkId"], "200");
        assert_eq!(summary["by_app"][0], json!({"packageName": APP_A, "users": 1}));

        let err = run("access_summary", &ctx, json!({"packageNames": ["bad name"]}))
            .await
            .unwrap_err();
        assert!(err.message().contains("packageName"), "{err}");
        let err = run("access_summary", &ctx, json!({"devVkId": "abc"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("`devVkId`"), "{err}");
        assert!(!err.message().contains("abc"), "{err}");
    }

    /// Изменяющие запросы в тесте `dry_run` не должны уходить.
    async fn forbid_writes(server: &MockServer) {
        for verb in ["PUT", "DELETE", "POST"] {
            Mock::given(method(verb))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(server)
                .await;
        }
    }

    #[tokio::test]
    async fn apply_dry_run_plans_without_writes() {
        let server = MockServer::start().await;
        mount_access(&server).await;
        forbid_writes(&server).await;
        let ctx = ctx(&server, AccessLevel::Destructive);

        let plan = run(
            "access_apply",
            &ctx,
            json!({"devVkId": "200", "action": "grant", "role": "release_manager",
                   "packageNames": [APP_A, APP_B, APP_C], "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(plan["dry_run"], true);
        assert_eq!(plan["role"], "RELEASE_MANAGER");
        let apps = plan["apps"].as_array().unwrap();
        assert_eq!(apps[0]["change"], "change");
        assert_eq!(apps[0]["roles_before"], json!(["DEV"]));
        assert_eq!(apps[0]["roles_after"], json!(["RELEASE_MANAGER"]));
        assert_eq!(apps[0]["applied"], false);
        assert!(apps[1]["error"].as_str().unwrap().contains("HTTP 403"));
        assert_eq!(apps[2]["change"], "change");
        assert_eq!(plan["summary"], json!({"failed": 1, "planned": 2}));
        assert!(plan["note"].as_str().unwrap().contains("dry run"));
    }

    #[tokio::test]
    async fn apply_grants_revokes_and_skips_owners() {
        let server = MockServer::start().await;
        mount_access(&server).await;
        Mock::given(method("PUT"))
            .and(path(format!("{}/400/role/DEV", access_path(APP_A))))
            .respond_with(ok(json!({"content": [user("400", "DEV", "New", "User")]})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("{}/400/role/DEV", access_path(APP_C))))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                json!({"code": "ERROR", "message": "Bad request", "body": null}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = ctx(&server, AccessLevel::Destructive);

        let done = run(
            "access_apply",
            &ctx,
            json!({"devVkId": 400, "action": "grant", "role": "DEV",
                   "packageNames": [APP_A, APP_C]}),
        )
        .await
        .unwrap();
        let apps = done["apps"].as_array().unwrap();
        assert_eq!(apps[0]["change"], "grant");
        assert_eq!(apps[0]["applied"], true);
        assert_eq!(apps[0]["roles_after"], json!(["DEV"]));
        // Ошибка одного приложения не останавливает остальные и видна в записи.
        assert_eq!(apps[1]["applied"], false);
        assert!(apps[1]["error"].as_str().unwrap().contains("HTTP 400"));
        assert_eq!(done["summary"], json!({"applied": 1, "failed": 1}));

        // Отзыв: у владельца ничего не меняется, у SUPPORT — DELETE.
        let server = MockServer::start().await;
        mount_access(&server).await;
        Mock::given(method("DELETE"))
            .and(path(format!("{}/300/role/SUPPORT", access_path(APP_A))))
            .respond_with(ok(Value::Null))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = testing::context(
            &server.uri(),
            AccessLevel::Destructive,
            &[],
            FilePolicy::disabled(),
        )
        .unwrap();
        let done = run(
            "access_apply",
            &ctx,
            json!({"devVkId": "300", "action": "revoke", "packageNames": [APP_A, APP_C]}),
        )
        .await
        .unwrap();
        let apps = done["apps"].as_array().unwrap();
        assert_eq!(apps[0]["change"], "revoke");
        assert_eq!(apps[0]["applied"], true);
        assert_eq!(apps[0]["roles_after"], json!([]));
        assert_eq!(apps[1]["change"], "none");
        assert_eq!(done["summary"], json!({"applied": 1, "unchanged": 1}));

        let server = MockServer::start().await;
        mount_access(&server).await;
        forbid_writes(&server).await;
        let ctx = testing::context(
            &server.uri(),
            AccessLevel::Destructive,
            &[],
            FilePolicy::disabled(),
        )
        .unwrap();
        let done = run(
            "access_apply",
            &ctx,
            json!({"devVkId": "100", "action": "revoke", "packageNames": [APP_A]}),
        )
        .await
        .unwrap();
        assert_eq!(done["apps"][0]["change"], "skip");
        assert!(done["apps"][0]["reason"].as_str().unwrap().contains("owner"));
        assert_eq!(done["summary"], json!({"skipped": 1}));
    }

    #[tokio::test]
    async fn apply_rejects_owner_roles_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = ctx(&server, AccessLevel::Destructive);
        for (action, role) in [
            ("grant", "OWNER"),
            ("revoke", "owner"),
            ("grant", "NON_RESIDENT_OWNER"),
            ("revoke", "INDIVIDUAL_OWNER"),
        ] {
            let err = run(
                "access_apply",
                &ctx,
                json!({"devVkId": "1", "action": action, "role": role, "packageNames": [APP_A]}),
            )
            .await
            .unwrap_err();
            assert!(err.message().contains("owner role"), "{err}");
        }
        for (args, expected) in [
            (json!({"devVkId": "1", "action": "grant", "packageNames": [APP_A]}), "needs `role`"),
            (
                json!({"devVkId": "1", "action": "grant", "role": "BOSS", "packageNames": [APP_A]}),
                "unknown role",
            ),
            (
                json!({"devVkId": "1", "action": "move", "role": "DEV", "packageNames": [APP_A]}),
                "`action`",
            ),
            (
                json!({"devVkId": "1/2", "action": "grant", "role": "DEV", "packageNames": [APP_A]}),
                "`devVkId`",
            ),
            (
                json!({"devVkId": "1", "action": "grant", "role": "DEV", "packageNames": []}),
                "`packageNames`",
            ),
            (json!({"action": "grant", "role": "DEV", "packageNames": [APP_A]}), "`devVkId`"),
        ] {
            let err = run("access_apply", &ctx, args.clone()).await.unwrap_err();
            assert!(err.message().contains(expected), "{args}: {err}");
        }
    }
}
