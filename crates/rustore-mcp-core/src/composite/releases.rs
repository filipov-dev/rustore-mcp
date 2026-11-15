//! Составные инструменты группы: версии и публикация (набор `releases`).
//!
//! - `release_summary` (чтение) — последние версии одного приложения по состояниям:
//!   опубликована, частичная публикация с процентом, одобрена, на модерации, черновик,
//!   отклонена с комментарием модерации (`statusComment` из `get_version`).
//! - `upload_build` (запись, локальные файлы) — черновик версии (новый или существующий),
//!   поля черновика и загрузка одного AAB или нескольких APK. На модерацию не отправляет.
//! - `release_submit` (удаление) — настройки публикации черновика и отправка на модерацию;
//!   при автоматической публикации это выпуск версии пользователям.
//! - `release_rollout` (удаление) — ручная публикация одобренной версии, рост процента
//!   частичной публикации или текущее состояние.
//!
//! У `release_submit` и `release_rollout` есть `dry_run`: чтения выполняются, изменяющие
//! вызовы только перечисляются. Сами инструменты регистрируются на уровне удаления, так что
//! и пробный запуск доступен только серверу с `--allow-destructive`.
//!
//! Как добавить инструмент — в описании модуля [`super`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::common::{
    bool_arg, bounded_u64, invalid_argument, object_schema, optional_str, optional_u64,
    package_name, package_name_property, pick, text_field,
};
use super::{Collect, CompositeTool, Context, ToolFuture, operation};
use crate::error::ToolError;

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[
    CompositeTool {
        name: "release_summary",
        toolset: "releases",
        access: AccessLevel::Read,
        title: "Summary of the app versions by status",
        description: "Summarizes the latest versions of one app by status, from `list_versions` \
            (all pages up to `scan`) and `get_version`: `active` (published), `partial` (published \
            to a share of users, with `partialValue`, the rollout percent), \
            `ready_for_publication` (approved, waiting for manual or delayed publication), \
            `in_moderation` (AUTO_CHECK, MODERATION, TAKEN_FOR_MODERATION), `draft`, and \
            `rejected` (AUTO_CHECK_FAILED, REJECTED_BY_MODERATOR, REJECTED_BY_SECURITY, with the \
            moderation comment `statusComment`). Older versions (PREVIOUS_ACTIVE, ARCHIVED, \
            deleted drafts) are only counted in `status_counts`. Use it to answer what is live, \
            what is in review and why a version was rejected, before creating or publishing a \
            version. Versions are ordered newest first (by `versionId`).",
        input_schema: release_summary_schema,
        uses_local_files: false,
        run: release_summary,
    },
    CompositeTool {
        name: "upload_build",
        toolset: "releases",
        access: AccessLevel::Write,
        title: "Upload a build to a draft version",
        description: "Uploads a new build of an app to a RuStore draft version in one call. It \
            uses the draft `versionId` when given, otherwise the existing draft of the app (an \
            app has at most one), and creates a new draft only when there is none. It can set \
            draft fields first (`whatsNew`, `moderInfo`, other fields of \
            `update_draft_version` in `draft_fields`), then uploads one AAB (`aab`) and/or APK \
            files (`apks`: the main APK and extra APKs, for example with Huawei Mobile Services, \
            `servicesType: HMS`). Limits are checked before anything is sent: `.aab` and `.apk` \
            extensions, at most 5 GB per file, one AAB plus up to 8 APKs or up to 10 APKs. \
            Returns the `versionId`, whether the draft was created or reused, the uploaded files \
            (with the permissions and SDKs RuStore found) and the draft state. Nothing is sent \
            for moderation and users see nothing: submit the draft with `release_submit` after \
            the user confirms. Uploading a new file of the same kind replaces the previous one \
            in the draft. Files must be inside the server's allowed directories.",
        input_schema: upload_build_schema,
        uses_local_files: true,
        run: upload_build,
    },
    CompositeTool {
        name: "release_submit",
        toolset: "releases",
        access: AccessLevel::Destructive,
        title: "Submit a draft version for moderation",
        description: "Submits the draft version of an app for RuStore moderation \
            (`submit_draft_for_moderation`), optionally setting its publication first \
            (`publishType`: INSTANTLY publishes to real users automatically as soon as \
            moderation passes, MANUAL waits for `release_rollout` with action `publish`, \
            DELAYED publishes at `publishDateTime`; `partialValue` publishes to that percent of \
            users) and the update priority (`priorityUpdate`). By default it first checks with \
            `get_version` that the version is a draft with an uploaded build. After the submit \
            the draft can no longer be edited. This is irreversible and can release the app to \
            users: ask the user before a real run, and run with `dry_run: true` first to see \
            the planned calls without changing anything.",
        input_schema: release_submit_schema,
        uses_local_files: false,
        run: release_submit,
    },
    CompositeTool {
        name: "release_rollout",
        toolset: "releases",
        access: AccessLevel::Destructive,
        title: "Publish a version or raise its rollout percent",
        description: "Manages the publication of an approved or partially published RuStore \
            version: `status` reports its state (status, publication type, rollout percent) \
            without changes; `publish` publishes a READY_FOR_PUBLICATION version with manual \
            publication to real users now (`publish_version`), optionally to `partialValue` \
            percent of users first; `set_percent` raises the rollout percent \
            (`update_publish_settings`; 5, 10, 25, 50, 75 or 100, where 100 publishes to all \
            users). RuStore cannot lower the rollout percent, so a smaller value is refused. \
            Without `versionId` it picks the version in PARTIAL_ACTIVE or \
            READY_FOR_PUBLICATION. `publish` and `set_percent` change what real users get: ask \
            the user before a real run, and run with `dry_run: true` first to see the planned \
            calls.",
        input_schema: release_rollout_schema,
        uses_local_files: false,
        run: release_rollout,
    },
];

/// Поля версии в ответе `release_summary` (из `list_versions`, `statusComment` — из
/// `get_version`).
const VERSION_FIELDS: &[&str] = &[
    "versionId",
    "versionName",
    "versionCode",
    "versionStatus",
    "filterTestingType",
    "publishType",
    "publishDateTime",
    "sendDateForModer",
    "partialValue",
    "whatsNew",
];

