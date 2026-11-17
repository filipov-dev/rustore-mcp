//! `wait_for_moderation` (чтение, набор `releases`): ждёт, пока версия выйдет из
//! автопроверки и модерации, опрашивая `get_version` с интервалом и пределом времени.
//!
//! Предел — не ошибка: модерация RuStore может идти часами, поэтому по истечении предела
//! инструмент возвращает текущий статус (`timed_out: true`), и агент вызывает его снова.
//! Значения по умолчанию подобраны под таймауты клиентов MCP: многие клиенты (SDK MCP для
//! TypeScript, Codex) по умолчанию ждут ответа на вызов 60 секунд, поэтому предел по
//! умолчанию — 50 секунд, а верхняя граница — 10 минут для клиентов с увеличенным таймаутом.
//! Интервал не короче 5 секунд, чтобы опрос не расходовал лимиты запросов RuStore API.
//!
//! Ход ожидания уходит клиенту уведомлениями `notifications/progress` ([`Context::report_progress`]),
//! время и сон — через [`Context::clock`], в тестах часы ручные.

use std::time::Duration;

use rmcp::model::JsonObject;
use rustore_api::spec::AccessLevel;
use serde_json::{Map, Value, json};

use super::super::common::{
    bounded_u64, object_schema, optional_u64, package_name, package_name_property, text_field,
};
use super::super::{CompositeTool, Context, ToolFuture};
use super::{find_versions, get_version, status_of, version_state};
use crate::error::ToolError;

pub(super) const WAIT_FOR_MODERATION: CompositeTool = CompositeTool {
    name: "wait_for_moderation",
    toolset: "releases",
    access: AccessLevel::Read,
    title: "Wait for the moderation result of a version",
    description: "Waits until a RuStore version leaves automatic checking and moderation \
        (AUTO_CHECK, MODERATION, TAKEN_FOR_MODERATION), polling `get_version` every \
        `interval_seconds` for at most `timeout_seconds`, and returns the final status: \
        `outcome` is `approved` (READY_FOR_PUBLICATION), `published` (ACTIVE, or \
        PARTIAL_ACTIVE with the rollout percent), `rejected` (AUTO_CHECK_FAILED, \
        REJECTED_BY_MODERATOR, REJECTED_BY_SECURITY, with the moderation comment \
        `statusComment` as `reason`), `not_submitted` (still a draft) or `other`. Use it \
        after `release_submit` instead of polling by hand; pass the `versionId` it returned \
        (without it the newest version in review is used). When the time limit runs out the \
        tool is not an error: it returns the current status with `timed_out: true` and \
        `outcome: in_review`, because RuStore moderation can take hours; call it again later. \
        The default limit, 50 seconds, stays below the 60-second request timeout many MCP \
        clients use; up to 600 seconds is allowed for clients with a longer tool timeout. \
        The interval is 15 seconds by default and at least 5, to keep the request rate low. \
        Progress is reported with MCP progress notifications when the client asks for them. \
        It only reads: approving or publishing is done by RuStore and `release_rollout`.",
    input_schema: schema,
    uses_local_files: false,
    run,
};

/// Статусы проверки: автопроверка и модерация.
const IN_REVIEW: [&str; 3] = ["AUTO_CHECK", "MODERATION", "TAKEN_FOR_MODERATION"];
/// Статусы отклонённой версии; причина — в `statusComment`.
const REJECTED: [&str; 3] = [
    "AUTO_CHECK_FAILED",
    "REJECTED_BY_MODERATOR",
    "REJECTED_BY_SECURITY",
];

/// Предел ожидания по умолчанию и наибольший, секунды (обоснование — в описании модуля).
const TIMEOUT_DEFAULT: u64 = 50;
const TIMEOUT_MAX: u64 = 600;
/// Интервал опроса по умолчанию и пределы, секунды.
const INTERVAL_DEFAULT: u64 = 15;
const INTERVAL_MIN: u64 = 5;
const INTERVAL_MAX: u64 = 300;
/// После стольких неудачных опросов подряд ожидание прекращается: ошибка, скорее всего,
/// не временная (нет доступа, версия удалена).
const MAX_FAILED_POLLS: usize = 3;

