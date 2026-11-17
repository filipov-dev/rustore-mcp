//! Составные инструменты группы: тексты, иконка и скриншоты (набор `listing`).
//!
//! Тексты карточки в RuStore — поля версии: название, описания, «Что нового», категории,
//! возраст, контакты и т. п. живут в версии (`get_version`), а меняются только в черновике
//! (`update_draft_version`). Скриншоты — тоже у версии (`list_screenshots`,
//! `upload_screenshot_v2`, `delete_screenshot`).
//!
//! - `listing_export` — тексты версии (активной, черновика или по `versionId`) и сведения об
//!   изображениях: в ответ или в каталог (`listing.json`, `images.json`). С каталогом иконка
//!   (`icon.png`, у ТВ — `icon_tv.png`) и скриншоты (по `sync_path`, раскладка
//!   `sync_screenshots`) скачиваются по ссылкам из ответа через [`Context::external`]:
//!   исполнитель принимает только пути RuStore API, а ссылки ведут в хранилище RuStore.
//!   Сбой одного файла — предупреждение, остальные скачиваются. В ответ и в `images.json`
//!   ссылки попадают без query и fragment: подпись или токен в ссылке агенту не нужны.
//! - `listing_apply` — тексты из `listing.json` (или аргумента) в черновик: разница по полям,
//!   `dry_run`, проверка ограничений справочника до запроса, `PATCH` только изменённых полей.
//!   Третий источник — `play_dir`, экспорт листинга Google Play (раскладка fastlane
//!   `supply`): русская локаль переносится в название, описания и «Что нового». Текст длиннее
//!   ограничения RuStore не обрезается молча (ошибка или `allow_truncate`), упоминания других
//!   магазинов — предупреждения: модерация RuStore может их отклонить.
//! - `sync_screenshots` — каталог скриншотов (`<устройство>/<ориентация>/<номер>.png`) в
//!   черновик: загрузка, удаление лишних по `delete_extra`, `dry_run`. Хеша и размера файла
//!   API не отдаёт, поэтому изменённый файл не отличить от прежнего: по умолчанию все
//!   локальные файлы загружаются заново, `skip_existing` пропускает занятые места.
//!
//! Как добавить инструмент — в описании модуля [`super`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::common::{
    bool_arg, object_schema, write_new_file, optional_str, optional_u64, package_name, package_name_property, pick,
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
            existing, and existing files are never overwritten. With `dir` the tool also \
            downloads the icon (icon.png, the TV icon to icon_tv.png) and the screenshots (to \
            their `sync_path`, like screenshots/phone/portrait/0.png) from the RuStore storage, \
            so the directory is ready for `sync_screenshots`; `images: false` skips the \
            downloads. A file that cannot be downloaded is reported in `warnings` and the \
            others are still downloaded.",
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
            null are not changed. To move texts from Google Play, pass `play_dir`: a listing \
            export of play-console-mcp (the fastlane supply layout: <locale>/title.txt, \
            short_description.txt, full_description.txt, changelogs/<versionCode>.txt); the \
            Russian locale (ru-RU, or ru) becomes appName, shortDescription, fullDescription \
            and whatsNew (the release notes of the highest versionCode). Texts longer than the \
            RuStore limits are never cut silently: the tool reports the excess and changes \
            nothing, unless `allow_truncate: true`. Mentions of Google Play, Play Market and \
            other stores are reported in `warnings`, because RuStore moderation may reject \
            them. The tool needs an existing draft: without `versionId` it uses the draft of \
            the app; when there is none, create it with `create_draft_version` first (only a \
            draft can be changed).",
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
            },
            "images": {
                "type": "boolean",
                "description": "With `dir`: download the icon (icon.png, icon_tv.png) and the \
                    screenshots (to their `sync_path`). Default: true."
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
                    (its `listing` object is applied). Pass one of `dir`, `listing` and \
                    `play_dir`."
            },
            "play_dir": {
                "type": "string",
                "description": "Directory of a Google Play listing export (play-console-mcp \
                    `listing_export`, the fastlane supply layout, like fastlane/metadata/android). \
                    The Russian locale is applied: <locale>/title.txt to appName, \
                    short_description.txt to shortDescription, full_description.txt to \
                    fullDescription, changelogs/<highest versionCode>.txt to whatsNew."
            },
            "play_locale": {
                "type": "string",
                "description": "With `play_dir`: the locale directory to take, like `ru-RU`. \
                    Default: ru-RU, otherwise ru."
            },
            "allow_truncate": {
                "type": "boolean",
                "description": "Cut texts longer than the RuStore limits at a word boundary and \
                    report it in `warnings`. Default: false (the tool reports the excess and \
                    changes nothing)."
            },
            "listing": {
                "type": "object",
                "description": format!(
                    "Listing fields to apply, as in the `listing` object of listing.json: {}. \
                     Pass one of `dir`, `listing` and `play_dir`.",
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
    sanitize_urls(&mut record, &["fileUrl"]);
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

/// Иконки в корне каталога экспорта: поле ссылки в `get_version` и имя файла без расширения.
const ICON_FILES: &[(&str, &str)] = &[("iconUrl", "icon"), ("iconUrlTv", "icon_tv")];

/// Ссылка без query и fragment. Сейчас ссылки хранилища RuStore без подписи, но если в
/// query появится подпись или токен, он не попадёт ни в ответ агенту, ни в `images.json`;
/// скачивание идёт по исходной ссылке.
fn public_url(url: &str) -> &str {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    &url[..end]
}

/// Заменяет строковые ссылки `fields` записи на [`public_url`].
fn sanitize_urls(record: &mut Value, fields: &[&str]) {
    let Value::Object(map) = record else {
        return;
    };
    for field in fields {
        if let Some(Value::String(url)) = map.get_mut(*field) {
            *url = public_url(url).to_owned();
        }
    }
}

/// Каталог `rel` (относительный, из обычных частей) внутри `root`: недостающие части
/// создаются по одной, каждая проверяется [`crate::files::FilePolicy`] (существующая часть
/// может оказаться символической ссылкой наружу). Возвращает канонический путь.
fn ensure_dir(ctx: &Context, root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let files = ctx.files();
    let mut current = root.to_path_buf();
    for part in rel.split('/').filter(|part| !part.is_empty()) {
        let next = current.join(part);
        if next.symlink_metadata().is_err() {
            let target = files.check_write(path_text(&next)?).map_err(file_error)?;
            match std::fs::create_dir(&target) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(ToolError::new(format!("cannot create `{rel}`: {err}")));
                }
            }
        }
        current = files.check_dir(path_text(&next)?).map_err(file_error)?;
    }
    Ok(current)
}

