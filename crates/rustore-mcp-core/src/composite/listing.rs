//! Составные инструменты группы: тексты, иконка и скриншоты (набор `listing`).
//!
//! Тексты карточки в RuStore — поля версии: название, описания, «Что нового», категории,
//! возраст, контакты и т. п. живут в версии (`get_version`), а меняются только в черновике
//! (`update_draft_version`). Скриншоты — тоже у версии (`list_screenshots`,
//! `upload_screenshot_v2`, `delete_screenshot`).
//!
//! - `listing_export` — тексты версии (активной, черновика или по `versionId`) и сведения об
//!   изображениях: в ответ или в каталог (`listing.json`, `images.json`). Картинки не
//!   скачиваются: RuStore отдаёт ссылки на своё хранилище, а сервер ходит только в RuStore
//!   API (исполнитель принимает только относительные пути API).
//! - `listing_apply` — тексты из `listing.json` (или аргумента) в черновик: разница по полям,
//!   `dry_run`, проверка ограничений справочника до запроса, `PATCH` только изменённых полей.
//! - `sync_screenshots` — каталог скриншотов (`<устройство>/<ориентация>/<номер>.png`) в
//!   черновик: загрузка, удаление лишних по `delete_extra`, `dry_run`. Хеша и размера файла
//!   API не отдаёт, поэтому изменённый файл не отличить от прежнего: по умолчанию все
//!   локальные файлы загружаются заново, `skip_existing` пропускает занятые места.
//!
//! Как добавить инструмент — в описании модуля [`super`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::common::{
    bool_arg, object_schema, optional_str, optional_u64, package_name, package_name_property, pick,
    required_str, string_list, text_field,
};
use super::{Collect, CompositeTool, Context, ToolFuture};
use crate::error::ToolError;
use crate::files::FilePolicyError;

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[
    CompositeTool {
        name: "listing_export",
        toolset: "listing",
        access: AccessLevel::Read,
        title: "Export the store listing of an app",
        description: "Exports the store listing of an app: the listing texts of a version \
            (appName, appType, categories, ageLegal, shortDescription, fullDescription, \
            whatsNew, the TV texts, developerContacts, faq, seoTagIds, privacyExplanation, \
            inAppPurchaseTypes, video, backgroundVideo) and the icon and screenshots of the \
            version (type, orientation, ordinal, id, storage links). In RuStore the listing \
            belongs to a version: by default the tool reads the published version (ACTIVE or \
            PARTIAL_ACTIVE, the newest one), `draft: true` reads the draft, `versionId` any \
            version. Without `dir` it returns the listing inline. With `dir` it writes \
            listing.json (edit its `listing` object and apply it with `listing_apply`) and \
            images.json (with `sync_path`, where `sync_screenshots` expects each screenshot \
            file) to that directory; the directory may be new (its parent must exist) or \
            existing, and existing files are never overwritten. Images are not downloaded: \
            the server calls only the RuStore API, and `fileUrl` and `iconUrl` point to the \
            RuStore storage.",
        input_schema: listing_export_schema,
        uses_local_files: true,
        run: listing_export,
    },
    CompositeTool {
        name: "listing_apply",
        toolset: "listing",
        access: AccessLevel::Write,
        title: "Apply listing texts to the draft",
        description: "Applies listing texts to the draft version of an app: from listing.json \
            in `dir` (the layout of `listing_export`, its `listing` object) or from the \
            `listing` argument (an object with the same fields). Compares every field with \
            the draft (`get_version`) and sends only the changed fields with \
            `update_draft_version`; `dry_run: true` returns the field-by-field difference and \
            changes nothing. `fields` limits the fields to apply. Values are checked against \
            the documented limits before any request (appName up to 50 characters, \
            shortDescription 80, fullDescription 4000, whatsNew 5000, moderInfo 180, at most \
            2 categories, 10 faq items, 5 seoTagIds and so on). Fields that are missing or \
            null are not changed. The tool needs an existing draft: without `versionId` it \
            uses the draft of the app; when there is none, create it with \
            `create_draft_version` first (only a draft can be changed).",
        input_schema: listing_apply_schema,
        uses_local_files: true,
        run: listing_apply,
    },
    CompositeTool {
        name: "sync_screenshots",
        toolset: "listing",
        access: AccessLevel::Write,
        title: "Sync screenshots of the draft with a directory",
        description: "Syncs the screenshots of the draft version with a local directory. \
            Layout: `<dir>/<device>/<orientation>/<ordinal>.png` (or .jpg, .jpeg), where device \
            is `phone`, `tablet` or `tv` (SCREENSHOT, TABLET_SCREENSHOT, TV_SCREENSHOT), \
            orientation is `portrait` or `landscape` and ordinal is the RuStore position from \
            0 to 9, like `phone/portrait/0.png`; `<dir>/portrait/0.png` is short for phone. A \
            `listing_export` directory works too: its `screenshots/` subdirectory is used. \
            Only device types with a local directory are synced; the others stay untouched. \
            Files are checked before any request: PNG or JPEG matching the extension, at most \
            5 MB, sides from 320 to 3840 px, the orientation of the directory, one \
            orientation per device type (RuStore rejects mixed orientations); an aspect ratio \
            other than 16:9 or 9:16 is a warning. RuStore exposes no hash or size of uploaded \
            screenshots, so a changed file cannot be detected: every local file is uploaded \
            again (`upload_screenshot_v2`, it replaces the screenshot at that position) \
            unless `skip_existing: true` skips positions that already have a screenshot. \
            Screenshots of the draft without a local file are deleted only with \
            `delete_extra: true`, and so are screenshots of the other orientation (then they \
            are deleted before the uploads). `dry_run: true` returns the plan and changes \
            nothing. Without `versionId` the draft of the app is used; create it with \
            `create_draft_version` when there is none.",
        input_schema: sync_screenshots_schema,
        uses_local_files: true,
        run: sync_screenshots,
    },
];

/// Поля карточки, которые `listing_export` читает из версии, а `listing_apply` пишет в
/// черновик. Цена, тип и дата публикации, раскатка — настройки выпуска, а не карточки;
/// `privacyDataTypes` в ответе `get_version` — объект категорий, а в запросе — список
/// строк, поэтому круговой путь для него не работает, и поле не переносится.
const LISTING_FIELDS: &[&str] = &[
    "appName",
    "appType",
    "categories",
    "ageLegal",
    "shortDescription",
    "fullDescription",
    "whatsNew",
    "shortDescriptionTv",
    "fullDescriptionTv",
    "whatsNewTv",
    "developerContacts",
    "faq",
    "seoTagIds",
    "privacyExplanation",
    "inAppPurchaseTypes",
    "video",
    "backgroundVideo",
];

/// Поле только для записи: комментарий модератору `get_version` не возвращает.
const MODER_INFO: &str = "moderInfo";

/// Длинные тексты: разница — построчно, а не значениями целиком.
const LONG_TEXTS: &[&str] = &[
    "fullDescription",
    "fullDescriptionTv",
    "whatsNew",
    "whatsNewTv",
    "privacyExplanation",
];

/// Списки, порядок которых не важен при сравнении.
const UNORDERED_LISTS: &[&str] = &["categories", "seoTagIds", "inAppPurchaseTypes"];

const CONTACT_FIELDS: &[&str] = &["email", "website", "vkCommunity", "maxMessenger"];

/// Файлы каталога экспорта.
const LISTING_FILE: &str = "listing.json";
const IMAGES_FILE: &str = "images.json";
/// Подкаталог скриншотов в каталоге экспорта.
const SCREENSHOTS_DIR: &str = "screenshots";
/// Предел размера listing.json: тексты карточки — десятки килобайт.
const MAX_LISTING_FILE: u64 = 1024 * 1024;

/// Каталоги устройств и типы скриншотов `upload_screenshot_v2`.
const DEVICES: &[(&str, &str)] = &[
    ("phone", "SCREENSHOT"),
    ("tablet", "TABLET_SCREENSHOT"),
    ("tv", "TV_SCREENSHOT"),
];
const ORIENTATIONS: &[(&str, &str)] = &[("portrait", "PORTRAIT"), ("landscape", "LANDSCAPE")];
/// Тип скриншота, если API его не вернул (скриншоты, загруженные методом v1).
const DEFAULT_TYPE: &str = "SCREENSHOT";

/// Ограничения скриншотов по справочнику RuStore (загрузка скриншотов v1 и v2).
const MAX_SCREENSHOT_BYTES: u64 = 5 * 1024 * 1024;
const MIN_SIDE: u32 = 320;
const MAX_SIDE: u32 = 3840;
const MAX_ORDINAL: u64 = 9;
/// Минимум скриншотов для публикации.
const MIN_FOR_PUBLICATION: usize = 3;

// --- Схемы -------------------------------------------------------------------------------

fn version_id_property(what: &str) -> Value {
    json!({
        "type": "integer",
        "description": format!("Version ID (`versionId`). {what}")
    })
}

fn listing_export_schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "versionId": version_id_property(
                "Default: the newest published version (ACTIVE or PARTIAL_ACTIVE)."
            ),
            "draft": {
                "type": "boolean",
                "description": "Export the draft version instead of the published one."
            },
            "dir": {
                "type": "string",
                "description": "Directory to write listing.json and images.json to (inside the \
                    allowed directories): a new directory whose parent exists, or an existing \
                    one without these files. Omit to get the listing inline."
            }
        }),
        &[],
    )
}

fn listing_apply_schema() -> Value {
    let mut fields: Vec<&str> = LISTING_FIELDS.to_vec();
    fields.push(MODER_INFO);
    object_schema(
        json!({
            "packageName": package_name_property(),
            "versionId": version_id_property("Must be a draft. Default: the draft of the app."),
            "dir": {
                "type": "string",
                "description": "Directory with listing.json in the layout of `listing_export` \
                    (its `listing` object is applied). Pass `dir` or `listing`."
            },
            "listing": {
                "type": "object",
                "description": format!(
                    "Listing fields to apply, as in the `listing` object of listing.json: {}. \
                     Pass `dir` or `listing`.",
                    fields.join(", ")
                )
            },
            "fields": {
                "type": "array",
                "items": {"type": "string", "enum": fields},
                "description": "Apply only these fields. Default: all fields of the listing."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Return the difference with the draft and change nothing."
            }
        }),
        &[],
    )
}

