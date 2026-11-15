//! Составные инструменты группы: отзывы и ответы (набор `reviews`).
//!
//! - `reviews_summary` — рейтинг приложения (`get_app_rating`) и последние отзывы со всех
//!   страниц `list_reviews` с фильтрами (период, оценки, без ответа, подстрока текста),
//!   счётчики по оценкам и по состоянию ответа, компактные записи без имени автора и данных
//!   устройства.
//! - `reviews_export` — все отзывы по фильтрам в новый локальный файл: CSV из
//!   `export_reviews_csv`, если фильтр — только период до 92 дней, иначе JSON Lines из
//!   `list_reviews`. Агенту — только путь и число записей.
//!
//! Контракты RuStore API, на которые опирается обход:
//!
//! - `list_reviews` отдаёт тело-массив (справочник описывает объект одного отзыва, пример —
//!   массив), сортировка — по `updatedAt` (дата публикации после модерации), новые первыми;
//!   фильтров по дате и оценке у метода нет, поэтому фильтры применяются здесь;
//! - период фильтрует по дате отзыва (`commentDateIso`, иначе `commentDate`, по UTC), а
//!   обход останавливается, когда вся страница опубликована раньше начала периода: отзыв
//!   публикуется не раньше, чем оставлен;
//! - состояние ответа — из `get_review_answer_statuses` (ответы на модерации и отклонённые
//!   видны только там), без него — по `devResponses` отзыва.
//!
//! Как добавить инструмент — в описании модуля [`super`].

use std::collections::{BTreeMap, HashMap};

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::common::{
    bool_arg, bounded_u64, check_new_file, day_of, format_date, object_schema, optional_date,
    optional_str, optional_u64, package_name, package_name_property, required_str, text_field,
    today_utc, write_new_file,
};
use super::{Collect, CompositeTool, Context, ToolFuture};
use crate::error::ToolError;

/// Инструменты группы; [`super::all`] собирает их в общий список.
pub const TOOLS: &[CompositeTool] = &[
    CompositeTool {
        name: "reviews_summary",
        toolset: "reviews",
        access: AccessLevel::Read,
        title: "Summary of the app rating and recent reviews",
        description: "Summarizes the reviews of one app: the rating statistics \
            (`get_app_rating`: average, totals, counts by stars) and the latest reviews read \
            across pages of `list_reviews` (newest first, up to `scan` reviews), filtered by a \
            period (`from`/`to` dates or the last `days`, by the date the review was left, \
            UTC), a star range (`min_rating`, `max_rating`), `unanswered_only` and a \
            case-insensitive substring of the text (`text`). Returns counts of the matched \
            reviews by stars (`by_rating`) and by answer state (`by_answer_status`: answered, \
            answer_in_moderation, answer_rejected, unanswered; from \
            `get_review_answer_statuses`) and up to `limit` compact records: commentId (pass \
            it to `reply_to_review`), appRating, commentDate, appVersionName, edited, \
            commentText (shortened to `max_text_length`), answer_status and the latest \
            developer answer. Reviewer names and device data are left out. Use it to triage \
            reviews and find the ones to answer; `reviews_export` saves all matching reviews \
            to a file.",
        input_schema: summary_schema,
        uses_local_files: false,
        run: reviews_summary,
    },
    CompositeTool {
        name: "reviews_export",
        toolset: "reviews",
        access: AccessLevel::Read,
        title: "Export reviews to a local file",
        description: "Saves all reviews of one app that match the filters to a new local file \
            `save_to` (never overwritten) and returns only the path and the number of \
            records, not the reviews. With only a period of up to 92 days (`from` and `to`, \
            or `days`) and `format` `auto` or `csv`, the file is RuStore's own CSV export \
            (`export_reviews_csv`: app version, dates, stars, text, reviewer name, comment \
            id, likes, edited). Otherwise (star range, `unanswered_only`, `text`, no or a \
            longer period, or `format: jsonl`) the tool reads `list_reviews` page by page and \
            writes JSON Lines, one review per line: commentId, appRating, commentText, \
            commentDate, updatedAt, appVersionName, appVersionCode, edited, likeCounter, \
            dislikeCounter, answer_status and the latest developer answer; the reviewer name \
            only with `include_user_name: true`. The file holds personal data of RuStore \
            users: keep it local and use it only for the user's task.",
        input_schema: export_schema,
        uses_local_files: true,
        run: reviews_export,
    },
];

/// Размер страницы `list_reviews` (максимум по справочнику).
const PAGE_SIZE: u64 = 100;
/// Сколько отзывов просматривает `reviews_summary` по умолчанию и самое большее.
const SUMMARY_SCAN_DEFAULT: u64 = 1000;
const SUMMARY_SCAN_MAX: u64 = 5000;
/// Предел страниц `reviews_export`: 20 000 отзывов.
const EXPORT_MAX_PAGES: usize = 200;
/// Предел ответов разработчика, которые читаются для состояний ответов.
const ANSWERS_MAX: usize = 10_000;
/// Самый длинный период CSV-выгрузки RuStore, дней.
const CSV_MAX_DAYS: i64 = 92;
/// Длина текста ответа разработчика в сводке.
const ANSWER_TEXT_LEN: usize = 300;

/// Состояния ответа на отзыв в результатах.
const ANSWERED: &str = "answered";
const IN_MODERATION: &str = "answer_in_moderation";
const REJECTED: &str = "answer_rejected";
const UNANSWERED: &str = "unanswered";
const ANSWER_STATES: &[&str] = &[ANSWERED, IN_MODERATION, REJECTED, UNANSWERED];