/// Скачивает картинку по ссылке `url` в `<root>/<stem>.<png|jpg>` (расширение — по
/// содержимому) и возвращает путь относительно `root` и размер. Ошибка — текст для
/// предупреждения, без ссылки.
async fn download_image(
    ctx: &Context,
    root: &Path,
    stem: &str,
    url: &str,
) -> Result<(String, usize), String> {
    let client = ctx.external().map_err(|err| err.message().to_owned())?;
    let file = client.get(url).await.map_err(|err| err.to_string())?;
    let format = ImageFormat::sniff(&file.bytes).ok_or_else(|| {
        format!(
            "the file is not a PNG or JPEG image ({})",
            file.content_type.as_deref().unwrap_or("no content type")
        )
    })?;
    let rel = format!("{stem}.{}", format.extension());
    let (dir, name) = match rel.rsplit_once('/') {
        Some((dir, name)) => (ensure_dir(ctx, root, dir).map_err(|err| err.to_string())?, name),
        None => (root.to_path_buf(), rel.as_str()),
    };
    let target = dir.join(name);
    write_new_file(
        ctx,
        "dir",
        path_text(&target).map_err(|err| err.to_string())?,
        &file.bytes,
    )
    .map_err(|err| err.to_string())?;
    Ok((rel, file.bytes.len()))
}

/// Что скачал экспорт.
#[derive(Default)]
struct Downloaded {
    /// Пути файлов относительно каталога экспорта.
    files: Vec<String>,
    bytes: usize,
}

/// Скачивает иконки (поля `ICON_FILES` версии) и скриншоты (по `sync_path` записей
/// `screenshots`, ссылки — `urls` в том же порядке) в каталог экспорта. У скачанного
/// скриншота `sync_path` уточняется по содержимому файла (PNG или JPEG) и ставится
/// `downloaded: true`; сбой — предупреждение в `warnings`.
async fn download_images(
    ctx: &Context,
    root: &Path,
    version: &Value,
    screenshots: &mut [Value],
    urls: &[Option<String>],
    warnings: &mut Vec<String>,
) -> Downloaded {
    let mut done = Downloaded::default();
    let mut record = |result: Result<(String, usize), String>, what: &str| match result {
        Ok((rel, bytes)) => {
            done.bytes += bytes;
            done.files.push(rel.clone());
            Some(rel)
        }
        Err(err) => {
            warnings.push(format!("{what} was not downloaded: {err}"));
            None
        }
    };
    for (field, stem) in ICON_FILES {
        if let Some(url) = text_field(version, field).filter(|url| !url.trim().is_empty()) {
            let result = download_image(ctx, root, stem, &url).await;
            record(result, &format!("`{field}` ({stem}.png)"));
        }
    }
    for (shot, url) in screenshots.iter_mut().zip(urls) {
        let Some(sync_path) = text_field(shot, "sync_path") else {
            continue;
        };
        let Some(url) = url.as_deref().filter(|url| !url.trim().is_empty()) else {
            record(
                Err("RuStore returned no link".to_owned()),
                &format!("`{sync_path}`"),
            );
            continue;
        };
        let stem = sync_path
            .rsplit_once('.')
            .map_or(sync_path.as_str(), |(stem, _)| stem);
        let result = download_image(ctx, root, stem, url).await;
        if let (Some(rel), Value::Object(map)) = (record(result, &format!("`{sync_path}`")), shot)
        {
            map.insert("sync_path".into(), json!(rel));
            map.insert("downloaded".into(), json!(true));
        }
    }
    done
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
        let with_images = bool_arg(&args, "images", true)?;
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
        // Исходные ссылки — только для скачивания; в ответ и в файл идут без query.
        let urls: Vec<Option<String>> = screenshots
            .iter()
            .map(|item| text_field(item, "fileUrl"))
            .collect();
        let mut screenshots: Vec<Value> = screenshots.iter().map(export_screenshot).collect();

        let listing = export_listing(&version);
        let summary = version_summary(&version);
        let mut icon = pick(&version, &["iconUid", "iconUrl", "iconUidTv", "iconUrlTv"]);
        sanitize_urls(&mut icon, &["iconUrl", "iconUrlTv"]);
        let images = |screenshots: &[Value]| {
            let mut images = Map::new();
            images.insert("packageName".into(), json!(package));
            images.insert("versionId".into(), json!(id));
            if icon.as_object().is_some_and(|icon| !icon.is_empty()) {
                images.insert("icon".into(), icon.clone());
            }
            images.insert("screenshots_source".into(), json!(source));
            images.insert("screenshots".into(), json!(screenshots));
            images
        };

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("version".into(), summary.clone());
        let note = match target {
            None => {
                result.insert("listing".into(), Value::Object(listing));
                result.insert("images".into(), Value::Object(images(&screenshots)));
                "Images are downloaded only with `dir`: call again with `dir` to get the icon \
                 and the screenshots as files at their `sync_path`, ready for \
                 `sync_screenshots`."
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
                let downloaded = if with_images {
                    Some(
                        download_images(
                            ctx,
                            &target.path,
                            &version,
                            &mut screenshots,
                            &urls,
                            &mut warnings,
                        )
                        .await,
                    )
                } else {
                    None
                };
                write_new_json(
                    ctx,
                    &target.path.join(IMAGES_FILE),
                    &Value::Object(images(&screenshots)),
                )?;
                result.insert("dir".into(), json!(dir));
                result.insert("files".into(), json!([LISTING_FILE, IMAGES_FILE]));
                result.insert("fields".into(), json!(fields));
                result.insert("screenshots".into(), json!(screenshots.len()));
                match downloaded {
                    Some(downloaded) => {
                        result.insert("downloaded".into(), json!(downloaded.files));
                        result.insert("downloaded_bytes".into(), json!(downloaded.bytes));
                        "The icon (icon.png, icon_tv.png) and the screenshots (at their \
                         `sync_path`, relative to the export directory) were downloaded; files \
                         that failed are listed in `warnings`. To sync screenshots, edit the \
                         files and call `sync_screenshots` with this directory; upload a new \
                         icon with `upload_icon`."
                    }
                    None => {
                        "Images were not downloaded (`images: false`). To sync screenshots, put \
                         the files at `sync_path` (relative to the export directory) and call \
                         `sync_screenshots` with that directory."
                    }
                }
            }
        };
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        result.insert("note".into(), json!(note));
        Ok(Value::Object(result))
    })
}