fn sync_screenshots_schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "versionId": version_id_property("Must be a draft. Default: the draft of the app."),
            "dir": {
                "type": "string",
                "description": "Directory with screenshots: `<device>/<orientation>/<ordinal>.png` \
                    (device phone, tablet or tv; orientation portrait or landscape; ordinal 0 to \
                    9), `<orientation>/<ordinal>.png` for phone, or a `listing_export` directory \
                    with a `screenshots/` subdirectory."
            },
            "delete_extra": {
                "type": "boolean",
                "description": "Delete screenshots of the draft that have no local file (and \
                    those of the other orientation) for the synced device types. Default: false."
            },
            "skip_existing": {
                "type": "boolean",
                "description": "Do not upload files for positions that already have a \
                    screenshot. Default: false (RuStore exposes no hash, so every file is \
                    uploaded again)."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Return the plan (uploads, deletions, warnings) and change nothing."
            }
        }),
        &["dir"],
    )
}

// --- Общие помощники ------------------------------------------------------------------------

fn file_error(err: FilePolicyError) -> ToolError {
    ToolError::new(err.0)
}

/// Путь строкой для [`crate::files::FilePolicy`].
fn path_text(path: &Path) -> Result<&str, ToolError> {
    path.to_str()
        .ok_or_else(|| ToolError::new(format!("unsupported path `{}`", path.display())))
}

/// Числовой идентификатор (число или строка с числом).
fn id_of(value: &Value, field: &str) -> Option<u64> {
    match value.get(field)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Самая новая версия (с наибольшим `versionId`) в статусах `statuses`.
async fn newest_version(
    ctx: &Context,
    package: &str,
    statuses: &[&str],
    testing_type: Option<&str>,
) -> Result<Option<Value>, ToolError> {
    let mut args = json!({"packageName": package, "versionStatuses": statuses.join(",")});
    if let Some(testing_type) = testing_type {
        args["filterTestingType"] = json!(testing_type);
    }
    let collected = ctx
        .collect(
            "list_versions",
            args,
            Collect {
                page_size: 100,
                max_items: 1000,
            },
        )
        .await?;
    // Фильтр статусов повторяется локально: справочник не обещает его точность.
    Ok(collected
        .items
        .into_iter()
        .filter(|version| {
            text_field(version, "versionStatus").is_none_or(|s| statuses.contains(&s.as_str()))
        })
        .filter(|version| id_of(version, "versionId").is_some())
        .max_by_key(|version| id_of(version, "versionId").unwrap_or(0)))
}

/// Подробности версии `get_version`.
async fn get_version(ctx: &Context, package: &str, id: u64) -> Result<Value, ToolError> {
    ctx.call(
        "get_version",
        json!({"packageName": package, "versionId": id}),
    )
    .await
}

/// Черновик: `versionId` из аргумента (статус проверяется) или единственный черновик
/// приложения. Возвращает идентификатор и подробности версии.
async fn draft_version(
    ctx: &Context,
    package: &str,
    version_id: Option<u64>,
) -> Result<(u64, Value), ToolError> {
    let id = match version_id {
        Some(id) => id,
        None => newest_version(ctx, package, &["DRAFT"], Some("ALL"))
            .await?
            .and_then(|version| id_of(&version, "versionId"))
            .ok_or_else(|| {
                ToolError::new(format!(
                    "`{package}` has no draft version; create one with `create_draft_version` \
                     (only a draft can be changed), then call this tool again"
                ))
            })?,
    };
    let version = get_version(ctx, package, id).await?;
    match text_field(&version, "versionStatus") {
        Some(status) if status != "DRAFT" => Err(ToolError::new(format!(
            "version {id} is {status}, not a draft: only a draft can be changed. Create a draft \
             with `create_draft_version`, or omit `versionId` to use the draft of the app"
        ))),
        _ => Ok((id, version)),
    }
}

/// Краткие сведения о версии для ответа.
fn version_summary(version: &Value) -> Value {
    pick(
        version,
        &[
            "versionId",
            "versionName",
            "versionStatus",
            "appType",
            "testingType",
        ],
    )
}

// --- listing_export ----------------------------------------------------------------------

/// Значение поля карточки в виде, который принимает `update_draft_version`: у `faq` нет
/// `id`, у контактов — только поля запроса. `None` — поля нет или оно пустое.
fn listing_value(field: &str, value: Option<&Value>) -> Option<Value> {
    let value = value.filter(|value| !value.is_null())?;
    Some(match field {
        "faq" => match value {
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| pick(item, &["question", "answer"]))
                    .collect(),
            ),
            other => other.clone(),
        },
        "developerContacts" => match value {
            Value::Object(_) => pick(value, CONTACT_FIELDS),
            other => other.clone(),
        },
        _ => value.clone(),
    })
}

/// Поля карточки версии.
fn export_listing(version: &Value) -> Map<String, Value> {
    LISTING_FIELDS
        .iter()
        .filter_map(|field| {
            listing_value(field, version.get(*field)).map(|value| ((*field).to_owned(), value))
        })
        .collect()
}

fn device_dir(kind: &str) -> Option<&'static str> {
    DEVICES
        .iter()
        .find(|(_, k)| *k == kind)
        .map(|(dir, _)| *dir)
}

fn orientation_dir(orientation: &str) -> Option<&'static str> {
    ORIENTATIONS
        .iter()
        .find(|(_, o)| *o == orientation)
        .map(|(dir, _)| *dir)
}

/// Тип скриншота записи API (без типа — скриншот телефона).
fn shot_type(item: &Value) -> String {
    text_field(item, "type").unwrap_or_else(|| DEFAULT_TYPE.to_owned())
}

/// Компактная запись скриншота и путь, по которому его ждёт `sync_screenshots`.
fn export_screenshot(item: &Value) -> Value {
    let mut record = pick(
        item,
        &["id", "type", "orientation", "ordinal", "fileUid", "fileUrl"],
    );
    let kind = shot_type(item);
    let orientation = text_field(item, "orientation").unwrap_or_default();
    if let (Some(device), Some(dir), Some(ordinal), Value::Object(map)) = (
        device_dir(&kind),
        orientation_dir(&orientation),
        id_of(item, "ordinal"),
        &mut record,
    ) {
        let url = text_field(item, "fileUrl").unwrap_or_default().to_lowercase();
        let ext = if url.ends_with(".jpg") || url.ends_with(".jpeg") {
            "jpg"
        } else {
            "png"
        };
        map.insert(
            "sync_path".into(),
            json!(format!("{SCREENSHOTS_DIR}/{device}/{dir}/{ordinal}.{ext}")),
        );
    }
    record
}

fn screenshot_key(item: &Value) -> (String, String, u64) {
    (
        shot_type(item),
        text_field(item, "orientation").unwrap_or_default(),
        id_of(item, "ordinal").unwrap_or(u64::MAX),
    )
}

/// Скриншоты версии из `list_screenshots`.
async fn list_screenshots(ctx: &Context, package: &str, id: u64) -> Result<Vec<Value>, ToolError> {
    let collected = ctx
        .collect(
            "list_screenshots",
            json!({"packageName": package, "versionId": id}),
            Collect {
                page_size: 50,
                max_items: 500,
            },
        )
        .await?;
    Ok(collected.items)
}

/// Куда пишет экспорт: канонический каталог и нужно ли его создать.
struct ExportDir {
    path: PathBuf,
    create: bool,
}

/// Проверяет каталог экспорта до запросов: существующий — без listing.json и images.json,
/// новый — с существующим родителем внутри разрешённых каталогов.
fn prepare_export_dir(ctx: &Context, dir: &str) -> Result<ExportDir, ToolError> {
    let files = ctx.files();
    let target = match files.check_dir(dir) {
        Ok(path) => ExportDir {
            path,
            create: false,
        },
        Err(_) => ExportDir {
            path: files.check_write(dir).map_err(file_error)?,
            create: true,
        },
    };
    if !target.create {
        for name in [LISTING_FILE, IMAGES_FILE] {
            files
                .check_write(path_text(&target.path.join(name))?)
                .map_err(file_error)?;
        }
    }
    Ok(target)
}

/// Новый JSON-файл; существующий не перезаписывается (`create_new`).
fn write_new_json(ctx: &Context, path: &Path, value: &Value) -> Result<(), ToolError> {
    let target = ctx
        .files()
        .check_write(path_text(path)?)
        .map_err(file_error)?;
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|err| ToolError::new(format!("cannot encode JSON: {err}")))?;
    text.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|err| ToolError::new(format!("cannot create `{}`: {err}", path.display())))?;
    file.write_all(text.as_bytes())
        .map_err(|err| ToolError::new(format!("cannot write `{}`: {err}", path.display())))
}