/// Поля версии из `get_version` в ответах `upload_build`, `release_submit` и
/// `release_rollout`. Ссылок на файлы и иконки (`fileUrl`, `iconUrl`) среди них нет.
const STATE_FIELDS: &[&str] = &[
    "versionId",
    "versionName",
    "versionStatus",
    "statusComment",
    "testingType",
    "publishType",
    "publishDateTime",
    "partialValue",
    "whatsNew",
];

/// Поля сборки (`packages` в `get_version`); `apkUid` агенту не нужен.
const PACKAGE_FIELDS: &[&str] = &[
    "fileName",
    "verName",
    "verCode",
    "size",
    "servicesType",
    "isMainApk",
    "deviceTypes",
];

/// Максимальный размер страницы `list_versions` по справочнику RuStore.
const VERSIONS_PAGE_SIZE: u64 = 100;

/// Допустимые проценты частичной публикации.
const PARTIAL_VALUES: [u64; 6] = [5, 10, 25, 50, 75, 100];
const PUBLISH_TYPES: [&str; 3] = ["INSTANTLY", "MANUAL", "DELAYED"];

/// Ограничения загрузки по справочнику (`upload_apk`, `upload_aab`): файл до 5 ГБ, один AAB
/// и до 8 APK или до 10 APK в версии.
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_APKS: usize = 10;
const MAX_APKS_WITH_AAB: usize = 8;

/// Пределы длины полей черновика (символы) по справочнику `create_draft_version`.
const WHATS_NEW_MAX: usize = 5000;
const MODER_INFO_MAX: usize = 180;

// ---------------------------------------------------------------------------------------
// release_summary

fn release_summary_schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "include_alpha": {
                "type": "boolean",
                "description": "Include alpha (testing) versions: `filterTestingType: ALL` \
                    instead of the default release versions only."
            },
            "per_status": {
                "type": "integer",
                "minimum": 1,
                "maximum": 20,
                "description": "Maximum number of versions listed in each group (default 3, \
                    newest first)."
            },
            "scan": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1000,
                "description": "Maximum number of versions read from `list_versions` (default \
                    200)."
            }
        }),
        &[],
    )
}

/// Группа сводки по статусу версии; `None` — старые версии, только в счётчиках.
fn status_group(status: &str) -> Option<&'static str> {
    match status {
        "ACTIVE" => Some("active"),
        "PARTIAL_ACTIVE" => Some("partial"),
        "READY_FOR_PUBLICATION" => Some("ready_for_publication"),
        "AUTO_CHECK" | "MODERATION" | "TAKEN_FOR_MODERATION" => Some("in_moderation"),
        "DRAFT" => Some("draft"),
        "AUTO_CHECK_FAILED" | "REJECTED_BY_MODERATOR" | "REJECTED_BY_SECURITY" => Some("rejected"),
        _ => None,
    }
}

/// Группы сводки в порядке ответа.
const GROUPS: &[&str] = &[
    "active",
    "partial",
    "ready_for_publication",
    "in_moderation",
    "draft",
    "rejected",
];

fn release_summary<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let include_alpha = bool_arg(&args, "include_alpha", false)?;
        let per_status = bounded_u64(&args, "per_status", 3, 1, 20)? as usize;
        let scan = bounded_u64(&args, "scan", 200, 1, 1000)? as usize;
        let mut list_args = json!({"packageName": package});
        if include_alpha {
            list_args["filterTestingType"] = json!("ALL");
        }
        let collected = ctx
            .collect(
                "list_versions",
                list_args,
                Collect {
                    page_size: VERSIONS_PAGE_SIZE.min(scan as u64),
                    max_items: scan,
                },
            )
            .await?;
        let mut versions = collected.items;
        // Новые версии — с большим `versionId`; порядок выдачи API не задокументирован.
        versions.sort_by(|a, b| version_order(b).total_cmp(&version_order(a)));

        let mut status_counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut groups: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        let mut group_totals: BTreeMap<&str, usize> = BTreeMap::new();
        for version in &versions {
            let status = text_field(version, "versionStatus").unwrap_or_default();
            *status_counts.entry(status.clone()).or_default() += 1;
            let Some(group) = status_group(&status) else {
                continue;
            };
            *group_totals.entry(group).or_default() += 1;
            let entries = groups.entry(group).or_default();
            if entries.len() < per_status {
                entries.push(pick(version, VERSION_FIELDS));
            }
        }

        // Комментарий модерации есть только в `get_version`; процент частичной публикации —
        // тоже там, если список его не вернул.
        let mut warnings = Vec::new();
        for group in ["rejected", "partial"] {
            let Some(entries) = groups.get_mut(group) else {
                continue;
            };
            for entry in entries.iter_mut() {
                if group == "partial" && entry.get("partialValue").is_some() {
                    continue;
                }
                let Some(id) = entry.get("versionId").cloned() else {
                    continue;
                };
                match ctx
                    .call(
                        "get_version",
                        json!({"packageName": package, "versionId": id}),
                    )
                    .await
                {
                    Ok(details) => {
                        for field in ["statusComment", "partialValue"] {
                            if let Some(value) = details.get(field).filter(|v| !v.is_null())
                                && let Value::Object(map) = entry
                            {
                                map.entry(field.to_owned()).or_insert(value.clone());
                            }
                        }
                    }
                    Err(err) => warnings.push(format!("version {id}: {}", err.message())),
                }
            }
        }

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert(
            "versions".into(),
            json!(if include_alpha { "ALL" } else { "RELEASE" }),
        );
        result.insert("scanned".into(), json!(versions.len()));
        result.insert("complete".into(), json!(collected.complete));
        for group in GROUPS {
            let entries = groups.remove(group).unwrap_or_default();
            let total = group_totals.get(group).copied().unwrap_or(0);
            if total > entries.len() {
                result.insert(format!("{group}_total"), json!(total));
            }
            result.insert((*group).to_owned(), Value::Array(entries));
        }
        result.insert("status_counts".into(), json!(status_counts));
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        let mut notes = Vec::new();
        if !collected.complete {
            notes.push(format!(
                "only the first {} versions were read; raise `scan` to see older ones",
                versions.len()
            ));
        }
        if !include_alpha {
            notes.push("alpha versions are not included; pass `include_alpha: true`".to_owned());
        }
        if !notes.is_empty() {
            result.insert("note".into(), json!(notes.join("; ")));
        }
        Ok(Value::Object(result))
    })
}

