//! Подсказки MCP (prompts): готовые сценарии для частых задач RuStore — выпуск версии,
//! разбор отзывов с черновиками ответов, сверка платежей за период, проверка состояния всех
//! приложений кабинета.
//!
//! Контракт с сервером: [`available`] отбирает подсказки по уровню доступа сервера (подсказка,
//! которой нужен уровень выше, не регистрируется), сервер отдаёт их в `prompts/list`
//! ([`info`]) и рендерит текст в `prompts/get` ([`get`]: неизвестные и недостающие
//! обязательные аргументы отклоняются до рендера). Подсказки только описывают шаги для агента
//! и ссылаются на инструменты по имени; запросов они не делают. Текст зависит от окружения
//! сервера ([`PromptEnv`]): в HTTP-режиме нет инструментов с локальными файлами, без
//! `--allow-destructive` нет публичных ответов на отзывы.
//!
//! Тексты — для агента, на английском. Имена инструментов в тексте пишутся в обратных
//! кавычках, аргументы — без них: тесты проверяют, что каждое такое имя существует
//! (операции, составные инструменты или служебные инструменты сервера), входит в набор из
//! строки «Toolsets used here», а пометки `(write…)` и `(destructive…)` после имени совпадают
//! с уровнем доступа инструмента.

use std::collections::BTreeMap;

use rmcp::ErrorData;
use rmcp::model::{
    GetPromptRequestParams, GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage,
    Role,
};
use rustore_api::spec::AccessLevel;
use serde_json::Value;

/// Аргумент подсказки.
#[derive(Debug, Clone, Copy)]
pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// Окружение сервера, от которого зависит текст подсказки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptEnv {
    /// Уровень доступа сервера.
    pub max_access: AccessLevel,
    /// Сервер на машине агента (stdio): есть инструменты с локальными файлами.
    pub local_machine: bool,
}

/// Описание подсказки.
#[derive(Clone, Copy)]
pub struct PromptDef {
    /// Имя подсказки (snake_case).
    pub name: &'static str,
    /// Заголовок для клиента.
    pub title: &'static str,
    /// Описание для клиента и агента (английский).
    pub description: &'static str,
    /// Наименьший уровень доступа, при котором сценарий выполним; на сервере с меньшим
    /// уровнем подсказка не регистрируется.
    pub access: AccessLevel,
    pub arguments: &'static [PromptArg],
    /// Текст сообщения пользователя по аргументам. Обязательные аргументы проверяет
    /// [`get`] до вызова.
    pub render: fn(&BTreeMap<String, String>, &PromptEnv) -> String,
}

impl std::fmt::Debug for PromptDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptDef")
            .field("name", &self.name)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// Все подсказки сервера.
pub fn all() -> Vec<PromptDef> {
    vec![
        RELEASE_VERSION,
        TRIAGE_REVIEWS,
        RECONCILE_PAYMENTS,
        APPS_HEALTH_CHECK,
    ]
}

/// Подсказки, выполнимые на уровне `max_access`.
pub fn available(max_access: AccessLevel) -> Vec<PromptDef> {
    all()
        .into_iter()
        .filter(|prompt| max_access.allows(prompt.access))
        .collect()
}

/// Описание подсказки для `prompts/list`.
pub fn info(prompt: &PromptDef) -> Prompt {
    let arguments = prompt
        .arguments
        .iter()
        .map(|arg| {
            PromptArgument::new(arg.name)
                .with_description(arg.description)
                .with_required(arg.required)
        })
        .collect();
    let mut info = Prompt::new(prompt.name, Some(prompt.description), Some(arguments));
    if !prompt.title.is_empty() {
        info = info.with_title(prompt.title);
    }
    info
}

/// `prompts/get`: находит подсказку по имени, проверяет аргументы и рендерит текст.
pub fn get(
    prompts: &[PromptDef],
    env: &PromptEnv,
    request: GetPromptRequestParams,
) -> Result<GetPromptResult, ErrorData> {
    let Some(prompt) = prompts.iter().find(|p| p.name == request.name) else {
        let known: Vec<String> = prompts.iter().map(|p| format!("`{}`", p.name)).collect();
        return Err(ErrorData::invalid_params(
            format!(
                "unknown prompt `{}`; known prompts: {}",
                request.name,
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            ),
            None,
        ));
    };
    let values = arguments(prompt, request.arguments)?;
    let text = (prompt.render)(&values, env);
    let mut result = GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)]);
    result.description = Some(prompt.description.to_owned());
    Ok(result)
}