fn listing_export<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let version_id = optional_u64(&args, "versionId")?;
        let from_draft = bool_arg(&args, "draft", false)?;
        if version_id.is_some() && from_draft {
            return Err(ToolError::new("pass either `versionId` or `draft`, not both"));
        }
        let dir = optional_str(&args, "dir")?;
        // Каталог проверяется до запросов, создаётся — после них.
        let target = dir.map(|dir| prepare_export_dir(ctx, dir)).transpose()?;

        let (id, version) = match version_id {
            Some(id) => (id, get_version(ctx, &package, id).await?),
            None if from_draft => draft_version(ctx, &package, None).await?,
            None => {
                let id = newest_version(ctx, &package, &["ACTIVE", "PARTIAL_ACTIVE"], None)
                    .await?
                    .and_then(|version| id_of(&version, "versionId"))
                    .ok_or_else(|| {
                        ToolError::new(format!(
                            "`{package}` has no published version; pass `versionId` or \
                             `draft: true`"
                        ))
                    })?;
                (id, get_version(ctx, &package, id).await?)
            }
        };

        let mut warnings = Vec::new();
        let (mut screenshots, source) = match list_screenshots(ctx, &package, id).await {
            Ok(items) => (items, "list_screenshots"),
            Err(err) => {
                // Справочник описывает список скриншотов для черновика; у других версий
                // остаются файлы версии из `get_version`.
                warnings.push(format!(
                    "{}; screenshots are taken from `fileUids` of `get_version`",
                    err.message()
                ));
                let items = version
                    .get("fileUids")
                    .and_then(Value::as_array)
                    .map(|files| {
                        files
                            .iter()
                            .filter(|file| {
                                text_field(file, "type").is_some_and(|t| t.ends_with("SCREENSHOT"))
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                (items, "get_version")
            }
        };
        screenshots.sort_by_key(screenshot_key);
        let screenshots: Vec<Value> = screenshots.iter().map(export_screenshot).collect();

        let listing = export_listing(&version);
        let summary = version_summary(&version);
        let mut images = Map::new();
        images.insert("packageName".into(), json!(package));
        images.insert("versionId".into(), json!(id));
        let icon = pick(&version, &["iconUid", "iconUrl", "iconUidTv", "iconUrlTv"]);
        if icon.as_object().is_some_and(|icon| !icon.is_empty()) {
            images.insert("icon".into(), icon);
        }
        images.insert("screenshots_source".into(), json!(source));
        images.insert("screenshots".into(), json!(screenshots));

        let note = "Images are not downloaded: `fileUrl` and `iconUrl` point to the RuStore \
            storage, and this server calls only the RuStore API. To sync screenshots, put the \
            files at `sync_path` (relative to the export directory) and call \
            `sync_screenshots` with that directory.";
        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("version".into(), summary.clone());
        match target {
            None => {
                result.insert("listing".into(), Value::Object(listing));
                result.insert("images".into(), Value::Object(images));
            }
            Some(target) => {
                if target.create {
                    std::fs::create_dir(&target.path).map_err(|err| {
                        ToolError::new(format!(
                            "cannot create `{}`: {err}",
                            dir.unwrap_or_default()
                        ))
                    })?;
                }
                let fields: Vec<String> = listing.keys().cloned().collect();
                let mut file = Map::new();
                file.insert("packageName".into(), json!(package));
                for (key, value) in summary.as_object().into_iter().flatten() {
                    file.insert(key.clone(), value.clone());
                }
                file.insert("listing".into(), Value::Object(listing));
                write_new_json(ctx, &target.path.join(LISTING_FILE), &Value::Object(file))?;
                write_new_json(ctx, &target.path.join(IMAGES_FILE), &Value::Object(images))?;
                result.insert("dir".into(), json!(dir));
                result.insert("files".into(), json!([LISTING_FILE, IMAGES_FILE]));
                result.insert("fields".into(), json!(fields));
                result.insert("screenshots".into(), json!(screenshots.len()));
            }
        }
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        result.insert("note".into(), json!(note));
        Ok(Value::Object(result))
    })
}

// --- listing_apply -----------------------------------------------------------------------

/// Поля карточки из listing.json в каталоге `dir` (объект `listing`).
fn read_listing_file(ctx: &Context, dir: &str) -> Result<Map<String, Value>, ToolError> {
    let files = ctx.files();
    let root = files.check_dir(dir).map_err(file_error)?;
    let path = files
        .check_read(path_text(&root.join(LISTING_FILE))?)
        .map_err(file_error)?;
    let size = std::fs::metadata(&path)
        .map_err(|err| ToolError::new(format!("cannot read `{LISTING_FILE}`: {err}")))?
        .len();
    if size > MAX_LISTING_FILE {
        return Err(ToolError::new(format!(
            "`{LISTING_FILE}` is too large ({size} bytes)"
        )));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|err| ToolError::new(format!("cannot read `{LISTING_FILE}`: {err}")))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| ToolError::new(format!("`{LISTING_FILE}` is not valid JSON: {err}")))?;
    match value.get("listing") {
        Some(Value::Object(listing)) => Ok(listing.clone()),
        _ => Err(ToolError::new(format!(
            "`{LISTING_FILE}` has no `listing` object (the layout of `listing_export`)"
        ))),
    }
}

/// Длина текста в символах в пределах `min..=max`.
fn check_text(value: &Value, min: usize, max: usize) -> Result<(), String> {
    let Value::String(text) = value else {
        return Err("must be a string".into());
    };
    let len = text.chars().count();
    if len < min || len > max {
        return Err(if min > 0 {
            format!("must be from {min} to {max} characters, got {len}")
        } else {
            format!("must be at most {max} characters, got {len}")
        });
    }
    Ok(())
}

fn check_enum(value: &Value, allowed: &[&str]) -> Result<(), String> {
    match value {
        Value::String(s) if allowed.contains(&s.as_str()) => Ok(()),
        _ => Err(format!("must be one of {}", allowed.join(", "))),
    }
}

/// Массив уникальных строк из `allowed` (пустой `allowed` — любые) не длиннее `max`.
fn check_string_list(value: &Value, max: usize, allowed: &[&str]) -> Result<(), String> {
    let Value::Array(items) = value else {
        return Err("must be an array of strings".into());
    };
    if items.len() > max {
        return Err(format!("must have at most {max} items, got {}", items.len()));
    }
    let mut seen = BTreeSet::new();
    for item in items {
        let Value::String(s) = item else {
            return Err("must be an array of strings".into());
        };
        if !allowed.is_empty() && !allowed.contains(&s.as_str()) {
            return Err(format!("items must be {}", allowed.join(", ")));
        }
        if !seen.insert(s.as_str()) {
            return Err(format!("has `{s}` twice"));
        }
    }
    Ok(())
}

/// Объект только с полями `allowed`, значения — строки.
fn check_object(value: &Value, allowed: &[&str]) -> Result<(), String> {
    let Value::Object(map) = value else {
        return Err(format!("must be an object with {}", allowed.join(", ")));
    };
    for (key, item) in map {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "has unknown field `{key}`; allowed: {}",
                allowed.join(", ")
            ));
        }
        if !item.is_string() && !item.is_null() {
            return Err(format!("`{key}` must be a string"));
        }
    }
    Ok(())
}

/// Проверка поля по ограничениям справочника (создание и редактирование черновика).
fn check_field(field: &str, value: &Value) -> Result<(), String> {
    match field {
        "appName" => check_text(value, 1, 50),
        "shortDescription" => check_text(value, 0, 80),
        "fullDescription" | "privacyExplanation" => check_text(value, 0, 4000),
        "whatsNew" => check_text(value, 0, 5000),
        "moderInfo" => check_text(value, 0, 180),
        "shortDescriptionTv" => check_text(value, 1, 80),
        "fullDescriptionTv" => check_text(value, 1, 4000),
        "whatsNewTv" => check_text(value, 1, 5000),
        "appType" => check_enum(value, &["GAMES", "MAIN"]),
        "ageLegal" => check_enum(value, &["18+", "16+", "12+", "6+", "0+"]),
        "categories" => check_string_list(value, 2, &[]),
        "inAppPurchaseTypes" => check_string_list(value, 2, &["PRODUCTS", "SUBSCRIPTIONS"]),
        "developerContacts" => check_object(value, CONTACT_FIELDS),
        "seoTagIds" => match value {
            Value::Array(items) if items.len() > 5 => {
                Err(format!("must have at most 5 items, got {}", items.len()))
            }
            Value::Array(items) if items.iter().all(|item| item.as_u64().is_some()) => Ok(()),
            _ => Err("must be an array of tag IDs (integers)".into()),
        },
        "faq" => {
            let Value::Array(items) = value else {
                return Err("must be an array of {question, answer} objects".into());
            };
            if items.len() > 10 {
                return Err(format!("must have at most 10 items, got {}", items.len()));
            }
            for (index, item) in items.iter().enumerate() {
                check_object(item, &["question", "answer"])
                    .and_then(|()| {
                        check_text(item.get("question").unwrap_or(&Value::Null), 1, 120)
                            .map_err(|err| format!("`question` {err}"))
                    })
                    .and_then(|()| {
                        check_text(item.get("answer").unwrap_or(&Value::Null), 1, 500)
                            .map_err(|err| format!("`answer` {err}"))
                    })
                    .map_err(|err| format!("item {index}: {err}"))?;
            }
            Ok(())
        }
        "video" | "backgroundVideo" => {
            check_object(value, &["orientation", "url"])?;
            match value.get("orientation") {
                None | Some(Value::Null) => Ok(()),
                Some(orientation) => check_enum(orientation, &["LANDSCAPE", "PORTRAIT"])
                    .map_err(|err| format!("`orientation` {err}")),
            }
        }
        _ => Err("is not a listing field".into()),
    }
}

/// Значение для сравнения: у списков без порядка — отсортированное.
fn comparable(field: &str, value: &Value) -> Value {
    match value {
        Value::Array(items) if UNORDERED_LISTS.contains(&field) => {
            let mut items = items.clone();
            items.sort_by_key(Value::to_string);
            Value::Array(items)
        }
        other => other.clone(),
    }
}

/// Запись разницы поля: длинные тексты — построчно.
fn field_change(field: &str, current: &Value, new: &Value) -> Value {
    if LONG_TEXTS.contains(&field)
        && let Value::String(new_text) = new
    {
        let current_text = current.as_str().unwrap_or_default();
        return json!({
            "field": field,
            "current_characters": current_text.chars().count(),
            "new_characters": new_text.chars().count(),
            "lines": line_diff(current_text, new_text),
        });
    }
    json!({"field": field, "current": current, "new": new})
}

/// Изменённые строки: `- старая`, `+ новая` (по наибольшей общей подпоследовательности).
fn line_diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // Тексты короткие (до 5000 символов), квадратичная таблица допустима.
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    out
}

/// Поля для записи: известные, не `null`, с учётом `fields`; ошибки всех полей сразу.
fn desired_fields(
    listing: &Map<String, Value>,
    only: &[String],
) -> Result<Vec<(String, Value)>, ToolError> {
    let known = |name: &str| LISTING_FIELDS.contains(&name) || name == MODER_INFO;
    let mut errors = Vec::new();
    for name in listing.keys().chain(only.iter()) {
        if !known(name) {
            errors.push(format!("`{name}` is not a listing field"));
        }
    }
    if !errors.is_empty() {
        let mut allowed: Vec<&str> = LISTING_FIELDS.to_vec();
        allowed.push(MODER_INFO);
        return Err(ToolError::new(format!(
            "{}; listing fields: {}",
            errors.join("; "),
            allowed.join(", ")
        )));
    }
    let mut desired = Vec::new();
    // Порядок полей — как в [`LISTING_FIELDS`], чтобы разница читалась одинаково.
    let order = LISTING_FIELDS.iter().copied().chain([MODER_INFO]);
    for (name, value) in order.filter_map(|name| listing.get_key_value(name)) {
        if value.is_null() || (!only.is_empty() && !only.contains(name)) {
            continue;
        }
        if let Err(err) = check_field(name, value) {
            errors.push(format!("`{name}` {err}"));
        }
        desired.push((name.clone(), value.clone()));
    }
    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "the listing is invalid, nothing was changed: {}",
            errors.join("; ")
        )));
    }
    if desired.is_empty() {
        return Err(ToolError::new(
            "nothing to apply: the listing has no fields (or none of `fields`)",
        ));
    }
    Ok(desired)
}