/// `versionId` для сортировки (число или строка с числом); нет — в конец.
fn version_order(version: &Value) -> f64 {
    text_field(version, "versionId")
        .and_then(|id| id.parse().ok())
        .unwrap_or(f64::MIN)
}

// ---------------------------------------------------------------------------------------
// Общее для upload_build, release_submit и release_rollout

/// Целый неотрицательный идентификатор или процент из JSON (число, в том числе `25.0`, или
/// строка с числом).
fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= u64::MAX as f64)
                .map(|f| f as u64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn version_id_of(version: &Value) -> Option<u64> {
    version.get("versionId").and_then(as_u64)
}

/// Статус версии из ответа `get_version` или `list_versions`.
fn status_of(version: &Value) -> Option<String> {
    text_field(version, "versionStatus").map(|s| s.to_uppercase())
}

/// Компактное состояние версии из `get_version`: поля [`STATE_FIELDS`] и сборки.
fn version_state(details: &Value) -> Value {
    let mut state = pick(details, STATE_FIELDS);
    if let (Value::Object(map), Some(packages)) =
        (&mut state, details.get("packages").and_then(Value::as_array))
    {
        let packages: Vec<Value> = packages.iter().map(|p| pick(p, PACKAGE_FIELDS)).collect();
        map.insert("packages".into(), Value::Array(packages));
    }
    state
}

/// Черновики приложения, новые первыми. У приложения один черновик, но фильтр по статусу
/// перепроверяется: справочник не обещает, что `versionStatuses` строгий.
async fn find_drafts(ctx: &Context, package: &str) -> Result<Vec<Value>, ToolError> {
    let collected = ctx
        .collect(
            "list_versions",
            json!({
                "packageName": package,
                "versionStatuses": "DRAFT",
                "filterTestingType": "ALL"
            }),
            Collect {
                page_size: 20,
                max_items: 20,
            },
        )
        .await?;
    let mut drafts: Vec<Value> = collected
        .items
        .into_iter()
        .filter(|v| status_of(v).as_deref() == Some("DRAFT") && version_id_of(v).is_some())
        .collect();
    drafts.sort_by(|a, b| version_order(b).total_cmp(&version_order(a)));
    Ok(drafts)
}

/// Версии в состояниях `statuses` (через запятую), новые первыми.
async fn find_versions(
    ctx: &Context,
    package: &str,
    statuses: &[&str],
) -> Result<Vec<Value>, ToolError> {
    let collected = ctx
        .collect(
            "list_versions",
            json!({
                "packageName": package,
                "versionStatuses": statuses.join(","),
                "filterTestingType": "ALL"
            }),
            Collect {
                page_size: 20,
                max_items: 100,
            },
        )
        .await?;
    let mut versions: Vec<Value> = collected
        .items
        .into_iter()
        .filter(|v| {
            status_of(v).is_some_and(|s| statuses.contains(&s.as_str()))
                && version_id_of(v).is_some()
        })
        .collect();
    versions.sort_by(|a, b| version_order(b).total_cmp(&version_order(a)));
    Ok(versions)
}

async fn get_version(ctx: &Context, package: &str, version_id: u64) -> Result<Value, ToolError> {
    ctx.call(
        "get_version",
        json!({"packageName": package, "versionId": version_id}),
    )
    .await
}

fn publish_type_arg(args: &JsonObject) -> Result<Option<String>, ToolError> {
    let Some(value) = optional_str(args, "publishType")? else {
        return Ok(None);
    };
    let value = value.to_uppercase();
    if PUBLISH_TYPES.contains(&value.as_str()) {
        Ok(Some(value))
    } else {
        Err(invalid_argument("publishType", "INSTANTLY, MANUAL or DELAYED"))
    }
}

fn partial_value_arg(args: &JsonObject) -> Result<Option<u64>, ToolError> {
    let Some(value) = optional_u64(args, "partialValue")? else {
        return Ok(None);
    };
    if PARTIAL_VALUES.contains(&value) {
        Ok(Some(value))
    } else {
        Err(invalid_argument("partialValue", "5, 10, 25, 50, 75 or 100"))
    }
}

/// Дата отложенной публикации в формате справочника `yyyy-MM-dd'T'HH:mm:ssXXX` (дробные
/// секунды допускаются, как в примерах справочника). Окно «от 24 часов до 60 дней» проверяет
/// RuStore.
fn publish_date_time_arg(args: &JsonObject) -> Result<Option<String>, ToolError> {
    let Some(value) = optional_str(args, "publishDateTime")? else {
        return Ok(None);
    };
    let pattern = regex::Regex::new(
        r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,9})?(Z|[+-]\d{2}:\d{2})$",
    )
    .map_err(|err| ToolError::new(format!("internal error: {err}")))?;
    if pattern.is_match(value) {
        Ok(Some(value.to_owned()))
    } else {
        Err(invalid_argument(
            "publishDateTime",
            "a date and time with an offset, like 2026-07-08T13:24:41+03:00",
        ))
    }
}

/// Вызов операции в плане: имя операции и её аргументы, как у инструмента операции.
fn planned(operation_id: &str, arguments: Value) -> Value {
    json!({"operation": operation_id, "arguments": arguments})
}

/// Выполняет план по порядку. Ошибка на шаге сообщает, какие шаги уже выполнены.
async fn execute_plan(ctx: &Context, plan: &[Value]) -> Result<Vec<Value>, ToolError> {
    let mut done: Vec<String> = Vec::new();
    for step in plan {
        let op = step["operation"].as_str().unwrap_or_default();
        if let Err(err) = ctx.call(op, step["arguments"].clone()).await {
            let before = if done.is_empty() {
                "no earlier step was made".to_owned()
            } else {
                format!("already done: {}", done.join(", "))
            };
            return Err(ToolError::new(format!("{} ({before})", err.message())));
        }
        done.push(format!("`{op}`"));
    }
    Ok(plan.to_vec())
}