/// Аргументы подсказки строками: неизвестные и недостающие обязательные отклоняются, пустые
/// значения считаются отсутствующими.
fn arguments(
    prompt: &PromptDef,
    args: Option<JsonObject>,
) -> Result<BTreeMap<String, String>, ErrorData> {
    let mut values = BTreeMap::new();
    for (name, value) in args.unwrap_or_default() {
        if !prompt.arguments.iter().any(|arg| arg.name == name) {
            let allowed: Vec<String> = prompt
                .arguments
                .iter()
                .map(|arg| format!("`{}`", arg.name))
                .collect();
            return Err(ErrorData::invalid_params(
                format!(
                    "prompt `{}` has no argument `{name}`; its arguments: {}",
                    prompt.name,
                    if allowed.is_empty() {
                        "none".to_owned()
                    } else {
                        allowed.join(", ")
                    }
                ),
                None,
            ));
        }
        let text = match value {
            Value::Null => continue,
            Value::String(text) => text,
            other => other.to_string(),
        };
        if !text.trim().is_empty() {
            values.insert(name, text);
        }
    }
    let missing: Vec<String> = prompt
        .arguments
        .iter()
        .filter(|arg| arg.required && !values.contains_key(arg.name))
        .map(|arg| format!("`{}`", arg.name))
        .collect();
    if !missing.is_empty() {
        return Err(ErrorData::invalid_params(
            format!(
                "prompt `{}` needs argument(s): {}",
                prompt.name,
                missing.join(", ")
            ),
            None,
        ));
    }
    Ok(values)
}

/// Значение аргумента без пробелов по краям; пустая строка — аргумента нет.
fn arg<'a>(args: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    args.get(name).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// Приложение подсказки: аргумент `package_name` или пакет профиля по умолчанию.
fn app(args: &BTreeMap<String, String>) -> String {
    match arg(args, "package_name") {
        Some(name) => format!("app {name}"),
        None => "the app the user means (the profile's default package if it has exactly one; \
                 otherwise find it with `search_apps` and ask the user)"
            .to_owned(),
    }
}

/// Аргумент packageName для текста шагов.
fn package(args: &BTreeMap<String, String>) -> String {
    match arg(args, "package_name") {
        Some(name) => format!("packageName {name}"),
        None => "packageName of the app".to_owned(),
    }
}

const PACKAGE_NAME: PromptArg = PromptArg {
    name: "package_name",
    description: "Package name of the app, for example com.example.app; default — the \
                  profile's default package.",
    required: false,
};

const ACCESS_NOTE: &str = "If a tool below is not available, the server may run with dynamic \
toolsets: call `list_toolsets` and turn on the needed toolsets with `enable_toolset`, or the \
toolset is not in --toolsets: ask the user to add it. A tool above the server's access level is \
not registered: tell the user which flag enables it (--allow-write or --allow-destructive) \
instead of working around it with `api_request`.";

const DESTRUCTIVE_RULE: &str = "Destructive steps reach real RuStore users or money. Before \
each of them tell the user exactly what will happen, get an explicit yes for that step, and \
never treat an earlier yes as consent for a later step.";

const PERSONAL_DATA: &str = "Responses contain personal data of RuStore users: use it only for \
this task and do not copy it into files, commits, wiki pages or other systems unless the user \
asks for exactly that.";

// ---------------------------------------------------------------------------------------
// release_version

const RELEASE_VERSION: PromptDef = PromptDef {
    name: "release_version",
    title: "Release a new RuStore version",
    description: "Release a new version of an app in RuStore: upload the build to the draft, \
                  update the texts and screenshots, send it to moderation, wait for the \
                  result and publish or roll it out. Sending to moderation, publishing and \
                  raising the rollout are destructive steps, each confirmed with the user.",
    access: AccessLevel::Destructive,
    arguments: &[
        PACKAGE_NAME,
        PromptArg {
            name: "build",
            description: "Path to the .aab or .apk to upload; default — ask the user.",
            required: false,
        },
        PromptArg {
            name: "whats_new",
            description: "Release notes (What's new) of the version; default — ask the user \
                          or take them from the changelog.",
            required: false,
        },
        PromptArg {
            name: "publish_type",
            description: "INSTANTLY (publish right after moderation), MANUAL (publish by \
                          hand after approval, the default here) or DELAYED.",
            required: false,
        },
        PromptArg {
            name: "rollout_percent",
            description: "Staged rollout: the first percent of users (5, 10, 25, 50, 75 or \
                          100); default — all users.",
            required: false,
        },
    ],
    render: render_release_version,
};