fn listing_apply<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let version_id = optional_u64(&args, "versionId")?;
        let dry_run = bool_arg(&args, "dry_run", false)?;
        let only = string_list(&args, "fields")?;
        let dir = optional_str(&args, "dir")?;
        let listing = match (dir, args.get("listing")) {
            (Some(_), Some(value)) if !value.is_null() => {
                return Err(ToolError::new("pass either `dir` or `listing`, not both"));
            }
            (Some(dir), _) => read_listing_file(ctx, dir)?,
            (None, Some(Value::Object(listing))) => listing.clone(),
            (None, Some(value)) if !value.is_null() => {
                return Err(ToolError::new("argument `listing` must be an object"));
            }
            (None, _) => {
                return Err(ToolError::new(
                    "pass `dir` (a directory with listing.json) or `listing` (an object)",
                ));
            }
        };
        // Все проверки — до запросов.
        let desired = desired_fields(&listing, &only)?;

        let (id, version) = draft_version(ctx, &package, version_id).await?;
        let mut changes = Vec::new();
        let mut unchanged = Vec::new();
        let mut body = Map::new();
        for (field, new) in &desired {
            let current = listing_value(field, version.get(field.as_str())).unwrap_or(Value::Null);
            if field != MODER_INFO && comparable(field, &current) == comparable(field, new) {
                unchanged.push(field.clone());
                continue;
            }
            changes.push(field_change(field, &current, new));
            body.insert(field.clone(), new.clone());
        }

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("versionId".into(), json!(id));
        result.insert("dry_run".into(), json!(dry_run));
        let applied = !dry_run && !body.is_empty();
        if applied {
            ctx.call(
                "update_draft_version",
                json!({"packageName": package, "versionId": id, "body": Value::Object(body)}),
            )
            .await?;
        }
        result.insert("applied".into(), json!(applied));
        result.insert("changes".into(), Value::Array(changes));
        result.insert("unchanged".into(), json!(unchanged));
        let mut notes = Vec::new();
        if desired.iter().any(|(field, _)| field == MODER_INFO) {
            notes.push(
                "`moderInfo` is not returned by `get_version`, so it is always sent".to_owned(),
            );
        }
        if dry_run && result["changes"].as_array().is_some_and(|c| !c.is_empty()) {
            notes.push("dry run: call again without `dry_run` to apply the changes".to_owned());
        }
        if result["changes"].as_array().is_some_and(Vec::is_empty) {
            notes.push("the draft already matches the listing".to_owned());
        }
        if !notes.is_empty() {
            result.insert("note".into(), json!(notes.join("; ")));
        }
        Ok(Value::Object(result))
    })
}

// --- sync_screenshots --------------------------------------------------------------------

/// Формат изображения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Png,
    Jpeg,
}

impl ImageFormat {
    /// Формат по первым байтам файла.
    fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
        }
    }
}

/// Размеры картинки в пикселях из заголовка PNG (`IHDR`) или JPEG (маркер `SOFn`).
fn image_size(format: ImageFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    match format {
        ImageFormat::Png => {
            let ihdr = bytes.get(12..24)?;
            if &ihdr[..4] != b"IHDR" {
                return None;
            }
            let width = u32::from_be_bytes(ihdr[4..8].try_into().ok()?);
            let height = u32::from_be_bytes(ihdr[8..12].try_into().ok()?);
            Some((width, height))
        }
        ImageFormat::Jpeg => {
            let mut pos = 2;
            while pos + 4 <= bytes.len() {
                if bytes[pos] != 0xFF {
                    return None;
                }
                let marker = bytes[pos + 1];
                if marker == 0xFF {
                    pos += 1;
                    continue;
                }
                // Маркеры без длины.
                if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
                    pos += 2;
                    continue;
                }
                let len = usize::from(u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]));
                let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
                if is_sof {
                    let segment = bytes.get(pos + 4..pos + 9)?;
                    let height = u32::from(u16::from_be_bytes([segment[1], segment[2]]));
                    let width = u32::from(u16::from_be_bytes([segment[3], segment[4]]));
                    return Some((width, height));
                }
                pos += 2 + len;
            }
            None
        }
    }
}

/// Проверяет скриншот по ограничениям справочника: формат по содержимому и расширению,
/// стороны, ориентация каталога. Возвращает размеры и предупреждение о соотношении сторон.
fn check_screenshot(
    rel: &str,
    ext: &str,
    bytes: &[u8],
    orientation: &str,
) -> Result<((u32, u32), Option<String>), String> {
    let format = ImageFormat::sniff(bytes).ok_or_else(|| format!("{rel}: not a PNG or JPEG"))?;
    let ext_ok = match format {
        ImageFormat::Png => ext == "png",
        ImageFormat::Jpeg => ext == "jpg" || ext == "jpeg",
    };
    if !ext_ok {
        return Err(format!(
            "{rel}: the file is a {} image, but its extension is .{ext}",
            format.name()
        ));
    }
    let (width, height) =
        image_size(format, bytes).ok_or_else(|| format!("{rel}: cannot read the image size"))?;
    let (short, long) = (width.min(height), width.max(height));
    if short < MIN_SIDE || long > MAX_SIDE {
        return Err(format!(
            "{rel}: {width} x {height} px; sides must be from {MIN_SIDE} to {MAX_SIDE} px"
        ));
    }
    let fits = match orientation {
        "PORTRAIT" => height > width,
        _ => width > height,
    };
    if !fits {
        return Err(format!(
            "{rel}: {width} x {height} px is not a {} image",
            orientation.to_lowercase()
        ));
    }
    // 16:9 с допуском 1 %: RuStore при другом соотношении только предупреждает.
    let (long, short) = (u64::from(long), u64::from(short));
    let warning = ((long * 9).abs_diff(short * 16) * 100 > short * 16).then(|| {
        format!(
            "{rel}: {width} x {height} px is not {}; RuStore accepts it with a warning",
            if orientation == "PORTRAIT" {
                "9:16"
            } else {
                "16:9"
            }
        )
    });
    Ok(((width, height), warning))
}

/// Локальный скриншот после проверки.
#[derive(Debug, Clone)]
struct LocalShot {
    kind: &'static str,
    orientation: &'static str,
    ordinal: u64,
    /// Канонический путь (проверен [`crate::files::FilePolicy::check_read`]).
    path: PathBuf,
    /// Путь относительно `dir` для ответа.
    rel: String,
    bytes: u64,
    width: u32,
    height: u32,
}

impl LocalShot {
    fn describe(&self) -> Value {
        json!({
            "type": self.kind, "orientation": self.orientation, "ordinal": self.ordinal,
            "file": self.rel, "bytes": self.bytes, "width": self.width, "height": self.height
        })
    }
}

/// Элементы каталога: имя, путь, каталог ли (по ссылке), по имени.
fn dir_entries(dir: &Path) -> Result<Vec<(String, PathBuf, bool)>, ToolError> {
    let read = std::fs::read_dir(dir)
        .map_err(|err| ToolError::new(format!("cannot read `{}`: {err}", dir.display())))?;
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry
            .map_err(|err| ToolError::new(format!("cannot read `{}`: {err}", dir.display())))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = path.is_dir();
        entries.push((name, path, is_dir));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Каталоги скриншотов: тип, ориентация, канонический путь, путь для ответа.
type ShotDir = (&'static str, &'static str, PathBuf, String);

/// Каталоги ориентаций внутри `dir` (`portrait`, `landscape`).
fn orientation_dirs(
    ctx: &Context,
    kind: &'static str,
    dir: &Path,
    rel: &str,
    warnings: &mut Vec<String>,
) -> Result<Vec<ShotDir>, ToolError> {
    let mut out = Vec::new();
    for (name, path, is_dir) in dir_entries(dir)? {
        if name.starts_with('.') {
            continue;
        }
        let rel_path = format!("{rel}{name}");
        match ORIENTATIONS.iter().find(|(dir, _)| *dir == name) {
            Some((_, orientation)) if is_dir => {
                let path = ctx
                    .files()
                    .check_dir(path_text(&path)?)
                    .map_err(file_error)?;
                out.push((kind, *orientation, path, rel_path));
            }
            _ => warnings.push(format!(
                "ignored `{rel_path}`: expected the directories portrait/ and landscape/"
            )),
        }
    }
    Ok(out)
}

/// Читает и проверяет локальные скриншоты. Ошибки всех файлов — одной ошибкой, до запросов.
fn scan_screenshots(ctx: &Context, dir: &str) -> Result<(Vec<LocalShot>, Vec<String>), ToolError> {
    let files = ctx.files();
    let mut root = files.check_dir(dir).map_err(file_error)?;
    let mut prefix = String::new();
    let nested = root.join(SCREENSHOTS_DIR);
    if nested.is_dir() {
        root = files.check_dir(path_text(&nested)?).map_err(file_error)?;
        prefix = format!("{SCREENSHOTS_DIR}/");
    }

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let mut shot_dirs: Vec<ShotDir> = Vec::new();
    let mut phone_dir = false;
    let mut shortcut = false;
    for (name, path, is_dir) in dir_entries(&root)? {
        if name.starts_with('.') {
            continue;
        }
        let rel = format!("{prefix}{name}");
        if !is_dir {
            warnings.push(format!(
                "ignored `{rel}`: expected the directories phone/, tablet/, tv/ (or portrait/ \
                 and landscape/ for phone)"
            ));
            continue;
        }
        if let Some((_, kind)) = DEVICES.iter().find(|(dir, _)| *dir == name) {
            phone_dir |= *kind == DEFAULT_TYPE;
            let path = files.check_dir(path_text(&path)?).map_err(file_error)?;
            shot_dirs.extend(orientation_dirs(
                ctx,
                kind,
                &path,
                &format!("{rel}/"),
                &mut warnings,
            )?);
        } else if let Some((_, orientation)) = ORIENTATIONS.iter().find(|(dir, _)| *dir == name) {
            shortcut = true;
            let path = files.check_dir(path_text(&path)?).map_err(file_error)?;
            shot_dirs.push((DEFAULT_TYPE, orientation, path, rel));
        } else {
            warnings.push(format!(
                "ignored `{rel}`: expected the directories phone/, tablet/, tv/ (or portrait/ \
                 and landscape/ for phone)"
            ));
        }
    }
    if phone_dir && shortcut {
        errors.push(
            "use either phone/ or the top-level portrait/ and landscape/ for phone screenshots, \
             not both"
                .to_owned(),
        );
    }

    let mut shots: Vec<LocalShot> = Vec::new();
    for (kind, orientation, path, rel) in shot_dirs {
        for (name, file, is_dir) in dir_entries(&path)? {
            if name.starts_with('.') {
                continue;
            }
            let rel = format!("{rel}/{name}");
            let (stem, ext) = match name.rsplit_once('.') {
                Some((stem, ext)) => (stem, ext.to_ascii_lowercase()),
                None => (name.as_str(), String::new()),
            };
            if is_dir || !matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
                warnings.push(format!("ignored `{rel}`: not a .png, .jpg or .jpeg file"));
                continue;
            }
            let ordinal = match stem.parse::<u64>() {
                Ok(n) if n <= MAX_ORDINAL && !stem.starts_with('+') => n,
                _ => {
                    errors.push(format!(
                        "{rel}: the file name must be the position from 0 to {MAX_ORDINAL}, \
                         like 0.png"
                    ));
                    continue;
                }
            };
            match check_local_file(ctx, &file, &rel, &ext, orientation) {
                Ok(checked) => {
                    warnings.extend(checked.warning);
                    shots.push(LocalShot {
                        kind,
                        orientation,
                        ordinal,
                        path: checked.path,
                        rel,
                        bytes: checked.bytes,
                        width: checked.width,
                        height: checked.height,
                    });
                }
                Err(err) => errors.push(err),
            }
        }
    }

    // Одно место — один файл; один тип — одна ориентация.
    let mut slots: BTreeMap<(&str, u64), &str> = BTreeMap::new();
    let mut orientations: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for shot in &shots {
        if let Some(other) = slots.insert((shot.kind, shot.ordinal), &shot.rel) {
            errors.push(format!(
                "{} and {other}: two files for {} position {}",
                shot.rel, shot.kind, shot.ordinal
            ));
        }
        orientations
            .entry(shot.kind)
            .or_default()
            .insert(shot.orientation);
    }
    for (kind, set) in &orientations {
        if set.len() > 1 {
            errors.push(format!(
                "{kind}: both portrait and landscape files; RuStore does not allow screenshots \
                 of different orientations"
            ));
        }
    }
    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "local screenshots are invalid, nothing was changed: {}",
            errors.join("; ")
        )));
    }
    if shots.is_empty() {
        return Err(ToolError::new(format!(
            "no screenshots found in `{dir}`; expected files like phone/portrait/0.png \
             (<device>/<orientation>/<ordinal>.png, device phone, tablet or tv)"
        )));
    }
    shots.sort_by(|a, b| (a.kind, a.ordinal).cmp(&(b.kind, b.ordinal)));
    Ok((shots, warnings))
}