// ---------------------------------------------------------------------------------------
// upload_build

fn upload_build_schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "versionId": {
                "type": "integer",
                "description": "Draft version to upload to. Omit to use the existing draft of \
                    the app or create a new one when there is none."
            },
            "aab": {
                "type": "string",
                "description": "Path to one Android App Bundle (`.aab`) to upload. The AAB \
                    signature for publishing must be uploaded in RuStore Console first."
            },
            "apks": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_APKS,
                "description": "APK files to upload (up to 10, or up to 8 together with \
                    `aab`): a path, or an object with `file`, `isMainApk` and \
                    `servicesType`. Without `aab` and without an explicit `isMainApk`, the \
                    first APK that is not HMS becomes the main APK, served to all users by \
                    default.",
                "items": {
                    "anyOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "properties": {
                                "file": {"type": "string", "description": "Path to the `.apk` file."},
                                "isMainApk": {
                                    "type": "boolean",
                                    "description": "The main APK, served to all users by \
                                        default (default false, see `apks`)."
                                },
                                "servicesType": {
                                    "type": "string",
                                    "enum": ["HMS", "Unknown"],
                                    "description": "`HMS` for an APK with Huawei Mobile \
                                        Services; default `Unknown`."
                                }
                            },
                            "required": ["file"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "whatsNew": {
                "type": "string",
                "maxLength": WHATS_NEW_MAX,
                "description": "Release notes («What's new») of the draft, up to 5000 characters."
            },
            "moderInfo": {
                "type": "string",
                "maxLength": MODER_INFO_MAX,
                "description": "Comment for the moderator, up to 180 characters."
            },
            "draft_fields": {
                "type": "object",
                "description": "Other draft fields of `update_draft_version` (its `body`), like \
                    `appName`, `shortDescription`, `fullDescription`, `publishType`. Set \
                    before the files are uploaded."
            }
        }),
        &[],
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildKind {
    Aab,
    Apk,
}

impl BuildKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Aab => "aab",
            Self::Apk => "apk",
        }
    }
}

/// Файл для загрузки после проверки.
#[derive(Debug, Clone)]
struct BuildFile {
    kind: BuildKind,
    /// Путь, как его передал агент (его же проверит [`crate::files::FilePolicy`] запроса).
    path: String,
    bytes: u64,
    main: bool,
    services_type: Option<String>,
}

/// Описание APK из аргумента `apks` до проверки файла.
struct ApkArg {
    path: String,
    main: Option<bool>,
    services_type: Option<String>,
}

fn parse_apks(args: &JsonObject) -> Result<Vec<ApkArg>, ToolError> {
    let items = match args.get("apks") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid_argument("apks", "an array of APK paths or objects")),
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let at = |what: &str| ToolError::new(format!("`apks[{index}]`: {what}"));
            match item {
                Value::String(path) if !path.trim().is_empty() => Ok(ApkArg {
                    path: path.trim().to_owned(),
                    main: None,
                    services_type: None,
                }),
                Value::Object(map) => {
                    if let Some(key) = map
                        .keys()
                        .find(|key| !["file", "isMainApk", "servicesType"].contains(&key.as_str()))
                    {
                        return Err(at(&format!(
                            "unknown field `{key}`; allowed: `file`, `isMainApk`, `servicesType`"
                        )));
                    }
                    let path = map
                        .get("file")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|path| !path.is_empty())
                        .ok_or_else(|| at("`file` must be a path to an .apk file"))?;
                    let main = match map.get("isMainApk") {
                        None | Some(Value::Null) => None,
                        Some(Value::Bool(b)) => Some(*b),
                        Some(_) => return Err(at("`isMainApk` must be true or false")),
                    };
                    let services_type = match map.get("servicesType") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(s)) if s.eq_ignore_ascii_case("hms") => {
                            Some("HMS".to_owned())
                        }
                        Some(Value::String(s)) if s.eq_ignore_ascii_case("unknown") => {
                            Some("Unknown".to_owned())
                        }
                        Some(_) => return Err(at("`servicesType` must be HMS or Unknown")),
                    };
                    Ok(ApkArg {
                        path: path.to_owned(),
                        main,
                        services_type,
                    })
                }
                _ => Err(at("expected a path or an object with `file`")),
            }
        })
        .collect()
}