// --- listing_apply -----------------------------------------------------------------------

// --- Перенос из Google Play ----------------------------------------------------------------

/// Тексты каталога экспорта листинга Google Play (раскладка fastlane `supply`, её пишет
/// `listing_export` play-console-mcp): файл в каталоге локали и поле RuStore. Видео,
/// изображения и контакты не переносятся: у RuStore другой формат видео, скриншоты
/// синхронизирует `sync_screenshots`, а частичный объект контактов стёр бы остальные.
const PLAY_TEXT_FILES: &[(&str, &str)] = &[
    ("title.txt", "appName"),
    ("short_description.txt", "shortDescription"),
    ("full_description.txt", "fullDescription"),
];
/// Release notes: `<локаль>/changelogs/<versionCode>.txt` (и `default.txt` у fastlane).
const PLAY_CHANGELOGS: &str = "changelogs";
const PLAY_DEFAULT_CHANGELOG: &str = "default.txt";
/// Русская локаль Google Play — `ru-RU`; fastlane и ручные каталоги бывают с `ru`.
const PLAY_RUSSIAN_LOCALES: &[&str] = &["ru-RU", "ru"];

/// Тексты из каталога Google Play: поля RuStore, откуда каждое взято, предупреждения.
struct PlayListing {
    locale: String,
    listing: Map<String, Value>,
    files: Map<String, Value>,
    warnings: Vec<String>,
}