fn filter_properties() -> Map<String, Value> {
    let Value::Object(map) = json!({
        "packageName": package_name_property(),
        "from": {
            "type": "string",
            "description": "First day of the period, `YYYY-MM-DD` (UTC), by the date the \
                review was left."
        },
        "to": {
            "type": "string",
            "description": "Last day of the period, inclusive, `YYYY-MM-DD` (UTC). Default: \
                today."
        },
        "days": {
            "type": "integer",
            "minimum": 1,
            "maximum": 3650,
            "description": "The last N days up to `to` (or today), instead of `from`."
        },
        "min_rating": {
            "type": "integer",
            "minimum": 1,
            "maximum": 5,
            "description": "Only reviews with at least this many stars."
        },
        "max_rating": {
            "type": "integer",
            "minimum": 1,
            "maximum": 5,
            "description": "Only reviews with at most this many stars, like 2 for negative \
                reviews."
        },
        "unanswered_only": {
            "type": "boolean",
            "description": "Only reviews without a published answer or an answer in \
                moderation (unanswered, or the answer was rejected)."
        },
        "text": {
            "type": "string",
            "description": "Case-insensitive substring of the review text."
        }
    }) else {
        return Map::new();
    };
    map
}

fn summary_schema() -> Value {
    let mut properties = filter_properties();
    properties.insert(
        "limit".into(),
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": 100,
            "description": "Maximum number of reviews returned (default 20, newest first); \
                the counts cover all matched reviews."
        }),
    );
    properties.insert(
        "scan".into(),
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": SUMMARY_SCAN_MAX,
            "description": "Maximum number of the latest reviews read (default 1000)."
        }),
    );
    properties.insert(
        "max_text_length".into(),
        json!({
            "type": "integer",
            "minimum": 20,
            "maximum": 5000,
            "description": "Review texts longer than this are shortened (default 500 \
                characters)."
        }),
    );
    properties.insert(
        "answer_statuses".into(),
        json!({
            "type": "boolean",
            "description": "Read the moderation state of developer answers \
                (`get_review_answer_statuses`, default true) to tell answers in moderation \
                and rejected answers apart; false saves requests and uses only the published \
                answers of the reviews."
        }),
    );
    object_schema(Value::Object(properties), &[])
}

fn export_schema() -> Value {
    let mut properties = filter_properties();
    properties.insert(
        "save_to".into(),
        json!({
            "type": "string",
            "description": "Path of the new local file (absolute, or relative to the server's \
                working directory) inside the allowed directories; an existing file is never \
                overwritten. Use the `.csv` or `.jsonl` extension to match `format`."
        }),
    );
    properties.insert(
        "format".into(),
        json!({
            "type": "string",
            "enum": ["auto", "csv", "jsonl"],
            "description": "`auto` (default): CSV when only a period of up to 92 days is \
                given, otherwise JSON Lines. `csv` needs `from` (or `days`) and no other \
                filters. `jsonl` always reads `list_reviews`."
        }),
    );
    properties.insert(
        "include_user_name".into(),
        json!({
            "type": "boolean",
            "description": "Add the reviewer name (`userName`) to JSON Lines records \
                (default false). RuStore's CSV export always has it."
        }),
    );
    object_schema(Value::Object(properties), &["save_to"])
}

/// Фильтры отзывов; даты — номера дней от 1970-01-01.
#[derive(Debug, Clone, PartialEq)]
struct Filters {
    from: Option<i64>,
    to: Option<i64>,
    min_rating: u64,
    max_rating: u64,
    unanswered_only: bool,
    /// Подстрока текста в нижнем регистре.
    text: Option<String>,
}

impl Filters {
    fn parse(args: &JsonObject) -> Result<Self, ToolError> {
        let mut from = optional_date(args, "from")?;
        let mut to = optional_date(args, "to")?;
        if let Some(days) = optional_u64(args, "days")? {
            if from.is_some() {
                return Err(ToolError::new("pass either `from` or `days`, not both"));
            }
            if !(1..=3650).contains(&days) {
                return Err(ToolError::new("argument `days` must be from 1 to 3650"));
            }
            let last = to.unwrap_or_else(today_utc);
            to = Some(last);
            from = Some(last - (days as i64 - 1));
        }
        if let (Some(start), Some(end)) = (from, to)
            && start > end
        {
            return Err(ToolError::new("`from` must not be later than `to`"));
        }
        let min_rating = bounded_u64(args, "min_rating", 1, 1, 5)?;
        let max_rating = bounded_u64(args, "max_rating", 5, 1, 5)?;
        if min_rating > max_rating {
            return Err(ToolError::new(
                "`min_rating` must not be greater than `max_rating`",
            ));
        }
        Ok(Self {
            from,
            to,
            min_rating,
            max_rating,
            unanswered_only: bool_arg(args, "unanswered_only", false)?,
            text: optional_str(args, "text")?.map(str::to_lowercase),
        })
    }

    /// Есть фильтры, кроме периода (их нет у CSV-выгрузки RuStore).
    fn has_content_filters(&self) -> bool {
        self.min_rating > 1 || self.max_rating < 5 || self.unanswered_only || self.text.is_some()
    }

    fn matches(&self, review: &Value, answer: &str) -> bool {
        if self.from.is_some() || self.to.is_some() {
            let Some(day) = review_day(review) else {
                return false;
            };
            if self.from.is_some_and(|from| day < from) || self.to.is_some_and(|to| day > to) {
                return false;
            }
        }
        if self.min_rating > 1 || self.max_rating < 5 {
            let Some(rating) = rating(review) else {
                return false;
            };
            if rating < self.min_rating || rating > self.max_rating {
                return false;
            }
        }
        if self.unanswered_only && !matches!(answer, UNANSWERED | REJECTED) {
            return false;
        }
        if let Some(text) = &self.text {
            let review_text = text_field(review, "commentText").unwrap_or_default();
            if !review_text.to_lowercase().contains(text.as_str()) {
                return false;
            }
        }
        true
    }