fn schema() -> Value {
    object_schema(
        json!({
            "packageName": package_name_property(),
            "versionId": {
                "type": "integer",
                "description": "ID of the version to wait for (as returned by \
                    `release_submit`); default — the newest version in AUTO_CHECK, MODERATION \
                    or TAKEN_FOR_MODERATION."
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 0,
                "maximum": TIMEOUT_MAX,
                "description": "How long to wait at most, in seconds (default 50, at most \
                    600; 0 checks once). When it runs out, the current status is returned \
                    with `timed_out: true`."
            },
            "interval_seconds": {
                "type": "integer",
                "minimum": INTERVAL_MIN,
                "maximum": INTERVAL_MAX,
                "description": "Seconds between two status checks (default 15, from 5 to \
                    300)."
            }
        }),
        &[],
    )
}

fn run<'a>(ctx: &'a Context, args: JsonObject) -> ToolFuture<'a> {
    Box::pin(async move {
        let package = package_name(ctx, &args)?;
        let timeout = bounded_u64(&args, "timeout_seconds", TIMEOUT_DEFAULT, 0, TIMEOUT_MAX)?;
        let interval = bounded_u64(
            &args,
            "interval_seconds",
            INTERVAL_DEFAULT,
            INTERVAL_MIN,
            INTERVAL_MAX,
        )?;
        let version_id = match optional_u64(&args, "versionId")? {
            Some(id) => id,
            None => newest_in_review(ctx, &package).await?,
        };
        let timeout = Duration::from_secs(timeout);
        let interval = Duration::from_secs(interval);

        let clock = ctx.clock();
        let started = clock.now();
        let mut details = get_version(ctx, &package, version_id).await?;
        let mut status = status_of(&details).unwrap_or_default();
        let mut polls = 1usize;
        let mut history = vec![json!({"status": status, "after_seconds": 0})];
        let mut failed_in_row = 0usize;
        let mut warnings: Vec<String> = Vec::new();
        loop {
            let elapsed = clock.now().saturating_sub(started);
            if !IN_REVIEW.contains(&status.as_str())
                || elapsed >= timeout
                || failed_in_row >= MAX_FAILED_POLLS
            {
                break;
            }
            let pause = interval.min(timeout - elapsed);
            ctx.report_progress(
                elapsed.as_secs_f64(),
                Some(timeout.as_secs_f64()),
                format!(
                    "version {version_id} is {status} after {}s; next check in {}s",
                    elapsed.as_secs(),
                    pause.as_secs()
                ),
            )
            .await;
            clock.sleep(pause).await;
            polls += 1;
            match get_version(ctx, &package, version_id).await {
                Ok(next) => {
                    failed_in_row = 0;
                    let next_status = status_of(&next).unwrap_or_default();
                    if next_status != status {
                        let after = clock.now().saturating_sub(started).as_secs();
                        history.push(json!({"status": next_status, "after_seconds": after}));
                    }
                    status = next_status;
                    details = next;
                }
                Err(err) => {
                    failed_in_row += 1;
                    warnings.push(format!("check {polls} failed: {}", err.message()));
                }
            }
        }
        let waited = clock.now().saturating_sub(started);
        let in_review = IN_REVIEW.contains(&status.as_str());

        let mut result = Map::new();
        result.insert("packageName".into(), json!(package));
        result.insert("versionId".into(), json!(version_id));
        result.insert("status".into(), json!(status));
        result.insert("outcome".into(), json!(outcome(&status)));
        result.insert("finished".into(), json!(!in_review));
        result.insert(
            "timed_out".into(),
            json!(in_review && failed_in_row < MAX_FAILED_POLLS),
        );
        if REJECTED.contains(&status.as_str()) {
            let reason = text_field(&details, "statusComment").filter(|text| !text.is_empty());
            if reason.is_none() {
                warnings.push(
                    "RuStore returned no moderation comment; the user can read the details \
                     in the RuStore console"
                        .to_owned(),
                );
            }
            result.insert("reason".into(), json!(reason));
        }
        result.insert("waited_seconds".into(), json!(waited.as_secs()));
        result.insert("checks".into(), json!(polls));
        result.insert("history".into(), Value::Array(history));
        result.insert("version".into(), version_state(&details));
        result.insert(
            "next_step".into(),
            json!(next_step(&status, &details, failed_in_row)),
        );
        if !warnings.is_empty() {
            result.insert("warnings".into(), json!(warnings));
        }
        Ok(Value::Object(result))
    })
}