/// Текстовый файл каталога (не больше [`MAX_LISTING_FILE`]); `rel` — путь для сообщений.
fn read_text_file(ctx: &Context, path: &Path, rel: &str) -> Result<String, ToolError> {
    let path = ctx
        .files()
        .check_read(path_text(path)?)
        .map_err(file_error)?;
    let size = std::fs::metadata(&path)
        .map_err(|err| ToolError::new(format!("cannot read `{rel}`: {err}")))?
        .len();
    if size > MAX_LISTING_FILE {
        return Err(ToolError::new(format!("`{rel}` is too large ({size} bytes)")));
    }
    let bytes =
        std::fs::read(&path).map_err(|err| ToolError::new(format!("cannot read `{rel}`: {err}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::new(format!("`{rel}` is not UTF-8 text")))?;
    // Переводы строк Windows и BOM редакторов не должны попасть в карточку.
    Ok(text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .trim()
        .to_owned())
}

/// Каталог локали: `play_locale`, иначе `ru-RU`, иначе `ru` (без учёта регистра).
fn play_locale_dir(
    ctx: &Context,
    root: &Path,
    wanted: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<(String, PathBuf), ToolError> {
    let locales: Vec<(String, PathBuf)> = dir_entries(root)?
        .into_iter()
        .filter(|(name, _, is_dir)| *is_dir && !name.starts_with('.'))
        .map(|(name, path, _)| (name, path))
        .collect();
    let find = |wanted: &str| {
        locales
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
    };
    let found = match wanted {
        Some(wanted) => find(wanted),
        None => {
            let found: Vec<&(String, PathBuf)> =
                PLAY_RUSSIAN_LOCALES.iter().filter_map(|l| find(l)).collect();
            if let [first, rest @ ..] = found.as_slice() {
                for (other, _) in rest {
                    warnings.push(format!(
                        "both `{}/` and `{other}/` exist; the texts are taken from `{}/`",
                        first.0, first.0
                    ));
                }
            }
            found.first().copied()
        }
    };
    let Some((name, path)) = found else {
        let names: Vec<&str> = locales.iter().map(|(name, _)| name.as_str()).collect();
        let available = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        };
        return Err(ToolError::new(match wanted {
            Some(wanted) => format!(
                "the Google Play export has no locale `{wanted}/` (locales: {available}), \
                 nothing was changed"
            ),
            None => format!(
                "the Google Play export has no Russian locale (`ru-RU/` or `ru/`; locales: \
                 {available}), nothing was changed. RuStore shows the listing in Russian: add \
                 the Russian texts in Google Play or to the export directory, or pass \
                 `play_locale` to take another locale deliberately"
            ),
        }));
    };
    let path = ctx
        .files()
        .check_dir(path_text(path)?)
        .map_err(file_error)?;
    Ok((name.clone(), path))
}

/// Release notes с наибольшим `versionCode` (иначе `default.txt`): имя файла.
fn latest_changelog(dir: &Path) -> Result<Option<String>, ToolError> {
    let mut best: Option<(u64, String)> = None;
    let mut default = None;
    for (name, _, is_dir) in dir_entries(dir)? {
        if is_dir {
            continue;
        }
        if name == PLAY_DEFAULT_CHANGELOG {
            default = Some(name);
            continue;
        }
        let Some(code) = name
            .strip_suffix(".txt")
            .and_then(|stem| stem.parse::<u64>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(best, _)| code > *best) {
            best = Some((code, name));
        }
    }
    Ok(best.map(|(_, name)| name).or(default))
}

/// Читает тексты русской локали из каталога экспорта Google Play.
fn read_play_export(
    ctx: &Context,
    dir: &str,
    wanted: Option<&str>,
) -> Result<PlayListing, ToolError> {
    let root = ctx.files().check_dir(dir).map_err(file_error)?;
    let mut warnings = Vec::new();
    let (locale, locale_dir) = play_locale_dir(ctx, &root, wanted, &mut warnings)?;
    let mut listing = Map::new();
    let mut files = Map::new();
    let mut seen = false;
    let mut take = |field: &str, rel: String, text: String, warnings: &mut Vec<String>| {
        seen = true;
        if text.is_empty() {
            warnings.push(format!("`{rel}` is empty; `{field}` is not changed"));
        } else {
            listing.insert(field.to_owned(), json!(text));
            files.insert(field.to_owned(), json!(rel));
        }
    };
    for (file, field) in PLAY_TEXT_FILES {
        let path = locale_dir.join(file);
        if path.symlink_metadata().is_ok() {
            let rel = format!("{locale}/{file}");
            let text = read_text_file(ctx, &path, &rel)?;
            take(field, rel, text, &mut warnings);
        }
    }
    let changelogs = locale_dir.join(PLAY_CHANGELOGS);
    if changelogs.is_dir() {
        let changelogs = ctx
            .files()
            .check_dir(path_text(&changelogs)?)
            .map_err(file_error)?;
        if let Some(name) = latest_changelog(&changelogs)? {
            let rel = format!("{locale}/{PLAY_CHANGELOGS}/{name}");
            let text = read_text_file(ctx, &changelogs.join(&name), &rel)?;
            take("whatsNew", rel, text, &mut warnings);
        }
    }
    if !seen {
        return Err(ToolError::new(format!(
            "`{locale}/` has none of title.txt, short_description.txt, full_description.txt \
             and changelogs/<versionCode>.txt: is `play_dir` a Google Play listing export \
             (the fastlane supply layout)?"
        )));
    }
    Ok(PlayListing {
        locale,
        listing,
        files,
        warnings,
    })
}

/// Текст длиннее ограничения RuStore: поле, длина, предел.
struct OverLimit {
    field: String,
    length: usize,
    max: usize,
}

/// Текстовые поля длиннее ограничений справочника.
fn over_limit(listing: &Map<String, Value>) -> Vec<OverLimit> {
    listing
        .iter()
        .filter_map(|(field, value)| {
            let (_, max) = text_limits(field)?;
            let length = value.as_str()?.chars().count();
            (length > max).then(|| OverLimit {
                field: field.clone(),
                length,
                max,
            })
        })
        .collect()
}

/// Обрезает текст до `max` символов по границе слова (если она не дальше пятой части
/// предела от конца), без пробелов в конце.
fn truncate_text(text: &str, max: usize) -> String {
    let cut: String = text.chars().take(max).collect();
    let at_boundary = text.chars().nth(max).is_none_or(char::is_whitespace);
    let cut = if at_boundary {
        cut.as_str()
    } else {
        match cut.rfind(char::is_whitespace) {
            Some(pos) if cut[..pos].chars().count() >= max - max / 5 => &cut[..pos],
            _ => cut.as_str(),
        }
    };
    cut.trim_end().to_owned()
}

/// Упоминания других магазинов приложений: модерация RuStore может отклонить такие тексты.
static STORE_MENTIONS: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:google[\s-]*play(?:[\s-]*(?:store|market|маркет))?|(?:google|гугл)[\s-]*(?:плей|плэй)(?:[\s-]*(?:маркет|market|стор|store))?|(?:play|плей|плэй)[\s-]*(?:market|маркет|store|стор)|play\.google\.com|android[\s-]*market)",
    )
    .ok()
});

/// Предупреждения об упоминаниях Google Play и похожих в текстовых полях.
fn store_mentions(fields: &[(String, Value)]) -> Vec<String> {
    let Some(regex) = STORE_MENTIONS.as_ref() else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for (field, value) in fields {
        let Some(text) = value.as_str() else {
            continue;
        };
        let mut found: Vec<String> = Vec::new();
        let mut first_line = None;
        for m in regex.find_iter(text) {
            first_line.get_or_insert_with(|| text[..m.start()].matches('\n').count() + 1);
            if !found.iter().any(|f| f.eq_ignore_ascii_case(m.as_str())) {
                found.push(m.as_str().to_owned());
            }
        }
        if let Some(line) = first_line {
            let quoted: Vec<String> = found.iter().map(|f| format!("\"{f}\"")).collect();
            warnings.push(format!(
                "`{field}` mentions {} (first at line {line}): RuStore moderation may reject \
                 texts that mention other app stores; rewrite it before sending the version \
                 for review",
                quoted.join(", ")
            ));
        }
    }
    warnings
}


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

/// Длина текстовых полей по справочнику (создание и редактирование черновика): минимум и
/// максимум символов.
fn text_limits(field: &str) -> Option<(usize, usize)> {
    Some(match field {
        "appName" => (1, 50),
        "shortDescription" => (0, 80),
        "fullDescription" | "privacyExplanation" => (0, 4000),
        "whatsNew" => (0, 5000),
        "moderInfo" => (0, 180),
        "shortDescriptionTv" => (1, 80),
        "fullDescriptionTv" => (1, 4000),
        "whatsNewTv" => (1, 5000),
        _ => return None,
    })
}