    fn period(&self) -> Option<Value> {
        if self.from.is_none() && self.to.is_none() {
            return None;
        }
        let mut period = Map::new();
        if let Some(from) = self.from {
            period.insert("from".into(), json!(format_date(from)));
        }
        if let Some(to) = self.to {
            period.insert("to".into(), json!(format_date(to)));
        }
        Some(Value::Object(period))
    }
}

/// День, когда отзыв оставлен (UTC в `commentDateIso`, иначе `commentDate`).
fn review_day(review: &Value) -> Option<i64> {
    ["commentDateIso", "commentDate"]
        .iter()
        .find_map(|field| text_field(review, field).and_then(|text| day_of(&text)))
}

/// День публикации отзыва — по нему RuStore сортирует выдачу.
fn published_day(review: &Value) -> Option<i64> {
    ["updatedAt", "updatedAtIso"]
        .iter()
        .find_map(|field| text_field(review, field).and_then(|text| day_of(&text)))
}

/// Оценка отзыва (1–5).
fn rating(review: &Value) -> Option<u64> {
    text_field(review, "appRating").and_then(|value| {
        value
            .parse::<f64>()
            .ok()
            .filter(|rating| (0.0..=10.0).contains(rating))
            .map(|rating| rating.round() as u64)
    })
}

/// Отзывы со страниц `list_reviews`.
struct Scan {
    reviews: Vec<Value>,
    complete: bool,
}

/// Читает отзывы с первой страницы: до `max_items` отзывов и `max_pages` страниц. С
/// началом периода `from` останавливается на странице, где все отзывы опубликованы раньше
/// него (выдача отсортирована по дате публикации, новые первыми).
async fn scan_reviews(
    ctx: &Context,
    package: &str,
    from: Option<i64>,
    max_items: usize,
    max_pages: usize,
) -> Result<Scan, ToolError> {
    let mut reviews = Vec::new();
    for page in 0..max_pages {
        let response = ctx
            .request(
                "list_reviews",
                json!({"packageName": package, "page": page, "size": PAGE_SIZE}),
            )
            .await?;
        let items = page_items(response.body);
        let received = items.len();
        let mut all_older = received > 0;
        for review in items {
            if from.is_none_or(|from| published_day(&review).is_none_or(|day| day >= from)) {
                all_older = false;
            }
            if reviews.len() >= max_items {
                return Ok(Scan {
                    reviews,
                    complete: false,
                });
            }
            reviews.push(review);
        }
        if (received as u64) < PAGE_SIZE || all_older {
            return Ok(Scan {
                reviews,
                complete: true,
            });
        }
    }
    Ok(Scan {
        reviews,
        complete: false,
    })
}

/// Элементы страницы: тело-массив или `content` (на случай выдачи в виде страницы).
fn page_items(body: Value) -> Vec<Value> {
    match body {
        Value::Array(items) => items,
        Value::Object(mut map) => match map.remove("content") {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Состояния ответов разработчика по `commentId` из `get_review_answer_statuses`.
struct Answers {
    by_comment: HashMap<String, &'static str>,
    complete: bool,
}

async fn answer_statuses(ctx: &Context, package: &str) -> Result<Answers, ToolError> {
    let collected = ctx
        .collect(
            "get_review_answer_statuses",
            json!({"packageName": package}),
            Collect {
                page_size: PAGE_SIZE,
                max_items: ANSWERS_MAX,
            },
        )
        .await?;
    let mut by_comment: HashMap<String, &'static str> = HashMap::new();
    for answer in &collected.items {
        let Some(comment) = text_field(answer, "commentId") else {
            continue;
        };
        // Удалённый или заменённый ответ (DELETED) не считается: у отзыва мог появиться
        // новый ответ, а если нет — отзыв без ответа.
        let state = match text_field(answer, "status")
            .unwrap_or_default()
            .to_uppercase()
            .as_str()
        {
            "PUBLISHED" => ANSWERED,
            "MODERATION" => IN_MODERATION,
            "REJECTED" => REJECTED,
            _ => continue,
        };
        let entry = by_comment.entry(comment).or_insert(state);
        if answer_rank(state) > answer_rank(entry) {
            *entry = state;
        }
    }
    Ok(Answers {
        by_comment,
        complete: collected.complete,
    })
}

/// Какое состояние важнее, если у отзыва несколько ответов.
fn answer_rank(state: &str) -> u8 {
    match state {
        ANSWERED => 3,
        IN_MODERATION => 2,
        REJECTED => 1,
        _ => 0,
    }
}

/// Последний ответ разработчика из `devResponses` отзыва.
fn latest_answer(review: &Value) -> Option<&Value> {
    review
        .get("devResponses")
        .and_then(Value::as_array)?
        .iter()
        .max_by(|a, b| {
            let created = |v: &Value| text_field(v, "createdAt").unwrap_or_default();
            created(a).cmp(&created(b))
        })
}

fn answer_state(review: &Value, answers: Option<&Answers>) -> &'static str {
    if let Some(state) = answers.and_then(|answers| {
        text_field(review, "commentId").and_then(|id| answers.by_comment.get(&id).copied())
    }) {
        return state;
    }
    if latest_answer(review).is_some() {
        ANSWERED
    } else {
        UNANSWERED
    }
}

/// Текст не длиннее `max` символов; длинный обрезается с `…`.
fn shorten(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text.to_owned(), false);
    }
    let mut short: String = text.chars().take(max).collect();
    short.push('…');
    (short, true)
}