fn render_release_version(args: &BTreeMap<String, String>, env: &PromptEnv) -> String {
    let app = app(args);
    let package = package(args);
    let publish_type = arg(args, "publish_type")
        .map(str::to_uppercase)
        .unwrap_or_else(|| "MANUAL".to_owned());
    let rollout = match arg(args, "rollout_percent") {
        Some(percent) => format!(" and partialValue {percent} (a staged rollout)"),
        None => String::new(),
    };
    let whats_new = match arg(args, "whats_new") {
        Some(text) => format!("Use these release notes (What's new): {text}"),
        None => "Ask the user for the release notes (What's new) or take them from the \
                 changelog they point to, and get the text approved"
            .to_owned(),
    };
    let build_and_texts = if env.local_machine {
        let build = match arg(args, "build") {
            Some(path) => format!("the file {path}"),
            None => "the build the user names (ask for the path to the .aab or .apk)".to_owned(),
        };
        format!(
            "2. Build: call `upload_build` (write) with {package}, {build} (aab for an .aab, \
apks for .apk files) and whatsNew. It fills the existing draft or creates one, uploads the \
files and returns the versionId and the draft state; the version code must be higher than the \
published one. Users see nothing yet.
3. Texts: {whats_new}. For other listing texts export the current ones with \
`listing_export` (draft true), edit them with the user and apply them with `listing_apply` \
(write): first with dry_run true to show the differences, then for real.
4. Screenshots (only if the user wants to change them): `sync_screenshots` (write) with the \
user's directory, dry_run true first, then for real. `list_screenshots` shows what the draft \
has."
        )
    } else {
        format!(
            "2. Build: this server runs in HTTP mode and cannot read local files, so ask the \
user to upload the build in the RuStore console (or use a local server). Then find the draft \
with `list_versions` (versionStatuses DRAFT, filterTestingType ALL) and read it with \
`get_version`.
3. Texts: {whats_new}. Set whatsNew and other listing texts of the draft with \
`update_draft_version` (write); show the user the changed fields before sending them.
4. Screenshots: uploads are not available in HTTP mode; ask the user to change them in the \
RuStore console if needed. `list_screenshots` shows what the draft has."
        )
    };
    format!(
        "Release a new version of {app} in RuStore with publishType {publish_type}{rollout}. \
Report every step as done / missing / needs the user.

{ACCESS_NOTE} Toolsets used here: releases, listing, apps, reviews.

{DESTRUCTIVE_RULE} The destructive steps of this release are: sending the draft to moderation \
with `release_submit` (destructive) — with publishType INSTANTLY an approved version reaches \
users without another call; publishing with `release_rollout` (destructive) action publish; \
raising the rollout percent with `release_rollout` action set_percent (RuStore cannot lower \
it). They need a server started with --allow-destructive.

1. State: call `release_summary` ({package}). Note what is live (and the rollout percent), \
what is in moderation, the draft if there is one, and the moderation comment of the last \
rejected version: fix the same problem in this release. An app has only one draft; continue \
it instead of creating another.
{build_and_texts}
5. Check: read the draft with `get_version` and show the user the summary: version name and \
code, files, What's new, publishType {publish_type}{rollout}.
6. Submit (destructive): call `release_submit` with {package}, publishType {publish_type}{rollout} \
and dry_run true; show the planned calls and what happens after moderation. Only after the \
user's explicit yes call it again without dry_run. After the submit the draft cannot be \
edited.
7. Moderation: call `wait_for_moderation` with the versionId from `release_submit`. It \
reports progress and returns the status; when it returns timed_out true, moderation is still \
going on: tell the user and call it again later instead of polling with other tools. If the \
outcome is rejected, show the user the reason and go back to step 2 with a fixed build or \
texts.
8. Publish (destructive, publishType MANUAL): when the outcome is approved, ask the user for \
an explicit yes, run `release_rollout` with action publish (and partialValue for a staged \
rollout) with dry_run true, then for real. With INSTANTLY RuStore publishes by itself, with \
DELAYED at publishDateTime.
9. Rollout (destructive): for a staged rollout check the reviews of the new version with \
`reviews_summary` (days 1 or 2) before every step; raise the percent with `release_rollout` \
action set_percent (dry_run first, then for real after the user's yes) until 100.
10. Report the version, its status, the rollout percent and what is left."
    )
}

// ---------------------------------------------------------------------------------------
// triage_reviews

const TRIAGE_REVIEWS: PromptDef = PromptDef {
    name: "triage_reviews",
    title: "Triage reviews and draft replies",
    description: "Read recent RuStore reviews of an app, group them by theme, rating and app \
                  version, and draft replies. A reply is posted only after the user approves \
                  its exact text, and only on a server with --allow-destructive.",
    access: AccessLevel::Read,
    arguments: &[
        PACKAGE_NAME,
        PromptArg {
            name: "days",
            description: "How many recent days of reviews to read (default 14).",
            required: false,
        },
    ],
    render: render_triage_reviews,
};

fn render_triage_reviews(args: &BTreeMap<String, String>, env: &PromptEnv) -> String {
    let app = app(args);
    let package = package(args);
    let days = arg(args, "days").unwrap_or("14");
    let posting = if env.max_access.allows(AccessLevel::Destructive) {
        "6. Posting (destructive, only approved texts): `reply_to_review` with the commentId \
and the approved text in body.message; `update_review_answer` (destructive) changes an \
existing answer. Replies are public and go through RuStore moderation: check them later with \
`get_review_answer_status`. Post each reply only after the user's explicit yes to its final \
wording."
    } else {
        "6. Posting: this server cannot post replies — `reply_to_review` (destructive) needs \
the server started with --allow-destructive. Give the user the approved texts to post in the \
RuStore console, or ask them to restart the server with that flag; never post without the \
user's explicit yes to the final wording."
    };
    let export = if env.local_machine {
        " For a larger period save the reviews to a new local file with `reviews_export` and \
read the file locally."
    } else {
        ""
    };
    format!(
        "Triage the reviews of {app} for the last {days} days and draft replies.

{ACCESS_NOTE} Toolsets used here: reviews, releases, apps.

{DESTRUCTIVE_RULE} The destructive step here is posting or changing a public reply \
(`reply_to_review` (destructive), `update_review_answer` (destructive)); everything else only \
reads.

1. Overview: call `reviews_summary` ({package}, days {days}, limit 100). Note the rating \
statistics, the counts by stars and by answer state and the reviews without an answer.{export}
2. Group: count the reviews by star rating and app version and group them by theme (crashes \
and bugs, performance, payments and subscriptions, login, ads, feature requests, UX, praise, \
other). For each theme give the count, the typical rating, the versions and one or two short \
paraphrased examples. Relate themes to releases with `release_summary` ({package}).
3. Prioritise: low ratings with a concrete problem, questions, unanswered reviews and answers \
rejected by moderation (`unanswered_only` in `reviews_summary` lists only unanswered ones; \
pass unanswered_only true and max_rating 3 for a short list).
4. Draft replies: one per selected review, in the language of the review, from 1 to 500 \
characters, polite and specific to what the user wrote, without promises of dates, without \
personal data and without asking for passwords or payment details. Show each draft next to the \
review it answers (use the commentId).
5. Approval: ask the user to approve, edit or reject every draft one by one.
{posting}
7. Report what was posted, what is waiting for moderation and what is left.

{PERSONAL_DATA}"
    )
}

// ---------------------------------------------------------------------------------------
// reconcile_payments

const RECONCILE_PAYMENTS: PromptDef = PromptDef {
    name: "reconcile_payments",
    title: "Reconcile payments for a period",
    description: "Sum up the RuStore payments of an app for a period (by status, product and \
                  currency) and reconcile them with the user's own records. Read-only: \
                  refunds and cancellations are not part of it and need the user's explicit \
                  confirmation.",
    access: AccessLevel::Read,
    arguments: &[
        PACKAGE_NAME,
        PromptArg {
            name: "from",
            description: "First day, YYYY-MM-DD; default — the first day of the previous \
                          month.",
            required: false,
        },
        PromptArg {
            name: "to",
            description: "Last day, YYYY-MM-DD, inclusive; default — the last day of the \
                          previous month. At most 92 days from the first day.",
            required: false,
        },
        PromptArg {
            name: "records",
            description: "Where the user's records are (an accounting export, a bank \
                          statement, a CSV file); default — ask the user.",
            required: false,
        },
    ],
    render: render_reconcile_payments,
};

fn render_reconcile_payments(args: &BTreeMap<String, String>, env: &PromptEnv) -> String {
    let app = app(args);
    let package = package(args);
    let period = match (arg(args, "from"), arg(args, "to")) {
        (Some(from), Some(to)) => format!("from {from} to {to}"),
        (Some(from), None) => format!("from {from} to today (at most 92 days)"),
        (None, Some(to)) => format!("of the calendar month that ends with {to}"),
        (None, None) => {
            "of the previous calendar month (work out the dates from today's date)".to_owned()
        }
    };
    let records = match arg(args, "records") {
        Some(records) => format!("the user's records in {records}"),
        None => "the user's records (ask where they are: an accounting export, a bank \
                 statement or a CSV file)"
            .to_owned(),
    };
    let csv = if env.local_machine {
        " Pass save_to with a new local CSV file when invoice-level matching is needed: the \
file has one row per payment (invoiceId, dates, status, product, amounts, currency)."
    } else {
        ""
    };
    format!(
        "Reconcile the RuStore payments of {app} for the period {period} with {records}.

{ACCESS_NOTE} Toolsets used here: purchases, apps.

{DESTRUCTIVE_RULE} This task only reads. Refunds (`refund_payment` (destructive)), \
cancellations and confirmations of purchases change money and are not part of the \
reconciliation: if the user asks for one, name the invoice and the amount, get an explicit yes \
and use a server started with --allow-destructive. Never run them in the sandbox tools \
(`_sandbox`) by mistake or the real ones for test purchases.

1. App: payments use the numeric appId. Find it with `search_apps` (it returns appId) or pass \
{package} to `payments_report`, which resolves it through `list_apps`.
2. Totals: call `payments_report` ({package}, from and to for the period, at most 92 days per \
call: split a longer period). Note the counts and sums by status, by product and the totals \
per currency.{csv} Amounts are integers in minor units (kopecks for RUB): divide by 100 for \
roubles.
3. Records: read {records} and bring them to the same shape: date, amount, currency, status \
(paid, refunded), product, and the invoice ID if there is one.
4. Match: compare the totals per day or per product first, then individual payments by \
invoiceId (or by date, amount and product). Watch for refunds in a later period, payments \
created near midnight (RuStore dates are by invoice date) and statuses that are not final \
(CREATED, EXECUTED).
5. Investigate: for a disputed payment read it with `get_payment` (invoiceId) and show the \
status, dates and amounts.
6. Report: the totals on both sides, the differences with their likely cause and the list of \
payments to check. Do not change anything.

Financial data is confidential and contains personal data of buyers: {PERSONAL_DATA}"
    )
}

// ---------------------------------------------------------------------------------------
// apps_health_check

const APPS_HEALTH_CHECK: PromptDef = PromptDef {
    name: "apps_health_check",
    title: "Check the state of all apps",
    description: "Go through every app of the RuStore developer account: versions live, in \
                  moderation, rejected or stuck in a draft or a partial rollout, the rating \
                  and unanswered negative reviews. Read-only; suggests next steps.",
    access: AccessLevel::Read,
    arguments: &[PromptArg {
        name: "days",
        description: "How many recent days of reviews to check per app (default 7).",
        required: false,
    }],
    render: render_apps_health_check,
};

fn render_apps_health_check(args: &BTreeMap<String, String>, _env: &PromptEnv) -> String {
    let days = arg(args, "days").unwrap_or("7");
    format!(
        "Check the state of every app of the RuStore developer account of this server's key.

{ACCESS_NOTE} Toolsets used here: apps, releases, reviews, catalog, access.

{DESTRUCTIVE_RULE} This check only reads. If it finds something to fix, suggest the step: \
sending a draft to moderation (`release_submit` (destructive)), publishing or widening a \
rollout (`release_rollout` (destructive)) or replying to reviews (`reply_to_review` \
(destructive)) happen only after the user's explicit yes and on a server started with \
--allow-destructive.

1. Apps: call `search_apps` without query to list every app the key can access (packageName, \
appName, appId, appStatus). The key may be limited to some apps in the RuStore console: say \
so if the user expects more apps; `auth_check` shows which API sections and apps the key \
can reach.
2. Versions: for each app call `release_summary` (packageName of the app). Flag: a version \
in moderation (how long since sendDateForModer), a rejected version with its moderation \
comment, a version approved but not published (READY_FOR_PUBLICATION with publishType MANUAL), \
a partial rollout below 100 percent, an old draft. `wait_for_moderation` with timeout_seconds 0 \
checks one version in moderation once.
3. Reviews: for each app call `reviews_summary` (packageName of the app, days {days}, \
max_rating 2, unanswered_only true, limit 10) and note the average rating, the number of new \
negative reviews and the unanswered ones. `get_app_rating` gives the rating alone.
4. Optional, when the user asks about money or people: `catalog_summary` (products and \
subscriptions, apps without monetization) and `access_summary` (users and roles per app).
5. Report: a table with one row per app (status, live version and rollout, in moderation or \
rejected, rating, unanswered negative reviews) and below it the problems ordered by urgency \
with the suggested next step and its access level.

{PERSONAL_DATA}"
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rustore_api::spec;

    use super::*;
    use crate::composite;

    /// Служебные инструменты сервера (не операции и не составные).
    const SERVER_TOOLS: [&str; 5] = [
        "api_search",
        "api_request",
        "list_toolsets",
        "enable_toolset",
        "disable_toolset",
    ];

    const ENVS: [PromptEnv; 4] = [
        PromptEnv {
            max_access: AccessLevel::Destructive,
            local_machine: true,
        },
        PromptEnv {
            max_access: AccessLevel::Destructive,
            local_machine: false,
        },
        PromptEnv {
            max_access: AccessLevel::Read,
            local_machine: true,
        },
        PromptEnv {
            max_access: AccessLevel::Write,
            local_machine: false,
        },
    ];

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Тексты подсказки без аргументов и со всеми аргументами во всех окружениях, где она
    /// регистрируется.
    fn renders(prompt: &PromptDef) -> Vec<String> {
        let mut all_args = BTreeMap::new();
        for arg in prompt.arguments {
            all_args.insert(arg.name.to_string(), "42".to_string());
        }
        let mut texts = Vec::new();
        for env in ENVS
            .iter()
            .filter(|env| env.max_access.allows(prompt.access))
        {
            texts.push((prompt.render)(&BTreeMap::new(), env));
            texts.push((prompt.render)(&all_args, env));
        }
        texts
    }

    /// Слова в обратных кавычках, похожие на имена инструментов, и текст после них.
    fn backticked(text: &str) -> Vec<(String, String)> {
        let parts: Vec<&str> = text.split('`').collect();
        parts
            .iter()
            .enumerate()
            .skip(1)
            .step_by(2)
            .filter(|(_, w)| {
                !w.is_empty()
                    && w.starts_with(|c: char| c.is_ascii_lowercase())
                    && w.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            })
            .map(|(i, w)| {
                let after = parts.get(i + 1).copied().unwrap_or_default();
                (w.to_string(), after.to_string())
            })
            .collect()
    }

    /// Уровень доступа и набор инструмента: операция или составной инструмент.
    fn tool(name: &str) -> Option<(AccessLevel, &'static str)> {
        spec::find(name)
            .map(|op| (op.access, op.toolset))
            .or_else(|| {
                composite::all()
                    .into_iter()
                    .find(|t| t.name == name)
                    .map(|t| (t.access, t.toolset))
            })
    }

    /// Аргументы составных инструментов, которые подсказки упоминают в обратных кавычках.
    const ARGUMENTS: [&str; 1] = ["unanswered_only"];

    #[test]
    fn prompts_have_unique_names_and_arguments() {
        let prompts = all();
        let names: BTreeSet<_> = prompts.iter().map(|p| p.name).collect();
        assert_eq!(names.len(), prompts.len());
        for prompt in &prompts {
            assert!(!prompt.title.is_empty() && !prompt.description.is_empty());
            let args: BTreeSet<_> = prompt.arguments.iter().map(|a| a.name).collect();
            assert_eq!(args.len(), prompt.arguments.len(), "{}", prompt.name);
        }
    }

    #[test]
    fn prompts_are_filtered_by_access() {
        let names = |level| -> Vec<&str> { available(level).iter().map(|p| p.name).collect() };
        assert_eq!(
            names(AccessLevel::Read),
            ["triage_reviews", "reconcile_payments", "apps_health_check"]
        );
        assert_eq!(names(AccessLevel::Write), names(AccessLevel::Read));
        assert_eq!(
            names(AccessLevel::Destructive),
            [
                "release_version",
                "triage_reviews",
                "reconcile_payments",
                "apps_health_check"
            ]
        );
        // Уровень подсказки — не ниже уровня каждого инструмента, без которого сценарий
        // невыполним: выпуск отправляет версию на модерацию.
        let release = all().into_iter().find(|p| p.name == "release_version");
        assert_eq!(release.map(|p| p.access), Some(AccessLevel::Destructive));
    }

    #[test]
    fn prompt_tool_names_exist() {
        let known: BTreeSet<String> = spec::operations()
            .iter()
            .map(|op| op.tool_name.to_string())
            .chain(composite::all().iter().map(|t| t.name.to_string()))
            .chain(SERVER_TOOLS.iter().map(|s| s.to_string()))
            .chain(ARGUMENTS.iter().map(|s| s.to_string()))
            .collect();
        for prompt in all() {
            let mut found = BTreeSet::new();
            let mut texts = renders(&prompt);
            texts.push(prompt.description.to_owned());
            for text in texts {
                for (name, _) in backticked(&text) {
                    assert!(
                        known.contains(&name),
                        "prompt {} mentions unknown tool `{name}`",
                        prompt.name
                    );
                    found.insert(name);
                }
            }
            assert!(
                found.len() > 4,
                "prompt {} mentions too few tools",
                prompt.name
            );
        }
    }

    /// Инструменты из подсказки входят в наборы, которые подсказка просит включить.
    #[test]
    fn prompt_tools_belong_to_listed_toolsets() {
        let slugs: BTreeSet<&str> = spec::toolsets().iter().map(|t| t.slug).collect();
        for prompt in all() {
            for text in renders(&prompt) {
                let line = text
                    .split("Toolsets used here:")
                    .nth(1)
                    .and_then(|rest| rest.split('.').next())
                    .unwrap_or_else(|| panic!("prompt {} lists no toolsets", prompt.name));
                let listed: BTreeSet<&str> = line.split(',').map(str::trim).collect();
                for slug in &listed {
                    assert!(
                        slugs.contains(slug),
                        "{}: unknown toolset {slug}",
                        prompt.name
                    );
                }
                for (name, _) in backticked(&text) {
                    if let Some((_, toolset)) = tool(&name) {
                        assert!(
                            listed.contains(toolset),
                            "prompt {}: `{name}` is in toolset {toolset}, not in: {line}",
                            prompt.name
                        );
                    }
                }
            }
        }
    }

    /// Пометки `(write…)` и `(destructive…)` после имени совпадают с уровнем доступа;
    /// каждый инструмент уровня записи и удаления помечен хотя бы раз, а подсказки с
    /// инструментами удаления требуют явного согласия пользователя.
    #[test]
    fn access_marks_match_access_levels() {
        let mut marked = 0;
        for prompt in all() {
            for text in renders(&prompt) {
                for (name, after) in backticked(&text) {
                    let Some((access, _)) = tool(&name) else {
                        continue;
                    };
                    let after = after.trim_start();
                    let expected = if after.starts_with("(destructive") {
                        Some(AccessLevel::Destructive)
                    } else if after.starts_with("(write") {
                        Some(AccessLevel::Write)
                    } else {
                        None
                    };
                    if let Some(expected) = expected {
                        assert_eq!(access, expected, "prompt {}: `{name}`", prompt.name);
                        marked += 1;
                    } else {
                        assert!(
                            access == AccessLevel::Read || text.contains(&format!("`{name}` (")),
                            "prompt {}: `{name}` needs an access mark",
                            prompt.name
                        );
                    }
                }
                assert!(
                    text.contains("explicit yes") && text.contains("Destructive steps"),
                    "prompt {} must name its destructive steps",
                    prompt.name
                );
            }
        }
        assert!(marked > 10);
    }

    #[test]
    fn render_uses_arguments_and_defaults() {
        let local = ENVS[0];
        let remote = ENVS[1];
        let release = render_release_version(&BTreeMap::new(), &local);
        assert!(release.contains("publishType MANUAL"), "{release}");
        assert!(release.contains("profile's default package"), "{release}");
        assert!(release.contains("`upload_build` (write)"), "{release}");
        assert!(release.contains("`wait_for_moderation`"), "{release}");
        let release = render_release_version(
            &args(&[
                ("package_name", "com.example.app"),
                ("build", "app/build/app.aab"),
                ("publish_type", "instantly"),
                ("rollout_percent", "10"),
                ("whats_new", "Bug fixes"),
            ]),
            &local,
        );
        assert!(release.contains("app com.example.app"), "{release}");
        assert!(release.contains("packageName com.example.app"), "{release}");
        assert!(release.contains("the file app/build/app.aab"), "{release}");
        assert!(
            release.contains("publishType INSTANTLY and partialValue 10"),
            "{release}"
        );
        assert!(release.contains("What's new): Bug fixes"), "{release}");
        // В HTTP-режиме нет инструментов с локальными файлами.
        let release = render_release_version(&BTreeMap::new(), &remote);
        assert!(release.contains("HTTP mode"), "{release}");
        for absent in ["`upload_build`", "`listing_apply`", "`sync_screenshots`"] {
            assert!(!release.contains(absent), "{absent}: {release}");
        }

        let reviews = render_triage_reviews(&BTreeMap::new(), &local);
        assert!(reviews.contains("last 14 days"), "{reviews}");
        assert!(reviews.contains("`reviews_export`"), "{reviews}");
        assert!(reviews.contains("Posting (destructive"), "{reviews}");
        let reviews = render_triage_reviews(&args(&[("days", "30")]), &ENVS[2]);
        assert!(reviews.contains("days 30"), "{reviews}");
        assert!(reviews.contains("cannot post replies"), "{reviews}");

        let payments = render_reconcile_payments(&BTreeMap::new(), &local);
        assert!(payments.contains("previous calendar month"), "{payments}");
        assert!(payments.contains("save_to"), "{payments}");
        let payments = render_reconcile_payments(
            &args(&[
                ("from", "2026-10-01"),
                ("to", "2026-10-31"),
                ("records", "books.csv"),
            ]),
            &remote,
        );
        assert!(
            payments.contains("from 2026-10-01 to 2026-10-31"),
            "{payments}"
        );
        assert!(payments.contains("records in books.csv"), "{payments}");
        assert!(!payments.contains("save_to"), "{payments}");

        let health = render_apps_health_check(&BTreeMap::new(), &local);
        assert!(health.contains("days 7"), "{health}");
        let health = render_apps_health_check(&args(&[("days", "3")]), &local);
        assert!(health.contains("days 3"), "{health}");
    }

    fn request(name: &str, value: Value) -> GetPromptRequestParams {
        let mut request = GetPromptRequestParams::new(name);
        if let Value::Object(map) = value {
            request = request.with_arguments(map);
        }
        request
    }

    #[test]
    fn get_checks_arguments() {
        let prompts = available(AccessLevel::Read);
        let env = ENVS[2];
        let ok = get(
            &prompts,
            &env,
            request(
                "triage_reviews",
                serde_json::json!({"package_name": "com.example.app", "days": 3}),
            ),
        )
        .unwrap();
        assert_eq!(ok.messages.len(), 1);
        let text = serde_json::to_string(&ok.messages[0]).unwrap();
        assert!(text.contains("com.example.app"), "{text}");
        assert!(text.contains("days 3"), "{text}");

        let err = get(
            &prompts,
            &env,
            request("triage_reviews", serde_json::json!({"x": "1"})),
        )
        .unwrap_err();
        assert!(err.message.contains("no argument `x`"), "{}", err.message);
        // Подсказки уровня удаления на сервере чтения нет.
        let err = get(&prompts, &env, request("release_version", Value::Null)).unwrap_err();
        assert!(err.message.contains("unknown prompt"), "{}", err.message);
    }

    #[test]
    fn required_arguments_are_checked() {
        const NEEDS_DIR: PromptDef = PromptDef {
            name: "needs_dir",
            title: "Test",
            description: "Test prompt.",
            access: AccessLevel::Read,
            arguments: &[PromptArg {
                name: "dir",
                description: "Directory.",
                required: true,
            }],
            render: |args, _| format!("dir {}", arg(args, "dir").unwrap_or("?")),
        };
        let env = ENVS[2];
        let err = get(
            &[NEEDS_DIR],
            &env,
            request("needs_dir", serde_json::json!({"dir": " "})),
        )
        .unwrap_err();
        assert!(err.message.contains("`dir`"), "{}", err.message);
        let ok = get(
            &[NEEDS_DIR],
            &env,
            request("needs_dir", serde_json::json!({"dir": "m"})),
        )
        .unwrap();
        assert_eq!(ok.description.as_deref(), Some("Test prompt."));
    }
}