fn has_extension(path: &str, extension: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

/// Проверяет файлы до любого запроса: расширения, количество, разрешённые каталоги и
/// размер. Так черновик не создаётся ради загрузки, которая заведомо не пройдёт.
fn check_build_files(ctx: &Context, args: &JsonObject) -> Result<Vec<BuildFile>, ToolError> {
    let aab = optional_str(args, "aab")?;
    let apks = parse_apks(args)?;
    if aab.is_none() && apks.is_empty() {
        return Err(ToolError::new(
            "pass `aab` (one .aab file) and/or `apks` (up to 10 .apk files)",
        ));
    }
    let limit = if aab.is_some() {
        MAX_APKS_WITH_AAB
    } else {
        MAX_APKS
    };
    if apks.len() > limit {
        return Err(ToolError::new(format!(
            "too many APK files: {} given; RuStore accepts one AAB and up to {MAX_APKS_WITH_AAB} \
             APKs, or up to {MAX_APKS} APKs, in a version",
            apks.len()
        )));
    }
    if let Some(path) = aab
        && !has_extension(path, "aab")
    {
        return Err(ToolError::new(format!(
            "`aab`: `{path}` must be a file with the .aab extension"
        )));
    }
    for apk in &apks {
        if !has_extension(&apk.path, "apk") {
            return Err(ToolError::new(format!(
                "`apks`: `{}` must be a file with the .apk extension",
                apk.path
            )));
        }
    }
    // Основной APK по умолчанию — первый не HMS, если основной не указан явно и AAB нет.
    let explicit_main = apks.iter().any(|apk| apk.main == Some(true));
    let default_main = if aab.is_none() && !explicit_main {
        apks.iter()
            .position(|apk| apk.main.is_none() && apk.services_type.as_deref() != Some("HMS"))
    } else {
        None
    };

    let mut files = Vec::new();
    if let Some(path) = aab {
        files.push((BuildKind::Aab, path.to_owned(), false, None));
    }
    for (index, apk) in apks.into_iter().enumerate() {
        let main = apk.main.unwrap_or(default_main == Some(index));
        files.push((BuildKind::Apk, apk.path, main, apk.services_type));
    }
    let mut seen = BTreeSet::new();
    let mut checked = Vec::new();
    for (kind, path, main, services_type) in files {
        let canonical = ctx
            .files()
            .check_read(&path)
            .map_err(|err| ToolError::new(format!("`{}`: {err}", kind.as_str())))?;
        if !seen.insert(canonical.clone()) {
            return Err(ToolError::new(format!(
                "`{path}` is given more than once"
            )));
        }
        let bytes = std::fs::metadata(&canonical)
            .map_err(|err| ToolError::new(format!("`{path}`: {err}")))?
            .len();
        if bytes == 0 {
            return Err(ToolError::new(format!("`{path}` is empty")));
        }
        if bytes > MAX_FILE_BYTES {
            return Err(ToolError::new(format!(
                "`{path}` is {bytes} bytes; RuStore accepts files up to 5 GB"
            )));
        }
        checked.push(BuildFile {
            kind,
            path,
            bytes,
            main,
            services_type,
        });
    }
    Ok(checked)
}

/// Поля черновика: `whatsNew`, `moderInfo` и `draft_fields`, проверенные по телу
/// `update_draft_version` из спецификации.
fn draft_fields(args: &JsonObject) -> Result<Map<String, Value>, ToolError> {
    let mut fields = match args.get("draft_fields") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err(invalid_argument("draft_fields", "a JSON object")),
    };
    let known = draft_body_fields()?;
    if let Some(key) = fields.keys().find(|key| !known.contains(key.as_str())) {
        return Err(ToolError::new(format!(
            "`draft_fields`: unknown field `{key}`; `update_draft_version` accepts: {}",
            known.iter().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    for (name, max) in [("whatsNew", WHATS_NEW_MAX), ("moderInfo", MODER_INFO_MAX)] {
        let Some(value) = optional_str(args, name)? else {
            continue;
        };
        if value.chars().count() > max {
            return Err(ToolError::new(format!(
                "`{name}` is longer than {max} characters"
            )));
        }
        if fields.contains_key(name) {
            return Err(ToolError::new(format!(
                "`{name}` is given both as an argument and in `draft_fields`"
            )));
        }
        fields.insert(name.to_owned(), json!(value));
    }
    Ok(fields)
}

/// Имена полей тела `update_draft_version` из схемы операции.
fn draft_body_fields() -> Result<BTreeSet<String>, ToolError> {
    let op = operation("update_draft_version")?;
    let schema: Value = serde_json::from_str(op.input_schema)
        .map_err(|err| ToolError::new(format!("internal error: {err}")))?;
    Ok(schema["properties"]["body"]["properties"]
        .as_object()
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default())
}

/// Как получен черновик.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DraftSource {
    Given,
    Existing,
    Created,
}

impl DraftSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Given => "given",
            Self::Existing => "reused",
            Self::Created => "created",
        }
    }
}

fn upload_build<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let given = optional_u64(&args, "versionId")?;
        let fields = draft_fields(&args)?;
        let files = check_build_files(ctx, &args)?;
        let mut notes: Vec<String> = Vec::new();

        // Черновик. Сначала ищем существующий: по справочнику новый черновик через API
        // удаляет прежний (или create возвращает ошибку — справочник говорит и то и другое),
        // а чужую работу в черновике терять нельзя.
        let mut fields_applied = false;
        let (version_id, source) = match given {
            Some(id) => {
                let details = get_version(ctx, &package, id).await?;
                if let Some(status) = status_of(&details)
                    && status != "DRAFT"
                {
                    return Err(ToolError::new(format!(
                        "version {id} is {status}, not a draft: builds are uploaded only to a \
                         DRAFT version. Omit `versionId` to use or create the draft"
                    )));
                }
                (id, DraftSource::Given)
            }
            None => match find_drafts(ctx, &package).await?.first().and_then(version_id_of) {
                Some(id) => (id, DraftSource::Existing),
                None => {
                    let mut create = json!({"packageName": package});
                    if !fields.is_empty() {
                        create["body"] = Value::Object(fields.clone());
                    }
                    match ctx.call("create_draft_version", create).await {
                        Ok(body) => {
                            let id = as_u64(&body)
                                .or_else(|| version_id_of(&body))
                                .ok_or_else(|| {
                                    ToolError::new(format!(
                                        "`create_draft_version` returned no version id: {body}"
                                    ))
                                })?;
                            fields_applied = true;
                            (id, DraftSource::Created)
                        }
                        // Черновик мог появиться между запросами (или список его не показал):
                        // тогда работаем с ним.
                        Err(err) => match find_drafts(ctx, &package)
                            .await
                            .ok()
                            .and_then(|drafts| drafts.first().and_then(version_id_of))
                        {
                            Some(id) => {
                                notes.push(format!(
                                    "`create_draft_version` failed ({}); the existing draft \
                                     {id} is used",
                                    err.message()
                                ));
                                (id, DraftSource::Existing)
                            }
                            None => return Err(err),
                        },
                    }
                }
            },
        };
        let draft_note = format!(
            "draft version {version_id} ({})",
            source.as_str()
        );

        let mut updated_fields: Vec<String> = fields.keys().cloned().collect();
        if !fields.is_empty() && !fields_applied {
            ctx.call(
                "update_draft_version",
                json!({
                    "packageName": package,
                    "versionId": version_id,
                    "body": Value::Object(fields.clone())
                }),
            )
            .await
            .map_err(|err| {
                ToolError::new(format!(
                    "{}; {draft_note}; no file was uploaded",
                    err.message()
                ))
            })?;
        }
        updated_fields.sort();

        let mut uploaded: Vec<Value> = Vec::new();
        for file in &files {
            let (op, call_args) = match file.kind {
                BuildKind::Aab => (
                    "upload_aab",
                    json!({"packageName": package, "versionId": version_id, "file": file.path}),
                ),
                BuildKind::Apk => {
                    let mut call_args = json!({
                        "packageName": package,
                        "versionId": version_id,
                        "file": file.path,
                        "isMainApk": file.main
                    });
                    if let Some(services) = &file.services_type {
                        call_args["servicesType"] = json!(services);
                    }
                    ("upload_apk", call_args)
                }
            };
            let response = ctx.call(op, call_args).await.map_err(|err| {
                let done: Vec<&str> = uploaded
                    .iter()
                    .filter_map(|item| item["file"].as_str())
                    .collect();
                ToolError::new(format!(
                    "{}; {draft_note}; uploaded before the failure: {}. Nothing was submitted \
                     for moderation",
                    err.message(),
                    if done.is_empty() {
                        "none".to_owned()
                    } else {
                        done.join(", ")
                    }
                ))
            })?;
            let mut record = json!({
                "file": file.path,
                "type": file.kind.as_str(),
                "bytes": file.bytes,
            });
            if file.kind == BuildKind::Apk {
                record["isMainApk"] = json!(file.main);
                record["servicesType"] =
                    json!(file.services_type.as_deref().unwrap_or("Unknown"));
            }
            if let (Value::Object(map), Value::Object(found)) =
                (&mut record, pick(&response, &["permissions", "autoDetectedSdks"]))
            {
                map.extend(found);
            }
            uploaded.push(record);
        }

        if !files.iter().any(|f| f.kind == BuildKind::Aab)
            && !files
                .iter()
                .any(|f| f.main && f.services_type.as_deref() != Some("HMS"))
        {
            notes.push(
                "no main non-HMS APK was uploaded in this call; moderation needs at least one \
                 in the version"
                    .to_owned(),
            );
        }

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("versionId".into(), json!(version_id));
        result.insert("draft".into(), json!(source.as_str()));
        if !updated_fields.is_empty() {
            result.insert("updated_fields".into(), json!(updated_fields));
        }
        result.insert("uploaded".into(), Value::Array(uploaded));
        match get_version(ctx, &package, version_id).await {
            Ok(details) => {
                result.insert("version".into(), version_state(&details));
            }
            Err(err) => notes.push(format!("draft state unavailable: {}", err.message())),
        }
        notes.push(
            "the draft is not submitted for moderation; submit it with `release_submit` after \
             the user confirms"
                .to_owned(),
        );
        result.insert("note".into(), json!(notes.join("; ")));
        Ok(Value::Object(result))
    })
}