/// Дата, когда оставлен отзыв: ISO, иначе как в API.
fn comment_date(review: &Value) -> Option<String> {
    text_field(review, "commentDateIso").or_else(|| text_field(review, "commentDate"))
}

/// Запись последнего ответа: текст (сокращённый до `max`, если задан) и дата.
fn answer_record(review: &Value, max: Option<usize>) -> Option<Value> {
    let answer = latest_answer(review)?;
    let text = text_field(answer, "text").unwrap_or_default();
    let text = match max {
        Some(max) => shorten(&text, max).0,
        None => text,
    };
    let mut record = Map::new();
    record.insert("text".into(), json!(text));
    if let Some(created) = text_field(answer, "createdAt") {
        record.insert("createdAt".into(), json!(created));
    }
    Some(Value::Object(record))
}

/// Компактная запись отзыва для сводки: без имени автора и данных устройства.
fn summary_record(review: &Value, state: &str, max_text: usize) -> Value {
    let mut record = Map::new();
    for field in ["commentId", "appRating"] {
        if let Some(value) = review.get(field).filter(|v| !v.is_null()) {
            record.insert(field.into(), value.clone());
        }
    }
    if let Some(date) = comment_date(review) {
        record.insert("commentDate".into(), json!(date));
    }
    for field in ["appVersionName", "edited"] {
        if let Some(value) = review.get(field).filter(|v| !v.is_null()) {
            record.insert(field.into(), value.clone());
        }
    }
    let (text, truncated) = shorten(
        &text_field(review, "commentText").unwrap_or_default(),
        max_text,
    );
    record.insert("commentText".into(), json!(text));
    if truncated {
        record.insert("text_truncated".into(), json!(true));
    }
    record.insert("answer_status".into(), json!(state));
    if let Some(answer) = answer_record(review, Some(ANSWER_TEXT_LEN)) {
        record.insert("answer".into(), answer);
    }
    Value::Object(record)
}

/// Запись JSON Lines для выгрузки: полный текст, без данных устройства; имя автора — по
/// просьбе.
fn export_record(review: &Value, state: &str, include_user_name: bool) -> Value {
    let mut record = Map::new();
    if let Some(value) = review.get("commentId").filter(|v| !v.is_null()) {
        record.insert("commentId".into(), value.clone());
    }
    if include_user_name && let Some(name) = review.get("userName").filter(|v| !v.is_null()) {
        record.insert("userName".into(), name.clone());
    }
    if let Some(value) = review.get("appRating").filter(|v| !v.is_null()) {
        record.insert("appRating".into(), value.clone());
    }
    record.insert(
        "commentText".into(),
        json!(text_field(review, "commentText").unwrap_or_default()),
    );
    if let Some(date) = comment_date(review) {
        record.insert("commentDate".into(), json!(date));
    }
    let updated = text_field(review, "updatedAtIso").or_else(|| text_field(review, "updatedAt"));
    if let Some(updated) = updated {
        record.insert("updatedAt".into(), json!(updated));
    }
    for field in [
        "appVersionName",
        "appVersionCode",
        "edited",
        "likeCounter",
        "dislikeCounter",
    ] {
        if let Some(value) = review.get(field).filter(|v| !v.is_null()) {
            record.insert(field.into(), value.clone());
        }
    }
    record.insert("answer_status".into(), json!(state));
    if let Some(answer) = answer_record(review, None) {
        record.insert("answer".into(), answer);
    }
    Value::Object(record)
}

fn reviews_summary<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let filters = Filters::parse(&args)?;
        let limit = bounded_u64(&args, "limit", 20, 1, 100)? as usize;
        let scan = bounded_u64(&args, "scan", SUMMARY_SCAN_DEFAULT, 1, SUMMARY_SCAN_MAX)? as usize;
        let max_text = bounded_u64(&args, "max_text_length", 500, 20, 5000)? as usize;
        let with_answers = bool_arg(&args, "answer_statuses", true)?;

        let mut warnings = Vec::new();
        let app_rating = match ctx
            .call("get_app_rating", json!({"packageName": package}))
            .await
        {
            Ok(body) => Some(super::common::pick(
                &body,
                &[
                    "averageUserRating",
                    "totalRatings",
                    "ratings",
                    "ratingsNoComments",
                    "totalResponses",
                ],
            )),
            Err(err) => {
                warnings.push(err.message().to_owned());
                None
            }
        };
        let scanned = scan_reviews(
            ctx,
            &package,
            filters.from,
            scan,
            scan.div_ceil(PAGE_SIZE as usize),
        )
        .await?;
        let answers = if with_answers {
            match answer_statuses(ctx, &package).await {
                Ok(answers) => {
                    if !answers.complete {
                        warnings.push(format!(
                            "only the first {ANSWERS_MAX} developer answers were read; answer \
                             states of older reviews come from their published answers"
                        ));
                    }
                    Some(answers)
                }
                Err(err) => {
                    warnings.push(format!(
                        "{}; answer states come from the published answers of the reviews",
                        err.message()
                    ));
                    None
                }
            }
        } else {
            None
        };

        let mut by_rating: BTreeMap<String, u64> =
            (1..=5).map(|stars| (stars.to_string(), 0)).collect();
        let mut by_answer: BTreeMap<&str, u64> =
            ANSWER_STATES.iter().map(|state| (*state, 0)).collect();
        let mut matched = 0usize;
        let mut records = Vec::new();
        for review in &scanned.reviews {
            let state = answer_state(review, answers.as_ref());
            if !filters.matches(review, state) {
                continue;
            }
            matched += 1;
            if let Some(stars) = rating(review) {
                *by_rating.entry(stars.to_string()).or_default() += 1;
            }
            *by_answer.entry(state).or_default() += 1;
            if records.len() < limit {
                records.push(summary_record(review, state, max_text));
            }
        }

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        if let Some(app_rating) = app_rating {
            result.insert("rating".into(), app_rating);
        }
        if let Some(period) = filters.period() {
            result.insert("period".into(), period);
        }
        result.insert("scanned".into(), json!(scanned.reviews.len()));
        result.insert("complete".into(), json!(scanned.complete));
        result.insert("matched".into(), json!(matched));
        result.insert("returned".into(), json!(records.len()));
        result.insert("by_rating".into(), json!(by_rating));
        result.insert("by_answer_status".into(), json!(by_answer));
        result.insert("reviews".into(), Value::Array(records));
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        let mut notes = Vec::new();
        if matched > limit {
            notes.push(format!(
                "{} more reviews matched; raise `limit`, narrow the filters or use \
                 `reviews_export`",
                matched - limit
            ));
        }
        if !scanned.complete {
            notes.push(format!(
                "only the latest {} reviews were read; raise `scan` or narrow the period",
                scanned.reviews.len()
            ));
        }
        if !notes.is_empty() {
            result.insert("note".into(), json!(notes.join("; ")));
        }
        Ok(Value::Object(result))
    })
}