/// Проверенный локальный файл.
struct CheckedFile {
    /// Канонический путь.
    path: PathBuf,
    bytes: u64,
    width: u32,
    height: u32,
    /// Предупреждение о соотношении сторон.
    warning: Option<String>,
}

/// Проверяет один файл: [`crate::files::FilePolicy`], размер, содержимое.
fn check_local_file(
    ctx: &Context,
    file: &Path,
    rel: &str,
    ext: &str,
    orientation: &str,
) -> Result<CheckedFile, String> {
    let path = ctx
        .files()
        .check_read(file.to_str().ok_or_else(|| format!("{rel}: unsupported path"))?)
        .map_err(|err| err.0)?;
    let bytes = std::fs::metadata(&path)
        .map_err(|err| format!("{rel}: {err}"))?
        .len();
    if bytes > MAX_SCREENSHOT_BYTES {
        return Err(format!(
            "{rel}: {bytes} bytes is more than 5 MB (the RuStore limit)"
        ));
    }
    let content = std::fs::read(&path).map_err(|err| format!("{rel}: {err}"))?;
    let ((width, height), warning) = check_screenshot(rel, ext, &content, orientation)?;
    Ok(CheckedFile {
        path,
        bytes,
        width,
        height,
        warning,
    })
}

/// Скриншот черновика.
#[derive(Debug, Clone)]
struct RemoteShot {
    id: u64,
    kind: String,
    orientation: String,
    ordinal: Option<u64>,
}

impl RemoteShot {
    fn from_api(item: &Value) -> Option<Self> {
        Some(Self {
            id: id_of(item, "id")?,
            kind: shot_type(item),
            orientation: text_field(item, "orientation").unwrap_or_default(),
            ordinal: id_of(item, "ordinal"),
        })
    }

    fn describe(&self, reason: Option<&str>) -> Value {
        let mut value = json!({
            "id": self.id, "type": self.kind, "orientation": self.orientation,
            "ordinal": self.ordinal
        });
        if let (Some(reason), Value::Object(map)) = (reason, &mut value) {
            map.insert("reason".into(), json!(reason));
        }
        value
    }
}

/// План синхронизации.
#[derive(Debug, Default)]
struct SyncPlan {
    /// Удаления другой ориентации: до загрузок (иначе RuStore отклонит загрузку).
    first_deletes: Vec<RemoteShot>,
    /// Загрузки: индекс локального файла и скриншоты, которые он заменяет на том же месте.
    uploads: Vec<(usize, Vec<u64>)>,
    /// Удаления лишних: после загрузок.
    last_deletes: Vec<RemoteShot>,
    skipped: Vec<usize>,
    kept: Vec<RemoteShot>,
    untouched: BTreeMap<String, usize>,
    warnings: Vec<String>,
}

const REASON_ORIENTATION: &str = "other orientation";
const REASON_EXTRA: &str = "no local file";

/// Строит план по локальным и удалённым скриншотам; конфликт ориентаций без
/// `delete_extra` — ошибка.
fn plan_sync(
    local: &[LocalShot],
    remote: &[RemoteShot],
    delete_extra: bool,
    skip_existing: bool,
) -> Result<SyncPlan, ToolError> {
    let mut plan = SyncPlan::default();
    let kinds: BTreeSet<&str> = local.iter().map(|shot| shot.kind).collect();
    let mut errors = Vec::new();
    for remote_shot in remote {
        if !kinds.contains(remote_shot.kind.as_str()) {
            *plan.untouched.entry(remote_shot.kind.clone()).or_default() += 1;
        }
    }
    for kind in &kinds {
        let locals: Vec<(usize, &LocalShot)> = local
            .iter()
            .enumerate()
            .filter(|(_, shot)| shot.kind == *kind)
            .collect();
        let orientation = locals.first().map(|(_, shot)| shot.orientation).unwrap_or("");
        let remotes: Vec<&RemoteShot> = remote.iter().filter(|shot| shot.kind == *kind).collect();
        let other: Vec<&RemoteShot> = remotes
            .iter()
            .copied()
            .filter(|shot| shot.orientation != orientation)
            .collect();
        if !other.is_empty() {
            if delete_extra {
                plan.first_deletes.extend(other.iter().map(|s| (*s).clone()));
            } else {
                errors.push(format!(
                    "the draft has {} {kind} screenshot(s) of another orientation than the local \
                     {} files; RuStore does not allow mixing orientations, pass `delete_extra: \
                     true` to replace them",
                    other.len(),
                    orientation.to_lowercase()
                ));
            }
        }
        let same: Vec<&RemoteShot> = remotes
            .iter()
            .copied()
            .filter(|shot| shot.orientation == orientation)
            .collect();
        let mut total = 0;
        for (index, shot) in &locals {
            let existing: Vec<u64> = same
                .iter()
                .filter(|remote| remote.ordinal == Some(shot.ordinal))
                .map(|remote| remote.id)
                .collect();
            total += 1;
            if skip_existing && !existing.is_empty() {
                plan.skipped.push(*index);
            } else {
                plan.uploads.push((*index, existing));
            }
        }
        for remote_shot in same {
            let has_local = locals
                .iter()
                .any(|(_, shot)| Some(shot.ordinal) == remote_shot.ordinal);
            if has_local {
                continue;
            }
            if delete_extra {
                plan.last_deletes.push(remote_shot.clone());
            } else {
                total += 1;
                plan.kept.push(remote_shot.clone());
            }
        }
        if *kind == DEFAULT_TYPE && total < MIN_FOR_PUBLICATION {
            plan.warnings.push(format!(
                "{kind}: {total} screenshot(s) after the sync; RuStore needs at least \
                 {MIN_FOR_PUBLICATION} to publish"
            ));
        }
    }
    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "{}; nothing was changed",
            errors.join("; ")
        )));
    }
    Ok(plan)
}