// ---------------------------------------------------------------------------------------
// release_submit

fn dry_run_property() -> Value {
    json!({
        "type": "boolean",
        "description": "Only read the current state and return the planned calls; change \
            nothing (default false)."
    })
}

fn release_submit_schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "versionId": {
                "type": "integer",
                "description": "Draft version to submit. Omit to submit the draft of the app."
            },
            "publishType": {
                "type": "string",
                "enum": PUBLISH_TYPES,
                "description": "Publication after moderation: INSTANTLY (RuStore's default; \
                    published to users automatically as soon as moderation passes), MANUAL \
                    (waits for `release_rollout` with action `publish`), DELAYED (published at \
                    `publishDateTime`). Omit to keep the draft's setting."
            },
            "publishDateTime": {
                "type": "string",
                "description": "With publishType DELAYED (required then): when to publish, \
                    format yyyy-MM-ddTHH:mm:ssXXX like 2026-07-08T13:24:41+03:00, from 24 hours \
                    to 60 days after the submit."
            },
            "partialValue": {
                "type": "integer",
                "enum": PARTIAL_VALUES,
                "description": "Publish to this percent of users (5, 10, 25, 50, 75 or 100); \
                    only with INSTANTLY or MANUAL publication. The percent can later only grow."
            },
            "priorityUpdate": {
                "type": "integer",
                "minimum": 0,
                "maximum": 5,
                "description": "Update priority from 0 (default) to 5."
            },
            "check_build": {
                "type": "boolean",
                "description": "Check with `get_version` first that the version is a DRAFT \
                    with an uploaded build (default true)."
            },
            "dry_run": dry_run_property()
        }),
        &[],
    )
}

/// Что произойдёт после модерации при типе публикации `publish_type`.
fn after_moderation(publish_type: Option<&str>, partial: Option<u64>) -> String {
    let share = match partial {
        Some(value) if value < 100 => format!("{value}% of users"),
        _ => "users".to_owned(),
    };
    match publish_type {
        Some("MANUAL") => "after moderation the version waits in READY_FOR_PUBLICATION until it \
            is published with `release_rollout` (action `publish`)"
            .to_owned(),
        Some("DELAYED") => {
            "after moderation the version is published automatically at `publishDateTime`"
                .to_owned()
        }
        Some("INSTANTLY") => {
            format!("the version is published to {share} automatically as soon as it passes moderation")
        }
        _ => format!(
            "the version is published to {share} automatically as soon as it passes moderation \
             (INSTANTLY is RuStore's default publication type)"
        ),
    }
}