fn reviews_export<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let filters = Filters::parse(&args)?;
        let save_to = required_str(&args, "save_to")?;
        let format = optional_str(&args, "format")?.unwrap_or("auto");
        let include_user_name = bool_arg(&args, "include_user_name", false)?;
        let csv_period = match (filters.from, filters.to) {
            (Some(from), to) => {
                let to = to.unwrap_or_else(today_utc);
                (to - from < CSV_MAX_DAYS).then_some((from, to))
            }
            (None, _) => None,
        };
        let use_csv = match format {
            "csv" => {
                if filters.has_content_filters() {
                    return Err(ToolError::new(
                        "`format: csv` takes only a period: RuStore's CSV export has no star, \
                         answer or text filters; use `format: jsonl` (or `auto`) with them",
                    ));
                }
                if csv_period.is_none() {
                    return Err(ToolError::new(format!(
                        "`format: csv` needs a period of at most {CSV_MAX_DAYS} days: pass \
                         `from` and `to` or `days`; use `format: jsonl` for other periods"
                    )));
                }
                true
            }
            "jsonl" => false,
            "auto" => csv_period.is_some() && !filters.has_content_filters(),
            other => {
                return Err(ToolError::new(format!(
                    "argument `format` must be `auto`, `csv` or `jsonl`, got `{other}`"
                )));
            }
        };
        check_new_file(ctx, "save_to", save_to)?;

        if let (true, Some((from, to))) = (use_csv, csv_period) {
            let saved = ctx
                .call(
                    "export_reviews_csv",
                    json!({
                        "packageName": package,
                        "from": format_date(from),
                        "to": format_date(to),
                        "save_to": save_to,
                    }),
                )
                .await?;
            let path = text_field(&saved, "saved_to").unwrap_or_else(|| save_to.to_owned());
            let rows = std::fs::read(&path)
                .ok()
                .map(|bytes| csv_records(&bytes).saturating_sub(1));
            let mut result = Map::new();
            result.insert("format".into(), json!("csv"));
            result.insert("saved_to".into(), json!(path));
            if let Some(rows) = rows {
                result.insert("count".into(), json!(rows));
            }
            if let Some(bytes) = saved.get("bytes") {
                result.insert("bytes".into(), bytes.clone());
            }
            result.insert(
                "period".into(),
                json!({"from": format_date(from), "to": format_date(to)}),
            );
            result.insert(
                "note".into(),
                json!(
                    "RuStore's CSV export (a header line, then one review per record; texts \
                     may span lines). It includes reviewer names: keep the file local"
                ),
            );
            return Ok(Value::Object(result));
        }

        let scanned = scan_reviews(
            ctx,
            &package,
            filters.from,
            EXPORT_MAX_PAGES * PAGE_SIZE as usize,
            EXPORT_MAX_PAGES,
        )
        .await?;
        let mut warnings = Vec::new();
        let answers = match answer_statuses(ctx, &package).await {
            Ok(answers) => Some(answers),
            Err(err) => {
                warnings.push(format!(
                    "{}; answer states come from the published answers of the reviews",
                    err.message()
                ));
                None
            }
        };
        let mut lines = String::new();
        let mut count = 0usize;
        for review in &scanned.reviews {
            let state = answer_state(review, answers.as_ref());
            if !filters.matches(review, state) {
                continue;
            }
            let record = export_record(review, state, include_user_name);
            lines.push_str(&record.to_string());
            lines.push('\n');
            count += 1;
        }
        let path = write_new_file(ctx, "save_to", save_to, lines.as_bytes())?;
        let mut result = Map::new();
        result.insert("format".into(), json!("jsonl"));
        result.insert("saved_to".into(), json!(path.display().to_string()));
        result.insert("count".into(), json!(count));
        result.insert("scanned".into(), json!(scanned.reviews.len()));
        result.insert("complete".into(), json!(scanned.complete));
        if let Some(period) = filters.period() {
            result.insert("period".into(), period);
        }
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        if !scanned.complete {
            result.insert(
                "note".into(),
                json!(format!(
                    "only the latest {} reviews were read; narrow the period to export older \
                     ones",
                    scanned.reviews.len()
                )),
            );
        }
        Ok(Value::Object(result))
    })
}