/// Проверка поля по ограничениям справочника (создание и редактирование черновика).
fn check_field(field: &str, value: &Value) -> Result<(), String> {
    if let Some((min, max)) = text_limits(field) {
        return check_text(value, min, max);
    }
    match field {
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
        let play_dir = optional_str(&args, "play_dir")?;
        let play_locale = optional_str(&args, "play_locale")?;
        let allow_truncate = bool_arg(&args, "allow_truncate", false)?;
        let inline = args.get("listing").filter(|value| !value.is_null());
        if [dir.is_some(), inline.is_some(), play_dir.is_some()]
            .iter()
            .filter(|given| **given)
            .count()
            > 1
        {
            return Err(ToolError::new(
                "pass only one of `dir`, `listing` and `play_dir`",
            ));
        }
        if play_locale.is_some() && play_dir.is_none() {
            return Err(ToolError::new("`play_locale` needs `play_dir`"));
        }
        let mut warnings = Vec::new();
        let mut source = None;
        let mut listing = match (dir, inline, play_dir) {
            (Some(dir), _, _) => read_listing_file(ctx, dir)?,
            (_, Some(Value::Object(listing)), _) => listing.clone(),
            (_, Some(_), _) => {
                return Err(ToolError::new("argument `listing` must be an object"));
            }
            (_, _, Some(play_dir)) => {
                let play = read_play_export(ctx, play_dir, play_locale)?;
                warnings.extend(play.warnings);
                source = Some(json!({
                    "format": "Google Play listing export (fastlane supply layout)",
                    "locale": play.locale,
                    "files": play.files,
                }));
                play.listing
            }
            (None, None, None) => {
                return Err(ToolError::new(
                    "pass `dir` (a directory with listing.json), `listing` (an object) or \
                     `play_dir` (a Google Play listing export)",
                ));
            }
        };
        // Все проверки — до запросов. Тексты длиннее ограничений не обрезаются молча:
        // превышение — ошибка, обрезка — только с `allow_truncate`.
        let over: Vec<OverLimit> = over_limit(&listing)
            .into_iter()
            .filter(|item| only.is_empty() || only.contains(&item.field))
            .collect();
        if allow_truncate {
            for item in &over {
                if let Some(Value::String(text)) = listing.get_mut(&item.field) {
                    *text = truncate_text(text, item.max);
                    warnings.push(format!(
                        "`{}` was cut from {} to {} characters (RuStore allows at most {}); \
                         check the end of the text",
                        item.field,
                        item.length,
                        text.chars().count(),
                        item.max
                    ));
                }
            }
        } else if !over.is_empty() && source.is_some() {
            let files = source.as_ref().and_then(|source| source.get("files"));
            let details: Vec<String> = over
                .iter()
                .map(|item| {
                    let file = files
                        .and_then(|files| files.get(&item.field))
                        .and_then(Value::as_str)
                        .map(|file| format!(" ({file})"))
                        .unwrap_or_default();
                    format!(
                        "`{}`{file} has {} characters, RuStore allows at most {} ({} over)",
                        item.field,
                        item.length,
                        item.max,
                        item.length - item.max
                    )
                })
                .collect();
            return Err(ToolError::new(format!(
                "texts are longer than the RuStore limits, nothing was changed: {}. Shorten \
                 the texts (edit the files), or pass `allow_truncate: true` to cut them at a \
                 word boundary",
                details.join("; ")
            )));
        }
        let desired = desired_fields(&listing, &only).map_err(|err| {
            if over.is_empty() || allow_truncate {
                err
            } else {
                ToolError::new(format!(
                    "{}. Texts over the limit can be cut with `allow_truncate: true`",
                    err.message()
                ))
            }
        })?;
        warnings.extend(store_mentions(&desired));

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
        if let Some(source) = source {
            result.insert("source".into(), source);
        }
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        let mut notes = Vec::new();
        if play_dir.is_some() {
            notes.push(
                "from the Google Play export only appName, shortDescription, fullDescription and \
                 whatsNew (the release notes of the highest versionCode) are taken; the video, \
                 the images and the contacts are not transferred (screenshots: \
                 `sync_screenshots`)"
                    .to_owned(),
            );
        }
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

    /// Расширение файла, которое принимает `sync_screenshots`.
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
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
        assert!(out["note"].as_str().unwrap().contains("only with `dir`"));
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

    /// Контекст, в котором хранилище — тот же `wiremock` по http на 127.0.0.1.
    fn ctx_with_storage(
        server: &MockServer,
        access: AccessLevel,
        policy: FilePolicy,
        max_bytes: u64,
    ) -> Context {
        let external = rustore_api::external::ExternalClient::with_loopback_http(
            &["127.0.0.1"],
            std::time::Duration::from_secs(5),
            max_bytes,
        )
        .unwrap();
        ctx(server, access, policy).with_external(external)
    }

    /// Опубликованная версия 7, ссылки на картинки которой ведут на `storage`.
    async fn mount_with_storage(server: &MockServer, storage: &str, shots: Value) {
        Mock::given(method("GET"))
            .and(path(VERSIONS))
            .and(query_param("versionStatuses", "ACTIVE,PARTIAL_ACTIVE"))
            .respond_with(ok(page(json!([{"versionId": 7, "versionStatus": "ACTIVE"}]))))
            .mount(server)
            .await;
        let mut version = published_version();
        version["iconUrl"] = json!(format!("{storage}/static/icon.png?token=icon-secret-1"));
        Mock::given(method("GET"))
            .and(path(version_path(7)))
            .respond_with(ok(version))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(screenshots_path(7)))
            .respond_with(ok(page(shots)))
            .mount(server)
            .await;
    }

    fn shot_at(id: u64, kind: &str, orientation: &str, ordinal: u64, url: &str) -> Value {
        json!({"id": id, "type": kind, "orientation": orientation, "ordinal": ordinal,
               "fileUid": format!("uid-{id}"), "fileUrl": url})
    }

    async fn serve_file(server: &MockServer, route: &str, bytes: Vec<u8>, mime: &str) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_raw(bytes, mime))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn export_downloads_images_for_sync_screenshots() {
        let server = MockServer::start().await;
        let base = server.uri();
        mount_with_storage(
            &server,
            &base,
            json!([
                shot_at(11, "SCREENSHOT", "PORTRAIT", 0,
                        &format!("{base}/static/s11.png?X-Amz-Signature=shot-secret-2")),
                // Ссылка с .png, а файл — JPEG: путь уточняется по содержимому.
                shot_at(12, "SCREENSHOT", "PORTRAIT", 1, &format!("{base}/static/s12.png")),
                shot_at(21, "TABLET_SCREENSHOT", "LANDSCAPE", 0,
                        &format!("{base}/static/s21.png")),
            ]),
        )
        .await;
        serve_file(&server, "/static/icon.png", png(512, 512, 9), "image/png").await;
        serve_file(&server, "/static/s11.png", png(1080, 1920, 1), "image/png").await;
        serve_file(&server, "/static/s12.png", jpeg(1080, 1920, 2), "image/jpeg").await;
        serve_file(&server, "/static/s21.png", png(1920, 1080, 3), "image/png").await;
        mount_remote(&server, json!([])).await;
        let f = files();
        let ctx = ctx_with_storage(&server, AccessLevel::Write, f.policy.clone(), 1024 * 1024);
        let dir = f.root.join("export");

        let out = run("listing_export", &ctx, json!({"dir": s(&dir)}))
            .await
            .unwrap();
        assert!(out.get("warnings").is_none(), "{out}");
        assert_eq!(
            out["downloaded"],
            json!([
                "icon.png",
                "screenshots/phone/portrait/0.png",
                "screenshots/phone/portrait/1.jpg",
                "screenshots/tablet/landscape/0.png"
            ])
        );
        assert_eq!(std::fs::read(dir.join("icon.png")).unwrap(), png(512, 512, 9));
        assert_eq!(
            std::fs::read(dir.join("screenshots/phone/portrait/1.jpg")).unwrap(),
            jpeg(1080, 1920, 2)
        );
        assert!(out["note"].as_str().unwrap().contains("were downloaded"));
        // Подписи и токены из ссылок не попадают ни в ответ, ни в images.json.
        let images = std::fs::read_to_string(dir.join("images.json")).unwrap();
        for text in [out.to_string(), images.clone()] {
            assert!(!text.contains("secret"), "{text}");
        }
        let images: Value = serde_json::from_str(&images).unwrap();
        assert_eq!(
            images["icon"]["iconUrl"],
            json!(format!("{base}/static/icon.png"))
        );
        assert_eq!(
            images["screenshots"][1]["sync_path"],
            "screenshots/phone/portrait/1.jpg"
        );
        assert_eq!(images["screenshots"][1]["downloaded"], true);
        // Запросы в хранилище — без токена RuStore API.
        let requests = server.received_requests().await.unwrap();
        for request in requests.iter().filter(|r| r.url.path().starts_with("/static/")) {
            assert!(request.headers.get("public-token").is_none());
        }

        // Тот же каталог находит sync_screenshots: те же файлы по тем же путям.
        let plan = run(
            "sync_screenshots",
            &ctx,
            json!({"dir": s(&dir), "dry_run": true}),
        )
        .await
        .unwrap();
        let mut planned: Vec<String> = plan["uploads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|upload| upload["file"].as_str().unwrap().to_owned())
            .collect();
        planned.sort();
        let mut exported: Vec<String> = images["screenshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|shot| shot["sync_path"].as_str().unwrap().to_owned())
            .collect();
        exported.sort();
        assert_eq!(planned, exported);
        // Иконка в корне каталога не мешает: берётся подкаталог screenshots/.
        assert!(!plan.to_string().contains("ignored"), "{plan}");
    }

    #[tokio::test]
    async fn export_reports_failed_downloads_and_keeps_going() {
        let server = MockServer::start().await;
        let base = server.uri();
        let foreign = base.replace("127.0.0.1", "localhost");
        mount_with_storage(
            &server,
            &base,
            json!([
                shot_at(1, "SCREENSHOT", "PORTRAIT", 0, &format!("{base}/static/ok.png")),
                shot_at(2, "SCREENSHOT", "PORTRAIT", 1, &format!("{base}/static/missing.png")),
                shot_at(3, "SCREENSHOT", "PORTRAIT", 2, &format!("{base}/static/big.png")),
                shot_at(4, "SCREENSHOT", "PORTRAIT", 3, &format!("{base}/static/page.png")),
                shot_at(5, "SCREENSHOT", "PORTRAIT", 4, &format!("{foreign}/static/ok.png")),
                shot_at(6, "SCREENSHOT", "PORTRAIT", 5, "https://example.invalid/s.png"),
                shot_at(7, "SCREENSHOT", "PORTRAIT", 6, &format!("{base}/static/ok.png")),
            ]),
        )
        .await;
        serve_file(&server, "/static/icon.png", png(512, 512, 9), "image/png").await;
        serve_file(&server, "/static/ok.png", png(1080, 1920, 1), "image/png").await;
        serve_file(&server, "/static/big.png", vec![0x89; 5000], "image/png").await;
        serve_file(&server, "/static/page.png", b"<html>".to_vec(), "text/html").await;
        let f = files();
        // Существующий каталог, в котором один скриншот уже лежит: он не перезаписывается.
        let dir = f.root.join("export");
        write(&f.root, "export/screenshots/phone/portrait/6.png", b"mine");
        let ctx = ctx_with_storage(&server, AccessLevel::Read, f.policy.clone(), 4096);

        let out = run("listing_export", &ctx, json!({"dir": s(&dir)}))
            .await
            .unwrap();
        assert_eq!(
            out["downloaded"],
            json!(["icon.png", "screenshots/phone/portrait/0.png"])
        );
        let warnings: Vec<&str> = out["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap())
            .collect();
        assert_eq!(warnings.len(), 6, "{warnings:?}");
        for (warning, part) in warnings.iter().zip([
            "portrait/1.png` was not downloaded: the storage returned HTTP 404",
            "portrait/2.png` was not downloaded: the file is larger than 4096 bytes",
            "portrait/3.png` was not downloaded: the file is not a PNG or JPEG image (text/html)",
            "portrait/4.png` was not downloaded: the URL is not allowed: host `localhost`",
            "portrait/5.png` was not downloaded: the URL is not allowed: host `example.invalid`",
            "portrait/6.png` was not downloaded: ",
        ]) {
            assert!(warning.contains(part), "{part}: {warning}");
            assert!(!warning.contains("/static/"), "ссылка в предупреждении: {warning}");
        }
        assert!(warnings[5].contains("already exists"), "{}", warnings[5]);
        // Свой файл на занятом месте не перезаписан.
        assert_eq!(
            std::fs::read(dir.join("screenshots/phone/portrait/6.png")).unwrap(),
            b"mine"
        );
        let images: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("images.json")).unwrap())
                .unwrap();
        assert_eq!(images["screenshots"][0]["downloaded"], true);
        assert!(images["screenshots"][1].get("downloaded").is_none());
    }

    #[tokio::test]
    async fn export_without_images_or_over_http() {
        let server = MockServer::start().await;
        let base = server.uri();
        mount_with_storage(
            &server,
            &base,
            json!([shot_at(1, "SCREENSHOT", "PORTRAIT", 0, &format!("{base}/static/ok.png"))]),
        )
        .await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex("^/static/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(png(512, 512, 1), "image/png"))
            .expect(0)
            .mount(&server)
            .await;
        let f = files();
        // Обычный клиент: только https на static.rustore.ru, http на loopback отклоняется.
        let ctx = ctx(&server, AccessLevel::Read, f.policy.clone());

        let out = run("listing_export", &ctx, json!({"dir": s(&f.root.join("a"))}))
            .await
            .unwrap();
        assert_eq!(out["downloaded"], json!([]));
        let warnings = out["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for warning in warnings {
            assert!(
                warning.as_str().unwrap().contains("only https is allowed"),
                "{warning}"
            );
        }

        let out = run(
            "listing_export",
            &ctx,
            json!({"dir": s(&f.root.join("b")), "images": false}),
        )
        .await
        .unwrap();
        assert!(out.get("downloaded").is_none());
        assert!(out.get("warnings").is_none());
        assert!(out["note"].as_str().unwrap().contains("`images: false`"));
        assert!(!f.root.join("b/screenshots").exists());
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
        assert!(err.message().contains("only one of"), "{err}");

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

    /// Синтетический экспорт листинга Google Play (раскладка fastlane supply) в `rel`.
    fn play_export(root: &Path, rel: &str) {
        write(root, &format!("{rel}/default_language.txt"), b"en-US\n");
        write(root, &format!("{rel}/en-US/title.txt"), b"Example\n");
        write(root, &format!("{rel}/ru-RU/title.txt"), "Пример\n".as_bytes());
        write(
            root,
            &format!("{rel}/ru-RU/short_description.txt"),
            "Короткое описание\n".as_bytes(),
        );
        write(
            root,
            &format!("{rel}/ru-RU/full_description.txt"),
            "\u{feff}Строка один\r\nСтрока два\r\n".as_bytes(),
        );
        write(root, &format!("{rel}/ru-RU/video.txt"), b"https://video.example.invalid/1\n");
        write(root, &format!("{rel}/ru-RU/changelogs/100.txt"), "Старое\n".as_bytes());
        write(root, &format!("{rel}/ru-RU/changelogs/120.txt"), "Исправления\n".as_bytes());
        write(root, &format!("{rel}/ru-RU/changelogs/default.txt"), "Общее\n".as_bytes());
        write(root, &format!("{rel}/ru-RU/images/icon.png"), &png(512, 512, 1));
    }

    #[tokio::test]
    async fn apply_moves_texts_from_a_google_play_export() {
        let server = MockServer::start().await;
        mount_draft(&server).await;
        Mock::given(method("PATCH"))
            .and(path(version_path(9)))
            .and(body_json(json!({
                "appName": "Пример",
                "shortDescription": "Короткое описание",
                "fullDescription": "Строка один\nСтрока два",
                "whatsNew": "Исправления"
            })))
            .respond_with(ok(json!(9)))
            .expect(1)
            .mount(&server)
            .await;
        let f = files();
        play_export(&f.root, "play");
        // Каталог `ru/` рядом с `ru-RU/`: берётся `ru-RU/`, о втором — предупреждение.
        write(&f.root, "play/ru/title.txt", "Другое\n".as_bytes());
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let out = run(
            "listing_apply",
            &ctx,
            json!({"play_dir": s(&f.root.join("play"))}),
        )
        .await
        .unwrap();
        assert_eq!(out["applied"], true, "{out}");
        assert_eq!(out["source"]["locale"], "ru-RU");
        assert_eq!(
            out["source"]["files"],
            json!({
                "appName": "ru-RU/title.txt",
                "shortDescription": "ru-RU/short_description.txt",
                "fullDescription": "ru-RU/full_description.txt",
                "whatsNew": "ru-RU/changelogs/120.txt"
            })
        );
        assert_eq!(out["warnings"].as_array().unwrap().len(), 1, "{out}");
        assert!(
            out["warnings"][0].as_str().unwrap().contains("`ru-RU/` and `ru/`"),
            "{out}"
        );
        assert!(out["note"].as_str().unwrap().contains("not transferred"));

        // Только `ru/` — тоже русская локаль; `fields` ограничивает поля.
        let f = files();
        write(&f.root, "play/en-US/title.txt", b"Example\n");
        write(&f.root, "play/ru/title.txt", "Example\n".as_bytes());
        write(&f.root, "play/ru/short_description.txt", "Новое\n".as_bytes());
        let ctx = super::super::testing::context(
            &server.uri(),
            AccessLevel::Write,
            &[PKG],
            f.policy.clone(),
        )
        .unwrap();
        let out = run(
            "listing_apply",
            &ctx,
            json!({"play_dir": s(&f.root.join("play")), "dry_run": true, "fields": ["appName"]}),
        )
        .await
        .unwrap();
        assert_eq!(out["source"]["locale"], "ru");
        assert_eq!(out["unchanged"], json!(["appName"]));
        assert_eq!(out["changes"], json!([]));
    }

    #[tokio::test]
    async fn apply_from_google_play_reports_texts_over_the_limits() {
        let server = no_requests().await;
        let f = files();
        play_export(&f.root, "play");
        std::fs::write(
            f.root.join("play/ru-RU/full_description.txt"),
            "я".repeat(4001),
        )
        .unwrap();
        std::fs::write(f.root.join("play/ru-RU/title.txt"), "т".repeat(55)).unwrap();
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let err = run(
            "listing_apply",
            &ctx,
            json!({"play_dir": s(&f.root.join("play")), "dry_run": true}),
        )
        .await
        .unwrap_err();
        let message = err.message();
        for part in [
            "nothing was changed",
            "`appName` (ru-RU/title.txt) has 55 characters, RuStore allows at most 50 (5 over)",
            "`fullDescription` (ru-RU/full_description.txt) has 4001 characters, RuStore allows \
             at most 4000 (1 over)",
            "`allow_truncate: true`",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
        // Превышение в других источниках — с той же подсказкой.
        let err = run(
            "listing_apply",
            &ctx,
            json!({"listing": {"shortDescription": "x".repeat(81)}}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("must be at most 80"), "{err}");
        assert!(err.message().contains("allow_truncate"), "{err}");
    }

    #[tokio::test]
    async fn apply_cuts_texts_only_when_allowed() {
        let server = MockServer::start().await;
        mount_draft(&server).await;
        let f = files();
        play_export(&f.root, "play");
        std::fs::write(
            f.root.join("play/ru-RU/full_description.txt"),
            "слово ".repeat(700),
        )
        .unwrap();
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let out = run(
            "listing_apply",
            &ctx,
            json!({"play_dir": s(&f.root.join("play")), "dry_run": true, "allow_truncate": true}),
        )
        .await
        .unwrap();
        let full = out["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["field"] == "fullDescription")
            .unwrap();
        // По границе слова: 666 слов через пробел, без пробела в конце.
        assert_eq!(full["new_characters"], 3995);
        assert!(
            out["warnings"][0]
                .as_str()
                .unwrap()
                .contains("`fullDescription` was cut from 4199 to 3995 characters"),
            "{out}"
        );
        assert_eq!(truncate_text("abcdefghij klm", 12), "abcdefghij");
        // Граница слова дальше пятой части предела — обрезка по символам.
        assert_eq!(truncate_text("abc def", 5), "abc d");
        assert_eq!(truncate_text("abcdefgh", 5), "abcde");
        assert_eq!(truncate_text("abc d", 5), "abc d");
    }

    #[tokio::test]
    async fn apply_from_google_play_needs_a_russian_locale() {
        let server = no_requests().await;
        let f = files();
        write(&f.root, "play/en-US/title.txt", b"Example\n");
        write(&f.root, "play/de-DE/title.txt", b"Beispiel\n");
        write(&f.root, "empty/ru-RU/notes.md", b"-");
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());
        let apply = |args: Value| run("listing_apply", &ctx, args);

        let err = apply(json!({"play_dir": s(&f.root.join("play"))}))
            .await
            .unwrap_err();
        let message = err.message();
        assert!(message.contains("no Russian locale"), "{message}");
        assert!(message.contains("de-DE, en-US"), "{message}");
        assert!(message.contains("`play_locale`"), "{message}");

        let err = apply(json!({"play_dir": s(&f.root.join("play")), "play_locale": "fr-FR"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("no locale `fr-FR/`"), "{err}");
        let err = apply(json!({"play_dir": s(&f.root.join("empty"))}))
            .await
            .unwrap_err();
        assert!(err.message().contains("has none of title.txt"), "{err}");
        let err = apply(json!({"listing": {"appName": "x"}, "play_locale": "ru-RU"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("needs `play_dir`"), "{err}");
        let err = apply(json!({"listing": {"appName": "x"}, "play_dir": s(&f.root)}))
            .await
            .unwrap_err();
        assert!(err.message().contains("only one of"), "{err}");
    }

    #[tokio::test]
    async fn apply_warns_about_other_stores() {
        let server = MockServer::start().await;
        mount_draft(&server).await;
        let f = files();
        play_export(&f.root, "play");
        std::fs::write(
            f.root.join("play/ru-RU/full_description.txt"),
            "Лучшее приложение.\nСкачайте в Google Play или в Плей Маркете.\nGoogle play!",
        )
        .unwrap();
        std::fs::write(
            f.root.join("play/ru-RU/changelogs/120.txt"),
            "Подробности: https://play.google.com/store/apps",
        )
        .unwrap();
        let ctx = ctx(&server, AccessLevel::Write, f.policy.clone());

        let out = run(
            "listing_apply",
            &ctx,
            json!({"play_dir": s(&f.root.join("play")), "dry_run": true}),
        )
        .await
        .unwrap();
        let warnings: Vec<&str> = out["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_str().unwrap())
            .collect();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].starts_with(
                "`fullDescription` mentions \"Google Play\", \"Плей Маркет\" (first at line 2)"
            ),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].contains("moderation"), "{}", warnings[0]);
        assert!(warnings[1].starts_with("`whatsNew` mentions \"play.google.com\""));

        let fields = |text: &str| vec![("appName".to_owned(), json!(text))];
        for text in ["Play Store", "Гугл Плей", "GooglePlay", "Play Market", "Android Market"] {
            assert_eq!(store_mentions(&fields(text)).len(), 1, "{text}");
        }
        for text in ["Display store settings", "Играйте и побеждайте", "Google Drive"] {
            assert!(store_mentions(&fields(text)).is_empty(), "{text}");
        }
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