fn release_submit<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let given = optional_u64(&args, "versionId")?;
        let publish_type = publish_type_arg(&args)?;
        let publish_date_time = publish_date_time_arg(&args)?;
        let partial = partial_value_arg(&args)?;
        let priority = optional_u64(&args, "priorityUpdate")?;
        if priority.is_some_and(|p| p > 5) {
            return Err(ToolError::new("argument `priorityUpdate` must be from 0 to 5"));
        }
        let check_build = bool_arg(&args, "check_build", true)?;
        let dry_run = bool_arg(&args, "dry_run", false)?;
        match (publish_type.as_deref(), &publish_date_time) {
            (Some("DELAYED"), None) => {
                return Err(ToolError::new(
                    "publishType DELAYED needs `publishDateTime`",
                ));
            }
            (Some("DELAYED"), Some(_)) | (_, None) => {}
            (_, Some(_)) => {
                return Err(ToolError::new(
                    "`publishDateTime` is used only with publishType DELAYED; pass both",
                ));
            }
        }
        if partial.is_some() && publish_type.as_deref() == Some("DELAYED") {
            return Err(ToolError::new(
                "`partialValue` works only with INSTANTLY or MANUAL publication, not DELAYED",
            ));
        }

        let version_id = match given {
            Some(id) => id,
            None => {
                let drafts = find_drafts(ctx, &package).await?;
                match drafts.first().and_then(version_id_of) {
                    Some(id) => id,
                    None => {
                        return Err(ToolError::new(format!(
                            "{package} has no draft version; create one and upload a build with \
                             `upload_build` first"
                        )));
                    }
                }
            }
        };

        let mut draft_publish_type: Option<String> = None;
        let mut draft_partial: Option<u64> = None;
        let mut before = Value::Null;
        if check_build {
            let details = get_version(ctx, &package, version_id).await?;
            if let Some(status) = status_of(&details)
                && status != "DRAFT"
            {
                return Err(ToolError::new(format!(
                    "version {version_id} is {status}, not a draft: only a DRAFT version can be \
                     submitted for moderation. `release_summary` shows the versions"
                )));
            }
            let packages = details
                .get("packages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if packages.is_empty() {
                return Err(ToolError::new(format!(
                    "draft version {version_id} has no uploaded build (APK or AAB); upload one \
                     with `upload_build` first"
                )));
            }
            if packages
                .iter()
                .all(|p| text_field(p, "servicesType").is_some_and(|s| s.eq_ignore_ascii_case("HMS")))
            {
                return Err(ToolError::new(format!(
                    "draft version {version_id} has only HMS APKs; RuStore needs at least one \
                     main APK without Huawei Mobile Services"
                )));
            }
            draft_publish_type = text_field(&details, "publishType").map(|s| s.to_uppercase());
            draft_partial = details.get("partialValue").and_then(as_u64);
            if partial.is_some()
                && publish_type.is_none()
                && draft_publish_type.as_deref() == Some("DELAYED")
            {
                return Err(ToolError::new(
                    "the draft uses DELAYED publication, and `partialValue` works only with \
                     INSTANTLY or MANUAL; pass `publishType` too",
                ));
            }
            before = version_state(&details);
        }

        let mut plan = Vec::new();
        let mut settings = Map::new();
        if let Some(value) = &publish_type {
            settings.insert("publishType".into(), json!(value));
        }
        if let Some(value) = &publish_date_time {
            settings.insert("publishDateTime".into(), json!(value));
        }
        if let Some(value) = partial {
            settings.insert("partialValue".into(), json!(value));
        }
        if !settings.is_empty() {
            plan.push(planned(
                "update_draft_version",
                json!({"packageName": package, "versionId": version_id, "body": settings}),
            ));
        }
        let mut submit = json!({"packageName": package, "versionId": version_id});
        if let Some(value) = priority {
            submit["priorityUpdate"] = json!(value);
        }
        plan.push(planned("submit_draft_for_moderation", submit));

        let effective_type = publish_type.or(draft_publish_type);
        let effective_partial = partial.or(draft_partial);
        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("versionId".into(), json!(version_id));
        result.insert("dry_run".into(), json!(dry_run));
        if let Some(value) = &effective_type {
            result.insert("publishType".into(), json!(value));
        }
        result.insert(
            "after_moderation".into(),
            json!(after_moderation(effective_type.as_deref(), effective_partial)),
        );
        if dry_run {
            result.insert("planned".into(), Value::Array(plan));
            if !before.is_null() {
                result.insert("version".into(), before);
            }
            result.insert(
                "note".into(),
                json!("dry run: nothing was changed; run again without `dry_run` after the user confirms"),
            );
            return Ok(Value::Object(result));
        }
        let executed = execute_plan(ctx, &plan).await?;
        result.insert("executed".into(), Value::Array(executed));
        match get_version(ctx, &package, version_id).await {
            Ok(details) => {
                result.insert("version".into(), version_state(&details));
            }
            Err(err) => {
                result.insert(
                    "note".into(),
                    json!(format!(
                        "submitted; the version state is unavailable: {}",
                        err.message()
                    )),
                );
            }
        }
        Ok(Value::Object(result))
    })
}

// ---------------------------------------------------------------------------------------
// release_rollout

const ROLLOUT_ACTIONS: [&str; 3] = ["status", "publish", "set_percent"];
/// Версии, с которыми работает `release_rollout`.
const ROLLOUT_STATUSES: [&str; 2] = ["PARTIAL_ACTIVE", "READY_FOR_PUBLICATION"];

fn release_rollout_schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "action": {
                "type": "string",
                "enum": ROLLOUT_ACTIONS,
                "description": "status: report the version state, change nothing. publish: \
                    publish a READY_FOR_PUBLICATION version with MANUAL publication now. \
                    set_percent: raise the rollout percent to `partialValue`."
            },
            "versionId": {
                "type": "integer",
                "description": "Version to act on. Omit to use the version in PARTIAL_ACTIVE or \
                    READY_FOR_PUBLICATION (an error lists them when there are several)."
            },
            "partialValue": {
                "type": "integer",
                "enum": PARTIAL_VALUES,
                "description": "Rollout percent: 5, 10, 25, 50, 75 or 100 (all users). \
                    Required for set_percent and must be larger than the current percent; \
                    optional for publish (set before publishing)."
            },
            "dry_run": dry_run_property()
        }),
        &["action"],
    )
}