fn sync_screenshots<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let version_id = optional_u64(&args, "versionId")?;
        let dir = required_str(&args, "dir")?;
        let delete_extra = bool_arg(&args, "delete_extra", false)?;
        let skip_existing = bool_arg(&args, "skip_existing", false)?;
        let dry_run = bool_arg(&args, "dry_run", false)?;
        // Файлы проверяются до запросов.
        let (local, mut warnings) = scan_screenshots(ctx, dir)?;

        let (id, _) = draft_version(ctx, &package, version_id).await?;
        let remote: Vec<RemoteShot> = list_screenshots(ctx, &package, id)
            .await?
            .iter()
            .filter_map(RemoteShot::from_api)
            .collect();
        let plan = plan_sync(&local, &remote, delete_extra, skip_existing)?;
        warnings.extend(plan.warnings.iter().cloned());

        let mut uploads: Vec<Value> = plan
            .uploads
            .iter()
            .map(|(index, replaces)| {
                let mut entry = local[*index].describe();
                if !replaces.is_empty()
                    && let Value::Object(map) = &mut entry
                {
                    map.insert("replaces".into(), json!(replaces));
                }
                entry
            })
            .collect();
        let deletions: Vec<Value> = plan
            .first_deletes
            .iter()
            .map(|shot| shot.describe(Some(REASON_ORIENTATION)))
            .chain(
                plan.last_deletes
                    .iter()
                    .map(|shot| shot.describe(Some(REASON_EXTRA))),
            )
            .collect();

        let mut done_uploads = 0;
        let mut done_deletes = 0;
        if !dry_run {
            let progress = |err: ToolError, uploads: usize, deletes: usize| {
                ToolError::new(format!(
                    "{}; done before the failure: {uploads} upload(s), {deletes} deletion(s). \
                     Call again with `dry_run: true` to see what is left",
                    err.message()
                ))
            };
            let delete = |shot: &RemoteShot| {
                ctx.call(
                    "delete_screenshot",
                    json!({"packageName": package, "versionId": id, "id": shot.id}),
                )
            };
            for shot in &plan.first_deletes {
                delete(shot)
                    .await
                    .map_err(|err| progress(err, done_uploads, done_deletes))?;
                done_deletes += 1;
            }
            for (position, (index, _)) in plan.uploads.iter().enumerate() {
                let shot = &local[*index];
                let response = ctx
                    .call(
                        "upload_screenshot_v2",
                        json!({
                            "packageName": package,
                            "versionId": id,
                            "orientation": shot.orientation,
                            "ordinal": shot.ordinal,
                            "type": shot.kind,
                            "file": path_text(&shot.path)?,
                        }),
                    )
                    .await
                    .map_err(|err| progress(err, done_uploads, done_deletes))?;
                done_uploads += 1;
                if let (Some(new_id), Some(Value::Object(map))) =
                    (id_of(&response, "id"), uploads.get_mut(position))
                {
                    map.insert("id".into(), json!(new_id));
                }
            }
            for shot in &plan.last_deletes {
                delete(shot)
                    .await
                    .map_err(|err| progress(err, done_uploads, done_deletes))?;
                done_deletes += 1;
            }
        }

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("versionId".into(), json!(id));
        result.insert("dry_run".into(), json!(dry_run));
        result.insert("uploads".into(), Value::Array(uploads));
        result.insert("deletions".into(), Value::Array(deletions));
        result.insert(
            "skipped".into(),
            json!(
                plan.skipped
                    .iter()
                    .map(|index| local[*index].describe())
                    .collect::<Vec<_>>()
            ),
        );
        if !plan.kept.is_empty() {
            result.insert(
                "kept".into(),
                json!(
                    plan.kept
                        .iter()
                        .map(|shot| shot.describe(None))
                        .collect::<Vec<_>>()
                ),
            );
        }
        if !plan.untouched.is_empty() {
            result.insert("untouched".into(), json!(plan.untouched));
        }
        if !dry_run {
            result.insert("uploaded".into(), json!(done_uploads));
            result.insert("deleted".into(), json!(done_deletes));
        }
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        let mut notes = Vec::new();
        if !plan.kept.is_empty() {
            notes.push(
                "`kept`: screenshots of the draft without a local file; pass `delete_extra: \
                 true` to delete them"
                    .to_owned(),
            );
        }
        if dry_run {
            notes.push("dry run: nothing was changed".to_owned());
        }
        if !notes.is_empty() {
            result.insert("note".into(), json!(notes.join("; ")));
        }
        Ok(Value::Object(result))
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wiremock::matchers::{any, body_json, header_regex, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::composite::testing;
    use crate::files::FilePolicy;

    const PKG: &str = "com.example.app";
    const VERSIONS: &str = "/public/v1/application/com.example.app/version";

    fn version_path(id: u64) -> String {
        format!("/public/v2/application/{PKG}/version/{id}")
    }

    fn screenshots_path(id: u64) -> String {
        format!("{}/image/screenshot", version_path(id))
    }

    fn envelope(body: Value) -> Value {
        json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
    }

    fn ok(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(envelope(body))
    }

    fn page(content: Value) -> Value {
        let total = content.as_array().map_or(0, Vec::len);
        json!({"content": content, "pageNumber": 0, "pageSize": 50, "totalElements": total,
               "totalPages": 1})
    }

    /// Каталог во временной папке и политика файлов только с ним.
    struct Files {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        policy: FilePolicy,
    }

    fn files() -> Files {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        Files {
            _tmp: tmp,
            root,
            policy,
        }
    }

    fn ctx(server: &MockServer, access: AccessLevel, policy: FilePolicy) -> Context {
        testing::context(&server.uri(), access, &[PKG], policy).unwrap()
    }

    async fn run(name: &str, ctx: &Context, args: Value) -> Result<Value, ToolError> {
        let tool = TOOLS.iter().find(|tool| tool.name == name).unwrap();
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        super::super::check_arguments(&(tool.input_schema)(), &args)?;
        (tool.run)(ctx, args).await
    }

    /// Ни одного запроса к API: проверки до запросов.
    async fn no_requests() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        server
    }

    fn s(path: &Path) -> String {
        path.display().to_string()
    }

    /// PNG с заголовком `IHDR` размера `width` x `height` и меткой `tag`.
    fn png(width: u32, height: u32, tag: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0, tag]);
        bytes
    }

    /// JPEG с маркером `SOF0` размера `width` x `height`.
    fn jpeg(width: u16, height: u16, tag: u8) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, tag];
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xD9]);
        bytes
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn published_version() -> Value {
        json!({
            "appId": 1, "versionId": 7, "versionName": "1.7", "versionStatus": "ACTIVE",
            "appType": "MAIN", "appName": "Example", "categories": ["news", "education"],
            "ageLegal": "6+", "shortDescription": "Short text",
            "fullDescription": "Line one\nLine two", "whatsNew": "Fixes",
            "developerContacts": {"email": "dev@example.invalid", "phone": "ignored"},
            "faq": [{"id": 3, "question": "Why?", "answer": "Because."}],
            "seoTagIds": [100, 102], "priceValue": 0, "paid": false,
            "iconUid": "icon-1", "iconUrl": "https://example.invalid/icon.png",
            "fileUids": [
                {"fileUid": "apk", "type": "APK"},
                {"fileUid": "s-0", "type": "SCREENSHOT", "orientation": "PORTRAIT",
                 "ordinal": 0, "fileUrl": "https://example.invalid/s0.jpg"}
            ]
        })
    }

    fn remote_shot(id: u64, kind: &str, orientation: &str, ordinal: u64) -> Value {
        json!({"id": id, "type": kind, "orientation": orientation, "ordinal": ordinal,
               "fileUid": format!("uid-{id}"), "fileUrl": format!("https://example.invalid/{id}.png")})
    }

    async fn mount_published(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(VERSIONS))
            .and(query_param("versionStatuses", "ACTIVE,PARTIAL_ACTIVE"))
            .respond_with(ok(page(json!([
                {"versionId": 5, "versionStatus": "ACTIVE"},
                {"versionId": 7, "versionStatus": "PARTIAL_ACTIVE"}
            ]))))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(version_path(7)))
            .respond_with(ok(published_version()))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(screenshots_path(7)))
            .respond_with(ok(page(json!([
                remote_shot(21, "TABLET_SCREENSHOT", "LANDSCAPE", 0),
                remote_shot(12, "SCREENSHOT", "PORTRAIT", 1),
                remote_shot(11, "SCREENSHOT", "PORTRAIT", 0)
            ]))))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn export_inline_reads_the_published_version() {
        let server = MockServer::start().await;
        mount_published(&server).await;
        let ctx = ctx(&server, AccessLevel::Read, FilePolicy::disabled());

        let out = run("listing_export", &ctx, json!({})).await.unwrap();
        assert_eq!(out["version"]["versionId"], 7);
        let listing = &out["listing"];
        assert_eq!(listing["appName"], "Example");
        assert_eq!(listing["fullDescription"], "Line one\nLine two");
        // В виде запроса: у faq нет id, у контактов — только поля запроса.
        assert_eq!(
            listing["faq"],
            json!([{"question": "Why?", "answer": "Because."}])
        );
        assert_eq!(
            listing["developerContacts"],
            json!({"email": "dev@example.invalid"})
        );
        // Цена — не поле карточки.
        assert!(listing.get("priceValue").is_none());
        let images = &out["images"];
        assert_eq!(images["icon"]["iconUid"], "icon-1");
        assert_eq!(images["screenshots_source"], "list_screenshots");
        let shots = images["screenshots"].as_array().unwrap();
        let paths: Vec<&str> = shots
            .iter()
            .map(|shot| shot["sync_path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "screenshots/phone/portrait/0.png",
                "screenshots/phone/portrait/1.png",
                "screenshots/tablet/landscape/0.png"
            ]
        );
        assert_eq!(shots[0]["id"], 11);
        assert!(out["note"].as_str().unwrap().contains("not downloaded"));
    }

    #[tokio::test]
    async fn export_falls_back_to_version_files() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(VERSIONS))
            .and(query_param("versionStatuses", "DRAFT"))
            .and(query_param("filterTestingType", "ALL"))
            .respond_with(ok(page(json!([{"versionId": 7, "versionStatus": "DRAFT"}]))))
            .expect(1)
            .mount(&server)
            .await;
        let mut draft = published_version();
        draft["versionStatus"] = json!("DRAFT");
        Mock::given(method("GET"))
            .and(path(version_path(7)))
            .respond_with(ok(draft))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(screenshots_path(7)))
            .respond_with(ResponseTemplate::new(404).set_body_json(
                json!({"code": "NOT_FOUND", "message": "Screenshots not found", "body": null}),
            ))
            .mount(&server)
            .await;
        let ctx = ctx(&server, AccessLevel::Read, FilePolicy::disabled());

        let out = run("listing_export", &ctx, json!({"draft": true}))
            .await
            .unwrap();
        assert_eq!(out["version"]["versionStatus"], "DRAFT");
        assert_eq!(out["images"]["screenshots_source"], "get_version");
        let shots = out["images"]["screenshots"].as_array().unwrap();
        assert_eq!(shots.len(), 1, "{shots:?}");
        assert_eq!(shots[0]["sync_path"], "screenshots/phone/portrait/0.jpg");
        assert!(
            out["warnings"][0]
                .as_str()
                .unwrap()
                .contains("Screenshots not found")
        );

        let err = run("listing_export", &ctx, json!({"draft": true, "versionId": 7}))
            .await
            .unwrap_err();
        assert!(err.message().contains("either `versionId` or `draft`"), "{err}");
    }

    #[tokio::test]
    async fn export_writes_files_and_never_overwrites() {
        let server = MockServer::start().await;
        mount_published(&server).await;
        let f = files();
        let ctx = ctx(&server, AccessLevel::Read, f.policy.clone());
        let dir = f.root.join("listing");

        let out = run("listing_export", &ctx, json!({"dir": s(&dir)}))
            .await
            .unwrap();
        assert_eq!(out["files"], json!(["listing.json", "images.json"]));
        assert_eq!(out["screenshots"], 3);
        assert!(out.get("listing").is_none());
        let listing: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("listing.json")).unwrap())
                .unwrap();
        assert_eq!(listing["packageName"], PKG);
        assert_eq!(listing["versionId"], 7);
        assert_eq!(listing["listing"]["shortDescription"], "Short text");
        let images: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("images.json")).unwrap())
                .unwrap();
        assert_eq!(images["screenshots"].as_array().unwrap().len(), 3);

        // Повторный экспорт в тот же каталог: ошибка до запросов, файлы не тронуты
        // (wiremock проверяет, что каждый запрос был один раз).
        std::fs::write(dir.join("listing.json"), "edited").unwrap();
        let err = run("listing_export", &ctx, json!({"dir": s(&dir)}))
            .await
            .unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.join("listing.json")).unwrap(),
            "edited"
        );

        // Вне разрешённых каталогов и без родителя — ошибка до запросов.
        let other = tempfile::tempdir().unwrap();
        let err = run(
            "listing_export",
            &ctx,
            json!({"dir": s(&other.path().join("out"))}),
        )
        .await
        .unwrap_err();
        assert!(
            err.message().contains("outside the allowed directories"),
            "{err}"
        );
        let err = run(
            "listing_export",
            &ctx,
            json!({"dir": s(&f.root.join("missing/out"))}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("does not exist"), "{err}");
    }

    fn draft_body() -> Value {
        json!({
            "versionId": 9, "versionStatus": "DRAFT", "appName": "Example",
            "categories": ["news", "education"], "shortDescription": "Old short",
            "fullDescription": "Line one\nLine two\nLine three",
            "faq": [{"id": 1, "question": "Why?", "answer": "Because."}],
            "developerContacts": {"email": "dev@example.invalid"}
        })
    }

    async fn mount_draft(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(VERSIONS))
            .and(query_param("versionStatuses", "DRAFT"))
            .and(query_param("filterTestingType", "ALL"))
            .respond_with(ok(page(json!([{"versionId": 9, "versionStatus": "DRAFT"}]))))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(version_path(9)))
            .respond_with(ok(draft_body()))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn apply_dry_run_shows_the_difference() {
        let server = MockServer::start().await;
        mount_draft(&server).await;
        Mock::given(method("PATCH"))
            .respond_with(ok(json!(9)))
            .expect(0)
            .mount(&server)
            .await;
        let f = files();
        write(
            &f.root,
            "listing/listing.json",
            json!({
                "packageName": PKG, "versionId": 7,
                "listing": {
                    "appName": "Example",
                    // Тот же набор категорий в другом порядке — без изменений.
                    "categories": ["education", "news"],
                    "shortDescription": "New short",
                    "fullDescription": "Line one\nLine 2\nLine three",
                    "faq": [{"question": "Why?", "answer": "Because."}],
                    "whatsNew": null
                }
            })
            .to_string()
            .as_bytes(),
        );
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let out = run(
            "listing_apply",
            &ctx,
            json!({"dir": s(&f.root.join("listing")), "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["versionId"], 9);
        assert_eq!(out["applied"], false);
        let changes = out["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!(
            changes[0],
            json!({"field": "shortDescription", "current": "Old short", "new": "New short"})
        );
        assert_eq!(changes[1]["field"], "fullDescription");
        assert_eq!(changes[1]["lines"], json!(["- Line two", "+ Line 2"]));
        let mut unchanged: Vec<&str> = out["unchanged"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        unchanged.sort_unstable();
        assert_eq!(unchanged, ["appName", "categories", "faq"]);
        assert!(out["note"].as_str().unwrap().contains("dry run"));
    }

    #[tokio::test]
    async fn apply_sends_only_changed_fields() {
        let server = MockServer::start().await;
        mount_draft(&server).await;
        Mock::given(method("PATCH"))
            .and(path(version_path(9)))
            .and(body_json(json!({"shortDescription": "New short", "moderInfo": "Test login: demo"})))
            .respond_with(ok(json!(9)))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = ctx(&server, AccessLevel::Write, FilePolicy::disabled());

        let out = run(
            "listing_apply",
            &ctx,
            json!({
                "listing": {
                    "appName": "Example", "shortDescription": "New short",
                    "fullDescription": "Not applied", "moderInfo": "Test login: demo"
                },
                "fields": ["appName", "shortDescription", "moderInfo"]
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["applied"], true);
        assert_eq!(out["unchanged"], json!(["appName"]));
        assert!(out["note"].as_str().unwrap().contains("moderInfo"));

        // Всё совпадает — запроса нет.
        let out = run(
            "listing_apply",
            &ctx,
            json!({"listing": {"appName": "Example"}}),
        )
        .await
        .unwrap();
        assert_eq!(out["applied"], false);
        assert!(out["note"].as_str().unwrap().contains("already matches"));
    }

    #[tokio::test]
    async fn apply_checks_the_listing_before_requests() {
        let server = no_requests().await;
        let f = files();
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());
        let apply = |listing: Value| run("listing_apply", &ctx, json!({"listing": listing}));

        let err = apply(json!({
            "shortDescription": "x".repeat(81),
            "categories": ["a", "b", "c"],
            "ageLegal": "21+",
            "faq": [{"question": "Q", "answer": ""}],
            "developerContacts": {"phone": "1"},
            "seoTagIds": [1, 2, 3, 4, 5, 6],
            "whatsNewTv": ""
        }))
        .await
        .unwrap_err();
        let message = err.message();
        for part in [
            "nothing was changed",
            "`shortDescription` must be at most 80 characters, got 81",
            "`categories` must have at most 2 items",
            "`ageLegal` must be one of",
            "`faq` item 0: `answer` must be from 1 to 500",
            "`developerContacts` has unknown field `phone`",
            "`seoTagIds` must have at most 5 items",
            "`whatsNewTv` must be from 1 to 5000",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
        // Кириллица считается символами, а не байтами.
        let err = apply(json!({"appName": "я".repeat(51)})).await.unwrap_err();
        assert!(err.message().contains("got 51"), "{err}");

        let err = apply(json!({"title": "x"})).await.unwrap_err();
        assert!(err.message().contains("`title` is not a listing field"), "{err}");
        let err = run(
            "listing_apply",
            &ctx,
            json!({"listing": {"appName": "x"}, "fields": ["title"]}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("`title` is not a listing field"), "{err}");
        let err = apply(json!({"whatsNew": null})).await.unwrap_err();
        assert!(err.message().contains("nothing to apply"), "{err}");

        let err = run("listing_apply", &ctx, json!({})).await.unwrap_err();
        assert!(err.message().contains("pass `dir`"), "{err}");
        let err = run(
            "listing_apply",
            &ctx,
            json!({"dir": s(&f.root), "listing": {}}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("not both"), "{err}");

        // Каталог без listing.json, файл без `listing`, каталог вне разрешённых.
        let err = run("listing_apply", &ctx, json!({"dir": s(&f.root)}))
            .await
            .unwrap_err();
        assert!(err.message().contains("does not exist"), "{err}");
        write(&f.root, "bad/listing.json", br#"{"appName": "x"}"#);
        let err = run("listing_apply", &ctx, json!({"dir": s(&f.root.join("bad"))}))
            .await
            .unwrap_err();
        assert!(err.message().contains("no `listing` object"), "{err}");
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other.path().join("listing.json"),
            br#"{"listing": {"appName": "x"}}"#,
        )
        .unwrap();
        let err = run("listing_apply", &ctx, json!({"dir": s(other.path())}))
            .await
            .unwrap_err();
        assert!(
            err.message().contains("outside the allowed directories"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn apply_needs_a_draft() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(VERSIONS))
            .respond_with(ok(page(json!([]))))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(version_path(7)))
            .respond_with(ok(published_version()))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .respond_with(ok(json!(7)))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = ctx(&server, AccessLevel::Write, FilePolicy::disabled());

        let err = run(
            "listing_apply",
            &ctx,
            json!({"listing": {"appName": "New"}}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("has no draft version"), "{err}");
        assert!(err.message().contains("create_draft_version"), "{err}");

        let err = run(
            "listing_apply",
            &ctx,
            json!({"listing": {"appName": "New"}, "versionId": 7}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("version 7 is ACTIVE, not a draft"), "{err}");
    }

    // --- sync_screenshots ---

    /// Загрузка скриншота: multipart с полем `file`, именем файла и содержимым.
    async fn expect_upload(
        server: &MockServer,
        route: &str,
        file_name: &'static str,
        content: Vec<u8>,
        new_id: u64,
    ) {
        Mock::given(method("POST"))
            .and(path(format!("{}/{route}", screenshots_path(9))))
            .and(header_regex("content-type", "^multipart/form-data; boundary="))
            .and(move |request: &Request| {
                let body = &request.body;
                let text = String::from_utf8_lossy(body);
                text.contains("name=\"file\"")
                    && text.contains(&format!("filename=\"{file_name}\""))
                    && body.windows(content.len()).any(|w| w == content.as_slice())
            })
            .respond_with(ok(json!({"id": new_id})))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_remote(server: &MockServer, shots: Value) {
        mount_draft(server).await;
        Mock::given(method("GET"))
            .and(path(screenshots_path(9)))
            .respond_with(ok(page(shots)))
            .mount(server)
            .await;
    }

    fn phone_set(root: &Path) {
        write(root, "shots/phone/portrait/0.png", &png(1080, 1920, 1));
        write(root, "shots/phone/portrait/1.png", &png(1080, 1920, 2));
        write(root, "shots/phone/portrait/2.jpg", &jpeg(1080, 1920, 3));
        write(root, "shots/tablet/landscape/0.png", &png(2560, 1440, 4));
        write(root, "shots/.DS_Store", b"junk");
        write(root, "shots/README.txt", b"notes");
    }

    #[tokio::test]
    async fn sync_dry_run_plans_uploads_and_keeps_extras() {
        let server = MockServer::start().await;
        mount_remote(
            &server,
            json!([
                remote_shot(10, "SCREENSHOT", "PORTRAIT", 0),
                remote_shot(11, "SCREENSHOT", "PORTRAIT", 5),
                remote_shot(12, "TV_SCREENSHOT", "LANDSCAPE", 0)
            ]),
        )
        .await;
        for verb in ["POST", "DELETE"] {
            Mock::given(method(verb))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&server)
                .await;
        }
        let f = files();
        phone_set(&f.root);
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let out = run(
            "sync_screenshots",
            &ctx,
            json!({"dir": s(&f.root.join("shots")), "dry_run": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["versionId"], 9);
        let uploads = out["uploads"].as_array().unwrap();
        assert_eq!(uploads.len(), 4, "{uploads:?}");
        assert_eq!(uploads[0]["file"], "phone/portrait/0.png");
        assert_eq!(uploads[0]["replaces"], json!([10]));
        assert_eq!(uploads[0]["width"], 1080);
        assert_eq!(uploads[2]["file"], "phone/portrait/2.jpg");
        assert_eq!(uploads[3]["type"], "TABLET_SCREENSHOT");
        assert_eq!(out["deletions"], json!([]));
        assert_eq!(out["kept"][0]["id"], 11);
        assert_eq!(out["untouched"], json!({"TV_SCREENSHOT": 1}));
        let warnings = out["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap().contains("ignored `README.txt`")),
            "{warnings:?}"
        );
        assert!(out.get("uploaded").is_none());
        assert!(out["note"].as_str().unwrap().contains("delete_extra"));
    }

    #[tokio::test]
    async fn sync_uploads_multipart_and_deletes_extras() {
        let server = MockServer::start().await;
        mount_remote(
            &server,
            json!([
                remote_shot(10, "SCREENSHOT", "PORTRAIT", 0),
                remote_shot(11, "SCREENSHOT", "PORTRAIT", 5),
                remote_shot(13, "TABLET_SCREENSHOT", "PORTRAIT", 0)
            ]),
        )
        .await;
        expect_upload(&server, "PORTRAIT/0/SCREENSHOT", "0.png", png(1080, 1920, 1), 100).await;
        expect_upload(&server, "PORTRAIT/1/SCREENSHOT", "1.png", png(1080, 1920, 2), 101).await;
        expect_upload(&server, "PORTRAIT/2/SCREENSHOT", "2.jpg", jpeg(1080, 1920, 3), 102).await;
        expect_upload(
            &server,
            "LANDSCAPE/0/TABLET_SCREENSHOT",
            "0.png",
            png(2560, 1440, 4),
            103,
        )
        .await;
        for id in [11, 13] {
            Mock::given(method("DELETE"))
                .and(path(format!("{}/{id}", screenshots_path(9))))
                .respond_with(ok(Value::Null))
                .expect(1)
                .mount(&server)
                .await;
        }
        let f = files();
        phone_set(&f.root);
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let out = run(
            "sync_screenshots",
            &ctx,
            json!({"dir": s(&f.root.join("shots")), "delete_extra": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["uploaded"], 4);
        assert_eq!(out["deleted"], 2);
        assert_eq!(out["uploads"][0]["id"], 100);
        let deletions = out["deletions"].as_array().unwrap();
        // Другая ориентация — первой (до загрузок), лишние — после.
        assert_eq!(deletions[0]["id"], 13);
        assert_eq!(deletions[0]["reason"], "other orientation");
        assert_eq!(deletions[1]["id"], 11);
        assert_eq!(deletions[1]["reason"], "no local file");
        assert!(out.get("kept").is_none());
    }

    #[tokio::test]
    async fn sync_orientation_conflicts_and_skip_existing() {
        let server = MockServer::start().await;
        mount_remote(
            &server,
            json!([
                remote_shot(10, "SCREENSHOT", "PORTRAIT", 0),
                remote_shot(13, "TABLET_SCREENSHOT", "PORTRAIT", 0)
            ]),
        )
        .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        // Только место 1 телефона: место 0 занято и пропускается.
        expect_upload(&server, "PORTRAIT/1/SCREENSHOT", "1.png", png(1080, 1920, 2), 101).await;
        let f = files();
        write(&f.root, "shots/phone/portrait/0.png", &png(1080, 1920, 1));
        write(&f.root, "shots/phone/portrait/1.png", &png(1080, 1920, 2));
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());
        let dir = s(&f.root.join("shots"));

        let out = run(
            "sync_screenshots",
            &ctx,
            json!({"dir": dir, "skip_existing": true}),
        )
        .await
        .unwrap();
        assert_eq!(out["uploaded"], 1);
        assert_eq!(out["skipped"][0]["file"], "phone/portrait/0.png");
        assert_eq!(out["untouched"], json!({"TABLET_SCREENSHOT": 1}));
        // Меньше трёх скриншотов телефона после синхронизации — предупреждение.
        assert!(
            out["warnings"][0]
                .as_str()
                .unwrap()
                .contains("at least 3"),
            "{out}"
        );

        // Планшет в другой ориентации без delete_extra — ошибка до изменений.
        write(&f.root, "shots/tablet/landscape/0.png", &png(2560, 1440, 4));
        let err = run("sync_screenshots", &ctx, json!({"dir": dir}))
            .await
            .unwrap_err();
        assert!(err.message().contains("another orientation"), "{err}");
        assert!(err.message().contains("delete_extra"), "{err}");
    }

    #[tokio::test]
    async fn sync_checks_files_before_requests() {
        let server = no_requests().await;
        let f = files();
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());
        write(&f.root, "bad/phone/portrait/0.jpg", &png(1080, 1920, 1));
        write(&f.root, "bad/phone/portrait/1.png", &png(100, 200, 1));
        write(&f.root, "bad/phone/portrait/2.png", &png(1920, 1080, 1));
        write(&f.root, "bad/phone/portrait/10.png", &png(1080, 1920, 1));
        write(&f.root, "bad/phone/portrait/first.png", &png(1080, 1920, 1));
        write(&f.root, "bad/phone/portrait/3.png", b"not an image");
        write(&f.root, "bad/phone/portrait/4.png", &png(1080, 1920, 1));
        write(&f.root, "bad/phone/portrait/04.png", &png(1080, 1920, 1));
        let mut big = png(1080, 1920, 1);
        big.resize(5 * 1024 * 1024 + 1, 0);
        write(&f.root, "bad/phone/portrait/5.png", &big);
        write(&f.root, "bad/tablet/portrait/0.png", &png(1440, 2560, 1));
        write(&f.root, "bad/tablet/landscape/1.png", &png(2560, 1440, 1));

        let err = run(
            "sync_screenshots",
            &ctx,
            json!({"dir": s(&f.root.join("bad")), "dry_run": true}),
        )
        .await
        .unwrap_err();
        let message = err.message();
        for part in [
            "nothing was changed",
            "phone/portrait/0.jpg: the file is a PNG image, but its extension is .jpg",
            "phone/portrait/1.png: 100 x 200 px; sides must be from 320 to 3840 px",
            "phone/portrait/2.png: 1920 x 1080 px is not a portrait image",
            "phone/portrait/10.png: the file name must be the position from 0 to 9",
            "phone/portrait/first.png: the file name",
            "phone/portrait/3.png: not a PNG or JPEG",
            "two files for SCREENSHOT position 4",
            "phone/portrait/5.png: 5242881 bytes is more than 5 MB",
            "TABLET_SCREENSHOT: both portrait and landscape",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }

        // Пустой каталог, phone/ вместе с portrait/, каталог вне разрешённых.
        std::fs::create_dir_all(f.root.join("empty")).unwrap();
        let err = run("sync_screenshots", &ctx, json!({"dir": s(&f.root.join("empty"))}))
            .await
            .unwrap_err();
        assert!(err.message().contains("no screenshots found"), "{err}");
        write(&f.root, "both/phone/portrait/0.png", &png(1080, 1920, 1));
        write(&f.root, "both/portrait/1.png", &png(1080, 1920, 1));
        let err = run("sync_screenshots", &ctx, json!({"dir": s(&f.root.join("both"))}))
            .await
            .unwrap_err();
        assert!(err.message().contains("either phone/"), "{err}");
        let other = tempfile::tempdir().unwrap();
        let err = run("sync_screenshots", &ctx, json!({"dir": s(other.path())}))
            .await
            .unwrap_err();
        assert!(
            err.message().contains("outside the allowed directories"),
            "{err}"
        );
        let err = run(
            "sync_screenshots",
            &testing::context(&server.uri(), AccessLevel::Write, &[PKG], FilePolicy::disabled())
                .unwrap(),
            json!({"dir": s(&f.root.join("both"))}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("local files are not available"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sync_rejects_symlinks_out_of_allowed_dirs() {
        let server = no_requests().await;
        let f = files();
        let other = tempfile::tempdir().unwrap();
        write(other.path(), "secret.png", &png(1080, 1920, 1));
        write(other.path(), "portrait/0.png", &png(1080, 1920, 1));
        std::fs::create_dir_all(f.root.join("links/portrait")).unwrap();
        std::os::unix::fs::symlink(
            other.path().join("secret.png"),
            f.root.join("links/portrait/0.png"),
        )
        .unwrap();
        std::fs::create_dir_all(f.root.join("dirlink")).unwrap();
        std::os::unix::fs::symlink(other.path().join("portrait"), f.root.join("dirlink/portrait"))
            .unwrap();
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());
        for dir in ["links", "dirlink"] {
            let err = run("sync_screenshots", &ctx, json!({"dir": s(&f.root.join(dir))}))
                .await
                .unwrap_err();
            assert!(
                err.message().contains("outside the allowed directories"),
                "{dir}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn sync_uses_the_export_layout_and_shortcut() {
        let server = MockServer::start().await;
        mount_remote(&server, json!([])).await;
        expect_upload(&server, "LANDSCAPE/0/SCREENSHOT", "0.png", png(1920, 1080, 7), 1).await;
        let f = files();
        write(&f.root, "export/listing.json", b"{}");
        write(&f.root, "export/screenshots/landscape/0.png", &png(1920, 1080, 7));
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let out = run(
            "sync_screenshots",
            &ctx,
            json!({"dir": s(&f.root.join("export"))}),
        )
        .await
        .unwrap();
        assert_eq!(out["uploads"][0]["file"], "screenshots/landscape/0.png");
        assert_eq!(out["uploaded"], 1);
    }

    #[test]
    fn images_are_checked() {
        assert_eq!(image_size(ImageFormat::Png, &png(1080, 1920, 0)), Some((1080, 1920)));
        assert_eq!(image_size(ImageFormat::Jpeg, &jpeg(1920, 1080, 0)), Some((1920, 1080)));
        assert_eq!(image_size(ImageFormat::Png, b"\x89PNG\r\n\x1a\n"), None);
        let ((w, h), warning) =
            check_screenshot("a.png", "png", &png(1080, 1920, 0), "PORTRAIT").unwrap();
        assert_eq!((w, h), (1080, 1920));
        assert!(warning.is_none());
        let (_, warning) =
            check_screenshot("b.jpeg", "jpeg", &jpeg(1000, 1000 + 1, 0), "PORTRAIT").unwrap();
        assert!(warning.unwrap().contains("not 9:16"));
        let err = check_screenshot("c.png", "png", &png(1000, 1000, 0), "LANDSCAPE").unwrap_err();
        assert!(err.contains("not a landscape image"), "{err}");
        let err = check_screenshot("d.png", "png", &png(2160, 3841, 0), "PORTRAIT").unwrap_err();
        assert!(err.contains("sides must be"), "{err}");
    }

    #[test]
    fn diffs_and_fields() {
        assert_eq!(line_diff("a\nb\nc", "a\nc\nd"), ["- b", "+ d"]);
        assert_eq!(line_diff("a\nb", "a\nB"), ["- b", "+ B"]);
        assert!(line_diff("same", "same").is_empty());
        assert!(check_field("video", &json!({"orientation": "PORTRAIT", "url": "x"})).is_ok());
        assert!(check_field("video", &json!({"orientation": "UP"})).is_err());
        assert!(check_field("inAppPurchaseTypes", &json!(["PRODUCTS"])).is_ok());
        assert!(check_field("inAppPurchaseTypes", &json!(["COINS"])).is_err());
        assert!(check_field("categories", &json!(["a", "a"])).is_err());
        assert!(check_field("appType", &json!("GAMES")).is_ok());
        let change = field_change("whatsNew", &Value::Null, &json!("New"));
        assert_eq!(change["current_characters"], 0);
        assert_eq!(change["lines"], json!(["+ New"]));
        assert_eq!(
            comparable("seoTagIds", &json!([102, 100])),
            comparable("seoTagIds", &json!([100, 102]))
        );
    }
}