/// Число записей CSV: переводы строк вне кавычек (текст отзыва может быть многострочным);
/// последняя строка без перевода строки тоже считается.
fn csv_records(bytes: &[u8]) -> usize {
    let mut records = 0;
    let mut quoted = false;
    let mut line_has_data = false;
    for &byte in bytes {
        match byte {
            b'"' => {
                quoted = !quoted;
                line_has_data = true;
            }
            b'\n' if !quoted => {
                if line_has_data {
                    records += 1;
                }
                line_has_data = false;
            }
            b'\r' if !quoted => {}
            _ => line_has_data = true,
        }
    }
    records + usize::from(line_has_data)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::common::parse_date;
    use crate::composite::testing;
    use crate::files::FilePolicy;

    const REVIEWS: &str = "/public/v1/application/com.example.app/comment";
    const ANSWERS: &str = "/public/v1/application/com.example.app/feedback";
    const RATING: &str = "/public/v1/application/com.example.app/comment/statistic";
    const EXPORT: &str = "/public/v1/application/com.example.app/comment/export";

    fn envelope(body: Value) -> Value {
        json!({"code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"})
    }

    fn ok(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(envelope(body))
    }

    fn ctx(server: &MockServer, files: FilePolicy) -> Context {
        testing::context(&server.uri(), AccessLevel::Read, &["com.example.app"], files).unwrap()
    }

    struct Dir {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Dir {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            Self { _dir: dir, root }
        }

        fn policy(&self) -> FilePolicy {
            FilePolicy::new(std::slice::from_ref(&self.root))
                .unwrap()
                .with_base_dir(&self.root)
        }

        fn file(&self, name: &str) -> String {
            self.root.join(name).display().to_string()
        }
    }

    async fn run(name: &str, ctx: &Context, args: Value) -> Result<Value, ToolError> {
        let tool = TOOLS.iter().find(|tool| tool.name == name).unwrap();
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        super::super::check_arguments(&(tool.input_schema)(), &args)?;
        (tool.run)(ctx, args).await
    }

    /// Отзыв с датой `day` (`ГГГГ-ММ-ДД`), оценкой и ответами разработчика.
    fn review(id: u64, stars: u64, day: &str, text: &str, answers: &[&str]) -> Value {
        let responses: Vec<Value> = answers
            .iter()
            .enumerate()
            .map(|(n, text)| json!({"text": text, "createdAt": format!("{day}T1{n}:00:00Z")}))
            .collect();
        json!({
            "packageName": "com.example.app", "appId": 1, "commentId": id,
            "userName": "Reviewer Name", "appRating": stars, "commentStatus": "PUBLISHED",
            "commentDate": format!("{day} 06:59:50.050"), "commentText": text,
            "likeCounter": 1, "dislikeCounter": 0, "updatedAt": format!("{day} 07:30:00.000"),
            "appVersionName": "1.4", "edited": false,
            "commentDateIso": format!("{day}T06:59:50.05Z"),
            "updatedAtIso": format!("{day}T07:30:00.0Z"), "devResponses": responses,
            "deviceInfo": {"device": "Example", "model": "X1", "osVersion": "Android 14"},
            "appVersionCode": 42
        })
    }

    /// Первая страница — полные 100 отзывов (за 2026-01-20), вторая — короткая.
    async fn mount_reviews(server: &MockServer) {
        let mut first: Vec<Value> = (0..100)
            .map(|n| review(1000 + n, 5, "2026-01-20", "Fine", &[]))
            .collect();
        first[0] = review(1, 1, "2026-01-20", "App CRASHES on start", &[]);
        first[1] = review(2, 2, "2026-01-20", "Crashes sometimes", &["Fixed in 1.5"]);
        first[2] = review(3, 1, "2026-01-20", "crash again", &[]);
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("page", "0"))
            .and(query_param("size", "100"))
            .respond_with(ok(json!(first)))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("page", "1"))
            .respond_with(ok(json!([
                review(4, 2, "2026-01-05", "Crash and slow", &[]),
                review(5, 4, "2025-12-30", "Good, one crash", &[]),
            ])))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_answers(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(ANSWERS))
            .and(query_param("page", "0"))
            .respond_with(ok(json!([
                {"id": 11, "commentId": 2, "text": "Fixed in 1.5", "status": "PUBLISHED",
                 "date": "2026-01-21 10:00:00.000"},
                {"id": 12, "commentId": 3, "text": "We are on it", "status": "MODERATION",
                 "date": "2026-01-21 10:00:00.000"},
                {"id": 13, "commentId": 4, "text": "Rude reply", "status": "REJECTED",
                 "date": "2026-01-21 10:00:00.000"},
                {"id": 14, "commentId": 1, "text": "Old answer", "status": "DELETED",
                 "date": "2026-01-21 10:00:00.000"}
            ])))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_rating(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(RATING))
            .respond_with(ok(json!({
                "ratings": {"amountFive": 100, "amountFour": 1, "amountThree": 0,
                            "amountTwo": 2, "amountOne": 2},
                "averageUserRating": 4.8, "totalRatings": 105, "totalResponses": 3,
                "ratingsNoComments": 0
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn summary_counts_filters_and_minimizes_records() {
        let server = MockServer::start().await;
        mount_reviews(&server).await;
        mount_answers(&server).await;
        mount_rating(&server).await;
        let ctx = ctx(&server, FilePolicy::disabled());

        let summary = run(
            "reviews_summary",
            &ctx,
            json!({"max_rating": 2, "text": "crash", "limit": 2, "max_text_length": 20}),
        )
        .await
        .unwrap();
        assert_eq!(summary["packageName"], "com.example.app");
        assert_eq!(summary["rating"]["averageUserRating"], 4.8);
        assert_eq!(summary["rating"]["ratings"]["amountOne"], 2);
        assert_eq!(summary["scanned"], 102);
        assert_eq!(summary["complete"], true);
        assert_eq!(summary["matched"], 4, "{summary}");
        assert_eq!(summary["by_rating"], json!({"1": 2, "2": 2, "3": 0, "4": 0, "5": 0}));
        assert_eq!(
            summary["by_answer_status"],
            json!({"answered": 1, "answer_in_moderation": 1, "answer_rejected": 1,
                   "unanswered": 1})
        );
        let first = &summary["reviews"][0];
        assert_eq!(first["commentId"], 1);
        assert_eq!(first["answer_status"], "unanswered");
        assert_eq!(first["commentText"], "App CRASHES on start");
        assert_eq!(first["commentDate"], "2026-01-20T06:59:50.05Z");
        // Персональные данные и устройство не попадают в сводку.
        for field in ["userName", "deviceInfo", "packageName", "appId"] {
            assert!(first.get(field).is_none(), "{field}");
        }
        let second = &summary["reviews"][1];
        assert_eq!(second["answer_status"], "answered");
        assert_eq!(second["answer"]["text"], "Fixed in 1.5");
        assert_eq!(summary["returned"], 2);
        assert!(summary["note"].as_str().unwrap().contains("2 more reviews"));
        assert!(summary.get("warnings").is_none(), "{summary}");
    }

    #[tokio::test]
    async fn summary_unanswered_and_period() {
        let server = MockServer::start().await;
        mount_reviews(&server).await;
        mount_answers(&server).await;
        mount_rating(&server).await;
        let ctx = ctx(&server, FilePolicy::disabled());

        let summary = run(
            "reviews_summary",
            &ctx,
            json!({"unanswered_only": true, "from": "2026-01-01", "to": "2026-01-19",
                   "max_text_length": 20}),
        )
        .await
        .unwrap();
        // Отзыв 4 — с отклонённым ответом, 5 — вне периода, страница 0 — позже периода.
        assert_eq!(summary["matched"], 1, "{summary}");
        assert_eq!(summary["reviews"][0]["commentId"], 4);
        assert_eq!(summary["reviews"][0]["answer_status"], "answer_rejected");
        assert_eq!(
            summary["period"],
            json!({"from": "2026-01-01", "to": "2026-01-19"})
        );
    }

    #[tokio::test]
    async fn summary_without_answer_statuses_and_with_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .respond_with(ok(json!([
                review(1, 5, "2026-01-20", &"long ".repeat(10), &["Thanks"]),
                review(2, 3, "2026-01-20", "ok", &[]),
            ])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(RATING))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({"code": "FORBIDDEN", "message": "No access to the method", "body": null}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(ANSWERS))
            .respond_with(ok(json!([])))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = ctx(&server, FilePolicy::disabled());

        let summary = run(
            "reviews_summary",
            &ctx,
            json!({"answer_statuses": false, "max_text_length": 20}),
        )
        .await
        .unwrap();
        assert!(summary.get("rating").is_none());
        let warnings = summary["warnings"].as_array().unwrap();
        assert!(
            warnings[0].as_str().unwrap().contains("get_app_rating"),
            "{warnings:?}"
        );
        assert_eq!(summary["reviews"][0]["answer_status"], "answered");
        assert_eq!(summary["reviews"][0]["text_truncated"], true);
        assert_eq!(
            summary["reviews"][0]["commentText"].as_str().unwrap().chars().count(),
            21
        );
        assert_eq!(summary["reviews"][1]["answer_status"], "unanswered");
    }

    #[tokio::test]
    async fn scan_stops_at_a_page_older_than_the_period() {
        let server = MockServer::start().await;
        let old: Vec<Value> = (0..100)
            .map(|n| review(n, 5, "2025-06-01", "Old", &[]))
            .collect();
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("page", "0"))
            .respond_with(ok(json!(old)))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .and(query_param("page", "1"))
            .respond_with(ok(json!([])))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = ctx(&server, FilePolicy::disabled());
        let from = parse_date("from", "2026-01-01").unwrap();
        let scan = scan_reviews(&ctx, "com.example.app", Some(from), 1000, 10)
            .await
            .unwrap();
        assert!(scan.complete);
        assert_eq!(scan.reviews.len(), 100);

        // Предел числа отзывов: обход неполный.
        let scan = scan_reviews(&ctx, "com.example.app", None, 50, 10)
            .await
            .unwrap();
        assert!(!scan.complete);
        assert_eq!(scan.reviews.len(), 50);
    }

    #[tokio::test]
    async fn filter_arguments_are_checked() {
        let server = MockServer::start().await;
        let ctx = ctx(&server, FilePolicy::disabled());
        for (args, expected) in [
            (json!({"from": "2026-02-01", "to": "2026-01-01"}), "later than"),
            (json!({"from": "2026-01-01", "days": 3}), "either `from` or `days`"),
            (json!({"min_rating": 4, "max_rating": 2}), "greater than"),
            (json!({"min_rating": 6}), "`min_rating`"),
            (json!({"from": "01.01.2026"}), "YYYY-MM-DD"),
            (json!({"userName": "x"}), "unknown argument"),
        ] {
            let err = run("reviews_summary", &ctx, args.clone())
                .await
                .unwrap_err();
            assert!(err.message().contains(expected), "{args}: {err}");
        }
        // `days` отсчитывается от `to`.
        let a = json!({"days": 7, "to": "2026-01-31"});
        let filters = Filters::parse(a.as_object().unwrap()).unwrap();
        assert_eq!(filters.from.map(format_date).as_deref(), Some("2026-01-25"));
        assert!(!filters.has_content_filters());
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn export_writes_json_lines_and_never_overwrites() {
        let server = MockServer::start().await;
        mount_reviews(&server).await;
        mount_answers(&server).await;
        let dir = Dir::new();
        let ctx = ctx(&server, dir.policy());

        let result = run(
            "reviews_export",
            &ctx,
            json!({"save_to": "negative.jsonl", "max_rating": 2}),
        )
        .await
        .unwrap();
        assert_eq!(result["format"], "jsonl");
        assert_eq!(result["count"], 4);
        assert_eq!(result["complete"], true);
        assert_eq!(result["saved_to"], dir.file("negative.jsonl"));
        // Агенту — только путь и число записей.
        assert!(result.get("reviews").is_none());
        let lines = read_lines(&dir.root.join("negative.jsonl"));
        let ids: Vec<u64> = lines
            .iter()
            .map(|line| line["commentId"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(lines[1]["answer"]["text"], "Fixed in 1.5");
        assert_eq!(lines[2]["answer_status"], "answer_in_moderation");
        assert_eq!(lines[0]["appVersionCode"], 42);
        assert!(lines[0].get("userName").is_none());
        assert!(lines[0].get("deviceInfo").is_none());

        // Существующий файл не перезаписывается, запросов к API нет.
        let before = std::fs::read(dir.root.join("negative.jsonl")).unwrap();
        let err = run(
            "reviews_export",
            &ctx,
            json!({"save_to": "negative.jsonl", "max_rating": 2}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        assert_eq!(std::fs::read(dir.root.join("negative.jsonl")).unwrap(), before);
    }

    #[tokio::test]
    async fn export_jsonl_with_user_names_on_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .respond_with(ok(json!([review(7, 3, "2026-01-20", "Meh", &[])])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(ANSWERS))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({"code": "FORBIDDEN", "message": "No access to the method", "body": null}),
            ))
            .mount(&server)
            .await;
        let dir = Dir::new();
        let ctx = ctx(&server, dir.policy());
        let result = run(
            "reviews_export",
            &ctx,
            json!({"save_to": dir.file("all.jsonl"), "include_user_name": true,
                   "format": "jsonl", "from": "2026-01-01", "to": "2026-01-31"}),
        )
        .await
        .unwrap();
        assert_eq!(result["count"], 1);
        assert!(result["warnings"][0].as_str().unwrap().contains("get_review_answer_statuses"));
        let lines = read_lines(&dir.root.join("all.jsonl"));
        assert_eq!(lines[0]["userName"], "Reviewer Name");
        assert_eq!(lines[0]["answer_status"], "unanswered");
    }

    #[tokio::test]
    async fn export_uses_rustore_csv_for_a_period() {
        let server = MockServer::start().await;
        let csv = "Package Name,Star Rating,Review Text,User name,Comment Id\r\n\
                   com.example.app,5,\"Great,\nreally \"\"great\"\"\",Name,1\r\n\
                   com.example.app,1,Bad,Name,2";
        Mock::given(method("GET"))
            .and(path(EXPORT))
            .and(query_param("from", "2026-01-01"))
            .and(query_param("to", "2026-01-31"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_string(csv),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(REVIEWS))
            .respond_with(ok(json!([])))
            .expect(0)
            .mount(&server)
            .await;
        let dir = Dir::new();
        let ctx = ctx(&server, dir.policy());

        let result = run(
            "reviews_export",
            &ctx,
            json!({"save_to": "reviews.csv", "from": "2026-01-01", "to": "2026-01-31"}),
        )
        .await
        .unwrap();
        assert_eq!(result["format"], "csv");
        assert_eq!(result["count"], 2, "{result}");
        assert_eq!(result["saved_to"], dir.file("reviews.csv"));
        assert_eq!(
            std::fs::read_to_string(dir.root.join("reviews.csv")).unwrap(),
            csv
        );

        // Существующий файл — ошибка до запроса (у мока expect(1)).
        let err = run(
            "reviews_export",
            &ctx,
            json!({"save_to": "reviews.csv", "from": "2026-01-01", "to": "2026-01-31"}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");

        // CSV не умеет других фильтров и периодов длиннее 92 дней.
        for (args, expected) in [
            (
                json!({"save_to": "x.csv", "format": "csv", "from": "2026-01-01",
                       "min_rating": 4}),
                "takes only a period",
            ),
            (
                json!({"save_to": "x.csv", "format": "csv", "from": "2025-01-01",
                       "to": "2025-12-31"}),
                "at most 92 days",
            ),
            (json!({"save_to": "x.csv", "format": "csv"}), "needs a period"),
            (json!({"save_to": "x.csv", "format": "xml"}), "`format`"),
            (json!({"from": "2026-01-01"}), "`save_to`"),
        ] {
            let err = run("reviews_export", &ctx, args.clone()).await.unwrap_err();
            assert!(err.message().contains(expected), "{args}: {err}");
        }
    }

    #[test]
    fn csv_records_are_counted_outside_quotes() {
        assert_eq!(csv_records(b""), 0);
        assert_eq!(csv_records(b"h\r\n"), 1);
        assert_eq!(csv_records(b"h\na\nb\n"), 3);
        assert_eq!(csv_records(b"h\n\"multi\nline\",x\nlast"), 3);
        assert_eq!(csv_records(b"h\n\n\na\n"), 2);
    }

    #[test]
    fn answer_states() {
        let mut answers = Answers {
            by_comment: HashMap::new(),
            complete: true,
        };
        answers.by_comment.insert("1".into(), IN_MODERATION);
        let with_published = review(1, 5, "2026-01-01", "x", &["Thanks"]);
        assert_eq!(answer_state(&with_published, Some(&answers)), IN_MODERATION);
        assert_eq!(answer_state(&with_published, None), ANSWERED);
        let without = review(2, 5, "2026-01-01", "x", &[]);
        assert_eq!(answer_state(&without, Some(&answers)), UNANSWERED);
        let latest = review(3, 5, "2026-01-01", "x", &["first", "second"]);
        assert_eq!(
            answer_record(&latest, None).unwrap()["text"],
            json!("second")
        );
    }
}