fn release_rollout<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let action = optional_str(&args, "action")?
            .map(str::to_lowercase)
            .filter(|a| ROLLOUT_ACTIONS.contains(&a.as_str()))
            .ok_or_else(|| invalid_argument("action", "status, publish or set_percent"))?;
        let given = optional_u64(&args, "versionId")?;
        let partial = partial_value_arg(&args)?;
        let dry_run = bool_arg(&args, "dry_run", false)?;
        if action == "set_percent" && partial.is_none() {
            return Err(ToolError::new("action set_percent needs `partialValue`"));
        }
        if action == "status" && partial.is_some() {
            return Err(ToolError::new(
                "action status changes nothing; `partialValue` is for publish or set_percent",
            ));
        }

        let version_id = match given {
            Some(id) => id,
            None => {
                let candidates = find_versions(ctx, &package, &ROLLOUT_STATUSES).await?;
                match candidates.as_slice() {
                    [] => {
                        return Err(ToolError::new(format!(
                            "{package} has no version in PARTIAL_ACTIVE or \
                             READY_FOR_PUBLICATION; `release_summary` shows the versions"
                        )));
                    }
                    [only] => version_id_of(only).unwrap_or_default(),
                    several => {
                        let list: Vec<String> = several
                            .iter()
                            .map(|v| {
                                format!(
                                    "{} ({})",
                                    version_id_of(v).unwrap_or_default(),
                                    status_of(v).unwrap_or_default()
                                )
                            })
                            .collect();
                        return Err(ToolError::new(format!(
                            "several versions can be published or rolled out: {}; pass \
                             `versionId`",
                            list.join(", ")
                        )));
                    }
                }
            }
        };

        let details = get_version(ctx, &package, version_id).await?;
        let state = version_state(&details);
        let status = status_of(&details).unwrap_or_default();
        let publish_type = text_field(&details, "publishType").map(|s| s.to_uppercase());
        let current = details.get("partialValue").and_then(as_u64);

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("versionId".into(), json!(version_id));
        result.insert("action".into(), json!(action));

        let mut plan = Vec::new();
        match action.as_str() {
            "status" => {
                result.insert("version".into(), state);
                result.insert(
                    "next".into(),
                    json!(next_steps(&status, publish_type.as_deref(), current)),
                );
                return Ok(Value::Object(result));
            }
            "publish" => {
                if status != "READY_FOR_PUBLICATION" {
                    return Err(ToolError::new(format!(
                        "version {version_id} is {status}: only a READY_FOR_PUBLICATION version \
                         (approved by moderation) can be published{}",
                        if status == "PARTIAL_ACTIVE" {
                            "; it is already published, raise its percent with action set_percent"
                        } else {
                            ""
                        }
                    )));
                }
                if let Some(kind) = publish_type.as_deref()
                    && kind != "MANUAL"
                {
                    return Err(ToolError::new(format!(
                        "version {version_id} uses {kind} publication; RuStore publishes \
                         manually only versions with MANUAL publication. Change the type with \
                         `update_publish_settings` first if the user wants to publish now"
                    )));
                }
                if let Some(value) = partial {
                    plan.push(planned(
                        "update_publish_settings",
                        json!({
                            "packageName": package,
                            "versionId": version_id,
                            "body": {"partialValue": value}
                        }),
                    ));
                }
                plan.push(planned(
                    "publish_version",
                    json!({"packageName": package, "versionId": version_id}),
                ));
            }
            _ => {
                let Some(value) = partial else {
                    return Err(ToolError::new("action set_percent needs `partialValue`"));
                };
                if !ROLLOUT_STATUSES.contains(&status.as_str()) {
                    return Err(ToolError::new(format!(
                        "version {version_id} is {status}: the rollout percent changes only for \
                         a PARTIAL_ACTIVE or READY_FOR_PUBLICATION version{}",
                        if status == "ACTIVE" {
                            "; it is already published to all users"
                        } else {
                            ""
                        }
                    )));
                }
                if status == "PARTIAL_ACTIVE"
                    && let Some(now) = current
                {
                    if value < now {
                        return Err(ToolError::new(format!(
                            "version {version_id} is already published to {now}% of users; \
                             RuStore cannot lower the rollout percent, only raise it (to {})",
                            PARTIAL_VALUES
                                .iter()
                                .filter(|v| **v > now)
                                .map(|v| format!("{v}%"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                    if value == now {
                        result.insert("version".into(), state);
                        result.insert(
                            "note".into(),
                            json!(format!("the rollout percent is already {now}%; nothing to change")),
                        );
                        return Ok(Value::Object(result));
                    }
                }
                plan.push(planned(
                    "update_publish_settings",
                    json!({
                        "packageName": package,
                        "versionId": version_id,
                        "body": {"partialValue": value}
                    }),
                ));
            }
        }

        result.insert("dry_run".into(), json!(dry_run));
        if dry_run {
            result.insert("planned".into(), Value::Array(plan));
            result.insert("version".into(), state);
            result.insert(
                "note".into(),
                json!("dry run: nothing was changed; run again without `dry_run` after the user confirms"),
            );
            return Ok(Value::Object(result));
        }
        let executed = execute_plan(ctx, &plan).await?;
        result.insert("executed".into(), Value::Array(executed));
        // Без `partialValue` публикация идёт на процент, уже заданный версии.
        let effect = match partial.or(current) {
            Some(value) if value < 100 => {
                format!("the version is being published to {value}% of users")
            }
            _ => "the version is being published to all users".to_owned(),
        };
        result.insert("effect".into(), json!(effect));
        match get_version(ctx, &package, version_id).await {
            Ok(after) => {
                result.insert("version".into(), version_state(&after));
            }
            Err(err) => {
                result.insert(
                    "note".into(),
                    json!(format!("done; the version state is unavailable: {}", err.message())),
                );
            }
        }
        Ok(Value::Object(result))
    })
}

/// Что можно сделать с версией дальше (для действия `status`).
fn next_steps(status: &str, publish_type: Option<&str>, current: Option<u64>) -> String {
    match status {
        "READY_FOR_PUBLICATION" if publish_type == Some("MANUAL") => {
            "approved: publish it with action publish (optionally with `partialValue`)".to_owned()
        }
        "READY_FOR_PUBLICATION" if publish_type == Some("DELAYED") => {
            "approved: RuStore publishes it at `publishDateTime`".to_owned()
        }
        "READY_FOR_PUBLICATION" => "approved and waiting for publication".to_owned(),
        "PARTIAL_ACTIVE" => match current {
            Some(now) if now < 100 => format!(
                "published to {now}% of users: raise the percent with action set_percent \
                 (it cannot be lowered)"
            ),
            _ => "partially published: raise the percent with action set_percent".to_owned(),
        },
        "ACTIVE" => "published to all users".to_owned(),
        other => format!(
            "the version is {other}; publish and set_percent work only for \
             READY_FOR_PUBLICATION and PARTIAL_ACTIVE"
        ),
    }
}

#[cfg(test)]
mod tests;