/// Самая новая версия на проверке (релизные и альфа-версии).
async fn newest_in_review(ctx: &Context, package: &str) -> Result<u64, ToolError> {
    let versions = find_versions(ctx, package, &IN_REVIEW).await?;
    versions
        .first()
        .and_then(super::version_id_of)
        .ok_or_else(|| {
            ToolError::new(format!(
                "no version of `{package}` is in AUTO_CHECK, MODERATION or \
                 TAKEN_FOR_MODERATION; pass the `versionId` of the submitted version \
                 (`release_summary` shows the versions by status)"
            ))
        })
}

/// Итог проверки по статусу версии.
fn outcome(status: &str) -> &'static str {
    match status {
        s if IN_REVIEW.contains(&s) => "in_review",
        s if REJECTED.contains(&s) => "rejected",
        "READY_FOR_PUBLICATION" => "approved",
        "ACTIVE" | "PARTIAL_ACTIVE" => "published",
        "DRAFT" => "not_submitted",
        _ => "other",
    }
}

/// Что делать дальше: подсказка агенту по итоговому статусу.
fn next_step(status: &str, details: &Value, failed_in_row: usize) -> String {
    let publish_type = text_field(details, "publishType").map(|s| s.to_uppercase());
    match status {
        s if IN_REVIEW.contains(&s) && failed_in_row >= MAX_FAILED_POLLS => {
            "the status checks failed several times in a row (see `warnings`); fix the cause \
             before waiting again"
                .to_owned()
        }
        s if IN_REVIEW.contains(&s) => {
            "moderation is still going on: call `wait_for_moderation` again later (RuStore \
             moderation can take hours), or check with `release_summary`"
                .to_owned()
        }
        s if REJECTED.contains(&s) => {
            "the version was rejected: show the user the `reason`, fix the build or the texts \
             in a draft and submit it again with `release_submit` after the user confirms"
                .to_owned()
        }
        "READY_FOR_PUBLICATION" => match publish_type.as_deref() {
            Some("MANUAL") => "approved: publish it with `release_rollout` (action `publish`) \
                after the user confirms"
                .to_owned(),
            Some("DELAYED") => {
                "approved: RuStore publishes it automatically at `publishDateTime`".to_owned()
            }
            _ => "approved and waiting for publication; `release_rollout` with action \
                `status` shows the next step"
                .to_owned(),
        },
        "PARTIAL_ACTIVE" => {
            let share = text_field(details, "partialValue")
                .map(|value| format!(" to {value}% of users"))
                .unwrap_or_default();
            format!(
                "published{share}: raise the rollout percent with `release_rollout` (action \
                 `set_percent`) after the user confirms"
            )
        }
        "ACTIVE" => "published to all users".to_owned(),
        "DRAFT" => {
            "the version is still a draft: send it to moderation with `release_submit` after \
             the user confirms"
                .to_owned()
        }
        other => format!("the version is {other}, not in moderation"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::testing;
    use crate::files::FilePolicy;
    use crate::progress::{Clock, Progress, ProgressUpdate};

    const V2: &str = "/public/v2/application/com.example.app/version";

    fn ok(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK", "message": null, "body": body, "timestamp": "2026-01-01T00:00:00Z"
        }))
    }

    fn version(status: &str) -> Value {
        json!({
            "versionId": 7, "versionName": "1.7", "versionStatus": status,
            "publishType": "MANUAL", "appName": "Example"
        })
    }

    /// Контекст с ручными часами и записью сообщений о ходе.
    fn context(server: &MockServer) -> (Context, Clock, Arc<Mutex<Vec<ProgressUpdate>>>) {
        let clock = Clock::manual();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let progress = Progress::new(move |update| {
            sink.lock().unwrap().push(update);
            Box::pin(async {})
        });
        let ctx = testing::context(
            &server.uri(),
            AccessLevel::Read,
            &["com.example.app"],
            FilePolicy::disabled(),
        )
        .unwrap()
        .with_clock(clock.clone())
        .with_progress(progress);
        (ctx, clock, seen)
    }

    async fn run_tool(ctx: &Context, args: Value) -> Result<Value, ToolError> {
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        crate::composite::check_arguments(&(WAIT_FOR_MODERATION.input_schema)(), &args)?;
        (WAIT_FOR_MODERATION.run)(ctx, args).await
    }

    /// Отвечает статусами по очереди: каждый ответ — `times` раз.
    async fn statuses(server: &MockServer, steps: &[(Value, u64)]) {
        for (body, times) in steps {
            Mock::given(method("GET"))
                .and(path(format!("{V2}/7")))
                .respond_with(ok(body.clone()))
                .up_to_n_times(*times)
                .expect(*times)
                .mount(server)
                .await;
        }
    }

    #[tokio::test]
    async fn waits_until_approved() {
        let server = MockServer::start().await;
        statuses(
            &server,
            &[
                (version("AUTO_CHECK"), 1),
                (version("MODERATION"), 2),
                (version("READY_FOR_PUBLICATION"), 1),
            ],
        )
        .await;
        let (ctx, clock, seen) = context(&server);
        let result = run_tool(
            &ctx,
            json!({"versionId": 7, "timeout_seconds": 600, "interval_seconds": 20}),
        )
        .await
        .unwrap();
        assert_eq!(result["outcome"], "approved", "{result}");
        assert_eq!(result["status"], "READY_FOR_PUBLICATION");
        assert_eq!(result["finished"], true);
        assert_eq!(result["timed_out"], false);
        assert_eq!(result["checks"], 4);
        assert_eq!(result["waited_seconds"], 60);
        assert!(result.get("reason").is_none(), "{result}");
        let history: Vec<&str> = result["history"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["status"].as_str().unwrap())
            .collect();
        assert_eq!(
            history,
            ["AUTO_CHECK", "MODERATION", "READY_FOR_PUBLICATION"]
        );
        assert_eq!(result["history"][1]["after_seconds"], 20);
        assert!(
            result["next_step"]
                .as_str()
                .unwrap()
                .contains("`release_rollout`"),
            "{result}"
        );
        assert_eq!(clock.now(), Duration::from_secs(60));
        // Сообщение о ходе — перед каждой паузой, с растущим `progress`.
        let seen = seen.lock().unwrap();
        let progress: Vec<f64> = seen.iter().map(|u| u.progress).collect();
        assert_eq!(progress, [0.0, 20.0, 40.0]);
        assert!(seen.iter().all(|u| u.total == Some(600.0)));
        assert!(seen[1].message.contains("MODERATION after 20s"), "{seen:?}");
    }

    #[tokio::test]
    async fn rejection_returns_the_reason() {
        let server = MockServer::start().await;
        let mut rejected = version("REJECTED_BY_MODERATOR");
        rejected["statusComment"] = json!("Screenshots do not match the app");
        statuses(
            &server,
            &[(version("TAKEN_FOR_MODERATION"), 1), (rejected, 1)],
        )
        .await;
        let (ctx, _clock, _seen) = context(&server);
        let result = run_tool(&ctx, json!({"versionId": "7"})).await.unwrap();
        assert_eq!(result["outcome"], "rejected", "{result}");
        assert_eq!(result["reason"], "Screenshots do not match the app");
        assert_eq!(
            result["version"]["statusComment"],
            "Screenshots do not match the app"
        );
        assert_eq!(result["waited_seconds"], 15);
        assert!(
            result["next_step"]
                .as_str()
                .unwrap()
                .contains("`release_submit`")
        );
    }

    #[tokio::test]
    async fn time_limit_returns_the_current_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("{V2}/7")))
            .respond_with(ok(version("MODERATION")))
            .expect(5)
            .mount(&server)
            .await;
        let (ctx, clock, seen) = context(&server);
        let result = run_tool(&ctx, json!({"versionId": 7})).await.unwrap();
        // Предел по умолчанию 50 с, интервал 15 с: опросы в 0, 15, 30 и 45 с, последняя
        // пауза короче (5 с), и пятый опрос — ровно на пределе.
        assert_eq!(result["outcome"], "in_review", "{result}");
        assert_eq!(result["finished"], false);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["status"], "MODERATION");
        assert_eq!(clock.now(), Duration::from_secs(50));
        assert_eq!(result["waited_seconds"], 50);
        assert_eq!(result["checks"], 5);
        assert!(
            result["next_step"]
                .as_str()
                .unwrap()
                .contains("again later")
        );
        assert_eq!(seen.lock().unwrap().len(), result["checks"].as_u64().unwrap() as usize - 1);
    }

    #[tokio::test]
    async fn finds_the_version_in_review_and_checks_once_without_time() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application/com.example.app/version"))
            .and(query_param("versionStatuses", "AUTO_CHECK,MODERATION,TAKEN_FOR_MODERATION"))
            .respond_with(ok(json!({
                "content": [
                    {"versionId": 5, "versionStatus": "MODERATION"},
                    {"versionId": 7, "versionStatus": "AUTO_CHECK"}
                ],
                "pageNumber": 0, "totalPages": 1, "totalElements": 2
            })))
            .expect(1)
            .mount(&server)
            .await;
        statuses(&server, &[(version("AUTO_CHECK"), 1)]).await;
        let (ctx, clock, seen) = context(&server);
        let result = run_tool(&ctx, json!({"timeout_seconds": 0})).await.unwrap();
        assert_eq!(result["versionId"], 7);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["checks"], 1);
        assert_eq!(clock.now(), Duration::ZERO);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_version_in_review_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/public/v1/application/com.example.app/version"))
            .respond_with(ok(json!({
                "content": [], "pageNumber": 0, "totalPages": 0, "totalElements": 0
            })))
            .mount(&server)
            .await;
        let (ctx, _clock, _seen) = context(&server);
        let err = run_tool(&ctx, json!({})).await.unwrap_err();
        assert!(err.message().contains("pass the `versionId`"), "{err}");

        let err = run_tool(&ctx, json!({"versionId": 7, "interval_seconds": 1}))
            .await
            .unwrap_err();
        assert!(err.message().contains("`interval_seconds`"), "{err}");
    }

    #[tokio::test]
    async fn failing_checks_stop_the_wait() {
        let server = MockServer::start().await;
        statuses(&server, &[(version("MODERATION"), 1)]).await;
        Mock::given(method("GET"))
            .and(path(format!("{V2}/7")))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({"code": "FORBIDDEN", "message": "Access denied", "body": null}),
            ))
            .expect(3)
            .mount(&server)
            .await;
        let (ctx, _clock, _seen) = context(&server);
        let result = run_tool(&ctx, json!({"versionId": 7, "timeout_seconds": 600}))
            .await
            .unwrap();
        assert_eq!(result["status"], "MODERATION");
        assert_eq!(result["timed_out"], false);
        assert_eq!(result["warnings"].as_array().unwrap().len(), 3);
        assert!(result["next_step"].as_str().unwrap().contains("failed"));
    }

    #[test]
    fn api_search_finds_the_tool_in_russian() {
        let registry = crate::registry::ToolRegistry::new(&crate::ServerOptions::default()).unwrap();
        let args = json!({"query": "дождаться модерации", "limit": 3});
        let Value::Object(args) = args else {
            panic!("не объект")
        };
        let text = crate::api_search::search(&args, &registry).unwrap();
        assert!(text.contains("wait_for_moderation"), "{text}");
    }

    #[test]
    fn outcomes_cover_the_statuses() {
        assert_eq!(outcome("AUTO_CHECK_FAILED"), "rejected");
        assert_eq!(outcome("REJECTED_BY_SECURITY"), "rejected");
        assert_eq!(outcome("PARTIAL_ACTIVE"), "published");
        assert_eq!(outcome("DRAFT"), "not_submitted");
        assert_eq!(outcome("ARCHIVED"), "other");
        let partial = json!({"partialValue": 25});
        assert!(next_step("PARTIAL_ACTIVE", &partial, 0).contains("to 25% of users"));
    }
}
