//! Составные инструменты группы: права ключа (набор `apps`).
//!
//! - `auth_check` — какие разделы RuStore API и какие приложения доступны ключу сервера
//!   (логика — [`crate::auth_check`], её же вызывает `rustore-mcp auth check`).
//!
//! Как добавить инструмент — в описании модуля [`super`].

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Value, json};

use super::common::object_schema;
use super::{CompositeTool, Context, ToolFuture};
use crate::auth_check::check_access;
use crate::error::ToolError;

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[CompositeTool {
    name: "auth_check",
    toolset: "apps",
    access: AccessLevel::Read,
    title: "Check the permissions of the key",
    description: "Checks what the server's RuStore API key may do: sends one read-only request \
        per API section (apps, releases, listing, reviews, purchases, subscriptions, catalog, \
        access) and lists the apps of the company with access to each. Permissions of a key \
        are ticked in the RuStore console per method and per app, and methods or apps added \
        later are not added to an existing key, so use this tool when a call fails with HTTP \
        401 or 403, or before a task that needs several sections. Returns `sections` (status \
        `available`, `denied`, `not_checked` or `error`, the probed tool `method` and its \
        console name `method_title`, the app it was checked on and details) and `apps` \
        (`packageName`, `appName`, `appId`, status `accessible`, `denied`, `unknown` or \
        `not_checked`). Response bodies are not returned. For a denied section ask the user \
        to tick its method for the key in the RuStore console (Company > RuStore API); for a \
        denied app, to give the key access to the app or create a new key.",
    input_schema: schema,
    uses_local_files: false,
    run,
}];

fn schema() -> Value {
    object_schema(json!({}), &[])
}

fn run<'a>(ctx: &'a Context, _args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let report = check_access(ctx).await?;
        serde_json::to_value(&report)
            .map_err(|err| ToolError::new(format!("internal error: cannot encode the report: {err}")))
    })
}
