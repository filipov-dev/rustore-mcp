//! Обработчик MCP-сервера `rmcp` без привязки к транспорту.
//!
//! Набор инструментов динамический (строится из таблицы операций), поэтому `ServerHandler`
//! реализован вручную, без макросов `#[tool]`. Транспорт подключает бинарник:
//! `RuStoreServer::new(...)?.serve(stdio())`.
//!
//! Ключ RuStore API и токены доступа ядро не видит: они в клиенте API, который передаёт
//! бинарник (источник токенов по ключу профиля, в HTTP-режиме — по ключу на стороне
//! сервера). Аргументы инструментов ключей и токенов не принимают.
//!
//! Состояние сессии — текущий реестр инструментов: в режиме динамических наборов
//! (`ServerOptions::dynamic_toolsets`) `enable_toolset`/`disable_toolset` подменяют его снимок,
//! и клиент получает `notifications/tools/list_changed`: в сессии с `initialize` — сразу после
//! вызова, в протоколе 2026-07-28 — через подписку `subscriptions/listen`. Клоны сервера делят
//! сессию; транспорт с несколькими сессиями создаёт их через [`RuStoreServer::new_session`].

use std::sync::{Arc, RwLock};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig,
    SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use rustore_api::executor::{ApiClient, ApiRequest, ApiResponse, ResponseBody};
use rustore_api::spec::{AccessLevel, OperationDef};
use serde_json::Value;

use crate::ServerOptions;
use crate::api_request::{self, API_REQUEST_TOOL};
use crate::api_search;
use crate::composite::{Context, check_arguments};
use crate::dynamic_toolsets;
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::jq::{FILTER_INPUT_LIMIT, JqFilter, JqRunner, take_jq, takes_jq};
use crate::redact::{redact_api_error, redact_json, redact_message, redact_response};
use crate::registry::{ToolKind, ToolRegistry, access_name};
use crate::request::build_request;
use crate::response::{adjust_pagination, failure, response_text, success};

/// Имя сервера в `initialize`.
pub const SERVER_NAME: &str = "rustore-mcp";

/// MCP-сервер RuStore API. Дёшево клонируется; клоны делят сессию.
#[derive(Debug, Clone)]
pub struct RuStoreServer {
    inner: Arc<Inner>,
    session: Arc<Session>,
}

#[derive(Debug)]
struct Inner {
    client: ApiClient,
    options: ServerOptions,
    files: FilePolicy,
    /// Окружение составных инструментов: тот же клиент, файлы и настройки.
    context: Context,
    /// Реестр при запуске; с него начинается каждая сессия.
    initial: Arc<ToolRegistry>,
    instructions: String,
}

/// Изменяемое состояние одной MCP-сессии.
#[derive(Debug)]
struct Session {
    registry: RwLock<Arc<ToolRegistry>>,
    /// Счётчик изменений списка инструментов: по нему подписки `subscriptions/listen` узнают,
    /// что пора отправить `notifications/tools/list_changed`.
    changes: tokio::sync::watch::Sender<u64>,
}

impl Session {
    fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry: RwLock::new(registry),
            changes: tokio::sync::watch::Sender::new(0),
        }
    }

    fn registry(&self) -> Arc<ToolRegistry> {
        // Отравленная блокировка не мешает: внутри только `Arc`, его замена атомарна.
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn generation(&self) -> u64 {
        *self.changes.borrow()
    }
}

impl RuStoreServer {
    /// Создаёт сервер с клиентом API (и его источником токенов).
    /// Ошибка — неизвестный набор инструментов в `options.toolsets` или конфликт имён
    /// пользовательских инструментов.
    ///
    /// Без [`ServerOptions::local_machine`] политика файлов принудительно
    /// [`FilePolicy::disabled`], какой бы ни передали: удалённый сервер не работает с файлами
    /// своей машины от имени агента.
    pub fn new(
        client: ApiClient,
        options: ServerOptions,
        files: FilePolicy,
    ) -> Result<Self, String> {
        let files = if options.local_machine {
            files
        } else {
            FilePolicy::disabled()
        };
        let registry = Arc::new(ToolRegistry::new(&options)?);
        let instructions = server_instructions(&options, &registry, &files);
        let context = Context::for_server(client.clone(), files.clone(), &options);
        Ok(Self {
            session: Arc::new(Session::new(registry.clone())),
            inner: Arc::new(Inner {
                client,
                options,
                files,
                context,
                initial: registry,
                instructions,
            }),
        })
    }

    /// Тот же сервер с новой сессией: наборы инструментов — как при запуске. Клон
    /// (`clone()`) делит сессию с исходным сервером.
    pub fn new_session(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            session: Arc::new(Session::new(self.inner.initial.clone())),
        }
    }

    /// Текущий реестр инструментов сессии (снимок).
    pub fn registry(&self) -> Arc<ToolRegistry> {
        self.session.registry()
    }

    pub fn instructions(&self) -> &str {
        &self.inner.instructions
    }

    pub fn options(&self) -> &ServerOptions {
        &self.inner.options
    }

    /// Вызов инструмента без транспорта. Ошибки возвращаются как `isError: true`;
    /// `Err` — только для неизвестного инструмента (ошибка протокола).
    pub async fn call(&self, name: &str, args: JsonObject) -> Result<CallToolResult, ErrorData> {
        let registry = self.registry();
        let Some(tool) = registry.get(name) else {
            // Известный, но выключенный инструмент: агенту объясняем, какой флаг нужен.
            return match registry.unavailable_reason(name) {
                Some(reason) => Ok(failure(&ToolError::new(reason))),
                None => Err(ErrorData::invalid_params(
                    format!("unknown tool `{name}`"),
                    None,
                )),
            };
        };
        // Поиск и управление наборами не обращаются к API: токен им не нужен.
        let local = match tool.kind {
            ToolKind::ApiSearch => Some(api_search::search(&args, &registry)),
            ToolKind::ListToolsets => Some(dynamic_toolsets::list(&args, &registry)),
            ToolKind::EnableToolset => Some(self.toggle_toolsets(&args, true)),
            ToolKind::DisableToolset => Some(self.toggle_toolsets(&args, false)),
            ToolKind::Operation(_)
            | ToolKind::Composite(_)
            | ToolKind::ApiRequest
            | ToolKind::Custom(_) => None,
        };
        if let Some(result) = local {
            return Ok(match result {
                Ok(text) => success(text),
                Err(err) => failure(&err),
            });
        }
        let client = &self.inner.client;
        let result = match tool.kind {
            ToolKind::Operation(op) => self.call_operation(client, op, args).await,
            ToolKind::Composite(index) => self.call_composite(index, &tool.tool, args).await,
            ToolKind::ApiRequest => self.call_api_request(client, args).await,
            ToolKind::Custom(index) => self.call_custom(client, index, args).await,
            ToolKind::ApiSearch
            | ToolKind::ListToolsets
            | ToolKind::EnableToolset
            | ToolKind::DisableToolset => {
                // Обработаны выше, без токена.
                Err(ToolError::new(format!("tool `{name}` is not an API call")))
            }
        };
        // Защита в глубину: токен не попадает в ответ агенту, даже если API его отразил.
        // Тексты ошибок, кроме ответа RuStore, проходят ещё и проверку значений по форме:
        // в них бывают пути и детали соединения.
        Ok(match result {
            Ok(text) => success(client.scrub_token(text)),
            Err(err) => {
                let err = ToolError::new(redact_message(
                    &client.scrub_token(err.message().to_owned()),
                ));
                tracing::debug!(tool = name, error = %err, "инструмент завершился ошибкой");
                failure(&err)
            }
        })
    }

    /// `enable_toolset`/`disable_toolset`: пересобирает реестр сессии. Замена делается под
    /// блокировкой записи от текущего реестра, чтобы параллельные вызовы не потеряли изменения.
    fn toggle_toolsets(&self, args: &JsonObject, enable: bool) -> Result<String, ToolError> {
        let mut guard = self
            .session
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let toggled = dynamic_toolsets::toggle(&guard, args, enable)?;
        if let Some(next) = toggled.registry {
            tracing::info!(
                toolsets = ?next.enabled_toolsets(),
                tools = next.len(),
                "наборы инструментов изменены"
            );
            *guard = Arc::new(next);
            drop(guard);
            self.session
                .changes
                .send_modify(|generation| *generation += 1);
        }
        Ok(toggled.text)
    }

    async fn call_operation(
        &self,
        client: &ApiClient,
        op: &'static OperationDef,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        // Фильтр проверяется до запроса: ошибка в нём не должна появиться после записи.
        // У операций с двоичным ответом аргумента `jq` нет: его отклонит `build_request`.
        let jq = if takes_jq(op) {
            take_jq(&mut args)?
        } else {
            None
        };
        self.run_operation(client, op, args, jq).await
    }

    /// Запрос операции `op` с уже проверенным фильтром.
    async fn run_operation(
        &self,
        client: &ApiClient,
        op: &'static OperationDef,
        args: JsonObject,
        jq: Option<JqFilter>,
    ) -> Result<String, ToolError> {
        let request = build_request(op, &args, &self.inner.files)?;
        tracing::debug!(tool = op.tool_name, path = %request.path, "вызов операции RuStore API");
        execute(
            client,
            request,
            Some(op),
            jq.as_ref(),
            &self.inner.options.jq,
        )
        .await
    }

    /// `api_request`: уровень доступа и файлы проверяет [`api_request::prepare`], дальше —
    /// тот же путь, что у сгенерированных инструментов.
    async fn call_api_request(
        &self,
        client: &ApiClient,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let jq = take_jq(&mut args)?;
        let prepared = api_request::prepare(&args, inner.options.max_access, &inner.files)?;
        tracing::debug!(
            tool = API_REQUEST_TOOL,
            method = prepared.request.method.as_str(),
            path = %prepared.request.path,
            operation = prepared.operation.map(|op| op.tool_name),
            "произвольный запрос к RuStore API"
        );
        if jq.is_some() && prepared.request.save_to.is_some() {
            return Err(ToolError::new(
                "`jq` cannot be combined with `save_to`: a downloaded file is not filtered",
            ));
        }
        execute(
            client,
            prepared.request,
            prepared.operation,
            jq.as_ref(),
            &inner.options.jq,
        )
        .await
    }

    /// Составной инструмент: `jq` снимается и проверяется до запросов, аргументы проверяются
    /// по схеме инструмента, результат проходит ту же постобработку, что и ответы операций
    /// ([`finish_value`]).
    async fn call_composite(
        &self,
        index: usize,
        tool: &Tool,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let Some(composite) = self.inner.options.composite_tools.get(index) else {
            return Err(ToolError::new("composite tool is not available"));
        };
        let jq = take_jq(&mut args)?;
        check_arguments(&Value::Object((*tool.input_schema).clone()), &args)?;
        tracing::debug!(tool = composite.name, "составной инструмент");
        let value = (composite.run)(&self.inner.context, args).await?;
        finish_value(
            &self.inner.client,
            composite.name,
            value,
            jq.as_ref(),
            &self.inner.options.jq,
        )
        .await
    }

    /// Пользовательский инструмент: фиксированные аргументы добавляются к аргументам агента,
    /// дальше — тот же путь, что у инструмента операции (сборка запроса, файлы, ответ).
    /// Уровень доступа операции проверен при регистрации.
    async fn call_custom(
        &self,
        client: &ApiClient,
        index: usize,
        args: JsonObject,
    ) -> Result<String, ToolError> {
        let Some(tool) = self.inner.options.custom_tools.get(index) else {
            return Err(ToolError::new("custom tool is not available"));
        };
        let (args, jq) = tool.build_call(args)?;
        tracing::debug!(tool = %tool.name, operation = tool.operation.tool_name, "пользовательский инструмент");
        self.run_operation(client, tool.operation, args, jq).await
    }
}

/// Выполняет запрос и превращает ответ в текст для агента.
///
/// Единая точка постобработки ответа для всех инструментов. Порядок: вырезание учётных
/// данных ([`redact_response`], в ошибках — [`redact_api_error`]) → фильтр `jq` → лимит
/// размера → текст с пометкой о замене. Фильтр получает только очищенный ответ, поэтому не
/// может раскрыть вырезанные значения, в том числе преобразованием (`@base64`), которое не
/// поймала бы итоговая `scrub_token`.
///
/// С фильтром ответ читается до [`FILTER_INPUT_LIMIT`]: фильтр как раз уменьшает большой
/// ответ, а лимит ответа сервера (`max_response_bytes`) применяется к результату фильтра.
///
/// `op` — операция запроса, если она известна: по ней выбирается аргумент следующей
/// страницы ([`adjust_pagination`]).
async fn execute(
    client: &ApiClient,
    request: ApiRequest,
    op: Option<&OperationDef>,
    jq: Option<&JqFilter>,
    runner: &JqRunner,
) -> Result<String, ToolError> {
    let path = request.path.clone();
    let max = client.config().max_response_bytes;
    let result = match jq {
        Some(_) => {
            client
                .execute_with_limit(request, FILTER_INPUT_LIMIT.max(max))
                .await
        }
        None => client.execute(request).await,
    };
    let mut response = result.map_err(|err| {
        let (err, redactions) = redact_api_error(&path, err);
        let mut message = ToolError::from(err).message().to_owned();
        if let Some(note) = redactions.note() {
            message.push_str("\n\n");
            message.push_str(&note);
        }
        ToolError::new(message)
    })?;
    adjust_pagination(op, &mut response.pagination);
    let redactions = redact_response(&path, &mut response);
    let mut text = match jq {
        None => response_text(&response),
        Some(filter) => filter_response(client, runner, filter, response, max).await?,
    };
    if let Some(note) = redactions.note() {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    Ok(text)
}

/// Применяет фильтр к телу очищенного ответа; заголовки постраничной выдачи остаются рядом
/// с результатом (`{"result", "pagination"}`, как без фильтра). Ответ не в JSON
/// возвращается без фильтра, с пояснением.
async fn filter_response(
    client: &ApiClient,
    runner: &JqRunner,
    filter: &JqFilter,
    response: ApiResponse,
    max: usize,
) -> Result<String, ToolError> {
    let status = response.status;
    // Ответ уже получен: изменение (если это запись) выполнено, и агент должен это знать,
    // чтобы не повторять запрос из-за ошибки фильтра.
    let after_request = |problem: String| {
        ToolError::new(format!(
            "the API request succeeded (HTTP {status}), but {problem}. Repeating a request that \
             changes data would apply the change again"
        ))
    };
    let ApiResponse {
        body: ResponseBody::Json(mut value),
        pagination,
        ..
    } = response
    else {
        let mut text = response_text(&response);
        if text.len() > max {
            return Err(after_request(format!(
                "the response is not JSON and is larger than {max} bytes, so it was not returned"
            )));
        }
        text.push_str("\n\n[The `jq` filter was not applied: the response is not JSON.]");
        return Ok(text);
    };
    scrub_token_json(client, &mut value);
    let output = runner
        .run(filter, value, max)
        .await
        .map_err(|err| after_request(format!("the `jq` filter failed: {err}. Fix the filter")))?;
    if output.truncated {
        return Err(after_request(format!(
            "the filtered result is larger than {max} bytes; select fewer fields or items with \
             the filter, or narrow the request"
        )));
    }
    if pagination.is_empty() {
        return Ok(output.text);
    }
    // Результат фильтра — компактный JSON (не обрезан), поэтому склейка даёт валидный JSON.
    let pagination = serde_json::to_string(&pagination).unwrap_or_else(|_| "{}".to_owned());
    Ok(format!(
        "{{\"result\":{},\"pagination\":{pagination}}}",
        output.text
    ))
}

/// Результат составного инструмента в текст для агента: та же постобработка, что у ответа
/// операции в [`execute`] — вырезание учётных данных и токена, фильтр `jq`, лимит размера
/// `max_response_bytes`, пометка о замене.
async fn finish_value(
    client: &ApiClient,
    name: &str,
    mut value: Value,
    jq: Option<&JqFilter>,
    runner: &JqRunner,
) -> Result<String, ToolError> {
    let max = client.config().max_response_bytes;
    let redactions = redact_json("", &mut value);
    scrub_token_json(client, &mut value);
    // Инструмент уже выполнил свои запросы: если он что-то менял, повтор изменит снова.
    let after_run = |problem: String| {
        ToolError::new(format!(
            "the tool `{name}` finished its API calls, but {problem}. Repeating a tool that \
             changes data would apply the change again"
        ))
    };
    let mut text = match jq {
        None => {
            let text = value.to_string();
            if text.len() > max {
                return Err(after_run(format!(
                    "its result is larger than {max} bytes; narrow the arguments or select less \
                     with `jq`"
                )));
            }
            text
        }
        Some(filter) => {
            let output = runner.run(filter, value, max).await.map_err(|err| {
                after_run(format!("the `jq` filter failed: {err}. Fix the filter"))
            })?;
            if output.truncated {
                return Err(after_run(format!(
                    "the filtered result is larger than {max} bytes; select fewer fields or \
                     items with the filter"
                )));
            }
            output.text
        }
    };
    if let Some(note) = redactions.note() {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    Ok(text)
}

/// Заменяет токен клиента во всех строках и ключах JSON до фильтра `jq`: итоговая
/// `scrub_token` ищет токен только в исходном виде, а фильтр мог бы его преобразовать.
fn scrub_token_json(client: &ApiClient, value: &mut Value) {
    match value {
        Value::String(text) => {
            let scrubbed = client.scrub_token(std::mem::take(text));
            *text = scrubbed;
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| scrub_token_json(client, item)),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut item) in entries {
                scrub_token_json(client, &mut item);
                map.insert(client.scrub_token(key), item);
            }
        }
        _ => {}
    }
}

impl ServerHandler for RuStoreServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = if self.inner.options.dynamic_toolsets {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build()
        } else {
            ServerCapabilities::builder().enable_tools().build()
        };
        ServerConfig::new(capabilities)
            .with_server_info(
                Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")).with_title("RuStore"),
            )
            .with_instructions(self.inner.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.registry().tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let before = self.session.generation();
        let result = self.call(&request.name, args).await;
        // В сессии с `initialize` уведомление уходит сразу; в протоколе 2026-07-28 клиент
        // получает его через подписку (`listen`), а незапрошенные уведомления отклоняет.
        if self.session.generation() != before
            && uses_legacy_session(&context)
            && let Err(err) = context.peer.notify_tool_list_changed().await
        {
            tracing::warn!(error = %err, "не удалось отправить tools/list_changed");
        }
        result.map(Into::into)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        // Подписка нужна только для смены списка инструментов в динамическом режиме.
        self.inner
            .options
            .dynamic_toolsets
            .then(|| requested.supported_by(&self.get_info().capabilities))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        if context.accepted().tools_list_changed != Some(true) {
            context.cancelled().await;
            return Ok(());
        }
        let mut changes = self.session.changes.subscribe();
        changes.mark_unchanged();
        loop {
            tokio::select! {
                () = context.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    changes.mark_unchanged();
                    if let Err(err) = context.sink().notify_tool_list_changed().await {
                        tracing::debug!(error = %err, "подписка на tools/list_changed закрыта");
                        return Ok(());
                    }
                }
            }
        }
    }

    fn get_tool(&self, _name: &str) -> Option<Tool> {
        // Аргументы проверяет `build_request` с понятными агенту сообщениями;
        // проверку схемы на стороне rmcp не включаем.
        None
    }
}

/// Сессия с `initialize` (протокол до 2026-07-28): уведомления отправляются без подписки.
fn uses_legacy_session(context: &RequestContext<RoleServer>) -> bool {
    // Версии `YYYY-MM-DD` сравниваются лексикографически так же, как по времени.
    context
        .protocol_version()
        .is_none_or(|version| version.as_str() < ProtocolVersion::V_2026_07_28.as_str())
}

/// Инструкции сервера для агента (поле `instructions` в `initialize`).
///
/// Текст собирается из строк-разделов: доступ, приложения и пакеты по умолчанию, жизненный
/// цикл версии, песочница, поиск операций, аргументы, ответы и постраничная выдача, `jq`,
/// большие ответы, справочник, локальные файлы, секреты и персональные данные. Строки,
/// которые зависят от настроек (наборы, файлы, пользовательские и составные инструменты),
/// появляются только тогда, когда к ним есть что сказать.
pub fn server_instructions(
    options: &ServerOptions,
    registry: &ToolRegistry,
    files: &FilePolicy,
) -> String {
    let access = access_text(options.max_access, options.local_machine);
    let token = if options.local_machine {
        format!(
            "The RuStore API key (key ID and private key) of the server's profile is kept in the \
             system keychain; the server signs token requests with it and keeps the short-lived \
             access token (`Public-Token`) in memory only. The key and the token are never \
             passed in tool arguments or shown in responses. If the API rejects the key or the \
             token (HTTP 401, or a token error), ask the user to run `rustore-mcp auth login \
             --profile {profile}`; if it denies access (HTTP 403), the key lacks a permission or \
             an app: ask the user to tick the needed methods and apps for the key in the RuStore \
             console (Company > RuStore API) or to create a key with them.",
            profile = options.profile.as_deref().unwrap_or("<profile>"),
        )
    } else {
        "This server runs in HTTP mode: the RuStore API key stays on the server, which gets \
         the short-lived access tokens itself. The MCP client only sends its access token for \
         this server in the `Authorization` header from its configuration; it is not a RuStore \
         key or token. Never read it from the client configuration, print it, or pass it to \
         tools, commands or files. If the RuStore API rejects the server's key (HTTP 401, or a \
         token error) or denies access (HTTP 403), ask the user to contact the operator of the \
         server: the key is configured there."
            .to_owned()
    };
    let account = match options.profile.as_deref() {
        Some(name) if options.local_machine => format!(
            "This server works with one RuStore API key of one company: the profile `{name}`. \
             For another company or key the user starts another server with another profile."
        ),
        _ => "This server works with one RuStore API key of one company.".to_owned(),
    };
    let packages = if options.packages.is_empty() {
        String::new()
    } else {
        format!(
            " The user's default package name(s) for this profile: {}. Use them when the user \
             does not name another app.",
            options
                .packages
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let files_line = if options.local_machine {
        format!(
            "- Local files: file arguments (`file` of `upload_apk`, `upload_aab`, `upload_icon` \
             and `upload_screenshot*`, `save_to` of downloads such as `export_reviews_csv`) take \
             a path (absolute, or relative to the server's working directory) inside the allowed \
             directories: {roots}. Other paths are rejected; the user can allow more with \
             --allow-path. Downloads never overwrite existing files. Large content (review CSV \
             exports) is never returned in responses: it is saved to `save_to`.\n",
            roots = files.describe_roots(),
        )
    } else {
        "- Remote server: this server runs on another machine (HTTP mode) and cannot read or \
         write files on your machine, so there are no upload or download tools (APK and AAB \
         uploads, icons, screenshots, review CSV export). Ask the user to upload or download \
         such files in the RuStore console.\n"
            .to_owned()
    };
    let enabled = registry.enabled_toolsets();
    let toolsets = if registry.is_dynamic() {
        format!(
            "Toolsets are loaded on demand; enabled at start: {}. Tools of other toolsets are \
             not registered until you enable them, and tools above the access level are never \
             registered; calling them explains what to do.",
            if enabled.is_empty() {
                "none".to_owned()
            } else {
                enabled.join(", ")
            }
        )
    } else {
        format!(
            "Enabled toolsets: {}; tools of other toolsets (and above the access level) are not \
             registered, and calling them explains which flag enables them.",
            enabled.join(", ")
        )
    };
    let dynamic = if registry.is_dynamic() {
        "- Toolsets: tools are loaded on demand, by toolset. Call `list_toolsets` to see the \
         toolsets, then `enable_toolset` to load the tools you need (`disable_toolset` unloads \
         them); `api_search` shows the toolset of an operation. The client is notified that the \
         tool list changed; if the enabled tools do not appear in your tool list, call the same \
         operations with `api_request`.\n"
    } else {
        ""
    };
    let composite = if registry
        .iter()
        .any(|tool| matches!(tool.kind, ToolKind::Composite(_)))
    {
        "- Composite tools: some tools run several API calls for a common task, such as \
         searching the apps of the key or summarizing the versions of an app; their \
         description says what they do and their access level. Prefer them to chains of \
         operation tools. They take `packageName` like the operation tools and use the \
         profile's default package when exactly one is set; `api_search` finds them too.\n"
    } else {
        ""
    };
    // Выпуск версии составными инструментами набора `releases`. Говорим о нём всегда, а не
    // только когда они зарегистрированы: в динамическом режиме набор включают позже, а вызов
    // инструмента выше уровня доступа объясняет, какой флаг нужен. `upload_build` работает с
    // локальными файлами и в HTTP-режиме не регистрируется.
    let release_flow = if options.local_machine {
        " The composite tools of the `releases` toolset cover this flow in fewer calls: \
         `upload_build` (write) fills the draft, creating it only when the app has none, and \
         uploads the AAB or APK files; then `release_submit` (destructive) sends it to \
         moderation; then `release_rollout` (destructive) publishes it or raises the rollout \
         percentage (see below)."
    } else {
        " The composite tools of the `releases` toolset cover the publication in fewer calls \
         (build uploads are not available in HTTP mode: ask the user to upload the build in \
         the RuStore console): `release_submit` (destructive) sends the draft \
         to moderation; then `release_rollout` (destructive) publishes it or raises the \
         rollout percentage (see below)."
    };
    let custom = if registry
        .iter()
        .any(|tool| matches!(tool.kind, ToolKind::Custom(_)))
    {
        "- Custom tools: some tools are defined by the user (their description says so): each \
         calls one API operation with arguments fixed by the user and an optional built-in \
         filter. Prefer them when they fit the task; fixed arguments cannot be passed.\n"
    } else {
        ""
    };
    format!(
        "RuStore MCP server ({level} access). Every operation tool calls one RuStore Public API \
         operation (apps, versions and publication, icon and screenshots, reviews, payments and \
         purchases, subscriptions, product catalog, app access) and returns its JSON response. \
         {account} {toolsets}\n\
         \n\
         - Access: {access}\n\
         - Apps: `list_apps` lists the apps the API key can access (`appName`, `packageName`, \
         `appId`, `appStatus`); the key may be limited to some apps in the RuStore console. \
         Apps, versions, images, reviews and access tools take the package name \
         (`packageName`, like `com.example.app`); payments, purchases, subscription \
         cancellation and the product catalog take the numeric app ID (`appId`) from \
         `list_apps`.{packages}\n\
         - Versions: a new release is a draft version. `create_draft_version` creates it \
         (fields left out are copied from the active version; an app has only one draft at a \
         time, so if creating fails because a draft exists, find it with `list_versions` and \
         `versionStatuses: \"DRAFT\"` and continue it, or delete it with \
         `delete_draft_version`); `update_draft_version` changes its texts and settings; \
         `upload_apk` or `upload_aab` adds the build (its version code must be higher than the \
         active one); `upload_icon` and `upload_screenshot*` add images; `get_version` shows \
         what is filled in. `submit_draft_for_moderation` sends the draft to moderation; with \
         automatic publication an approved version goes to users without another call, with \
         manual publication `publish_version` publishes a version in `READY_FOR_PUBLICATION`. \
         `update_publish_settings` sets the publication type, time and rollout percentage. \
         Statuses: `DRAFT`; `AUTO_CHECK`, `MODERATION`, `TAKEN_FOR_MODERATION` (in review); \
         `AUTO_CHECK_FAILED`, `REJECTED_BY_MODERATOR`, `REJECTED_BY_SECURITY` (rejected, the \
         reason is in `statusComment` of `get_version`); `READY_FOR_PUBLICATION`; \
         `PARTIAL_ACTIVE` (rolled out to `partialValue` percent of users); `ACTIVE`; \
         `ARCHIVED`. `list_versions` returns release versions by default; pass \
         `filterTestingType: \"ALL\"` to include alpha versions. `release_summary` shows what \
         is live, in review, rejected or drafted.{release_flow}\n\
         - Releasing: `release_submit` can set the publication first: `publishType` \
         `INSTANTLY` (the RuStore default) releases the version to real users as soon as \
         moderation passes, `MANUAL` waits for `release_rollout` with `action: \"publish\"`, \
         `DELAYED` publishes at `publishDateTime`; `partialValue` (5, 10, 25, 50, 75, 100) \
         starts a staged rollout. `release_rollout` with `action: \"set_percent\"` raises the \
         percentage; RuStore cannot lower it. Both tools take `dry_run: true`: run it first \
         and ask the user before a real run, because a submitted draft can no longer be \
         edited and a publication reaches users at once.\n\
         - Sandbox: tools ending in `_sandbox` call the same payment and subscription methods \
         in the RuStore sandbox (test purchases made with test accounts); use them to check a \
         payment integration. The other payment tools work with real purchases and money.\n\
         - Finding operations: `api_search` searches all RuStore API operations by words in \
         English or Russian (names, summaries, paths), including tools not enabled here, and \
         tells which flag enables them. Use it when no listed tool fits. `api_request` calls \
         any RuStore API path (`/public/...`, like \
         `/public/v1/application/com.example.app/version`) within the server's access level; \
         toolsets do not limit it. Prefer the dedicated tool when it is registered: it \
         validates arguments and documents the schema.\n\
         {composite}\
         {dynamic}\
         {custom}\
         - Arguments: path and query parameters are plain arguments named as in the API \
         (`packageName`, `versionId`, `appId`, `page`, `size`); a JSON request body goes in \
         the `body` argument as a JSON object; upload tools take a local file in `file`. \
         Unknown arguments are rejected. Numeric IDs may be passed as numbers or strings.\n\
         - Responses: RuStore wraps every response as `{{\"code\", \"message\", \"body\", \
         \"timestamp\"}}`; tools return its `body` (or the whole wrapper when `body` is empty), \
         and a `code` other than `OK` is returned as an error with RuStore's `message`.\n\
         - Pagination: page lists (versions, reviews, payments by date) take a page number \
         (from 0) and a page size (`page` and `size`, or `pageNumber` and `pageSize`). A \
         response with `content` and page totals is wrapped as `{{\"result\": <response>, \
         \"pagination\": {{...}}}}` with `page`, `page_size`, `total_pages`, `total_elements` \
         and, when there is a next page, `next_page` and `pass_as`: call the same tool again \
         with the same arguments and `next_page` in the argument named by `pass_as`; repeat \
         until `next_page` is missing. Other lists return a continuation token in the body \
         (`continuationToken` of `list_apps` and the product catalog, `pageToken` of \
         `list_app_access`): pass it back in the argument of the same name (`list_apps` also \
         needs `pagination: true` and `pageSize`) until it is missing.\n\
         - Filtering: operation tools with a JSON response, composite tools, custom tools and \
         `api_request` accept an optional `jq` argument, a jq filter (jaq dialect) applied to the JSON \
         response body, for example `[.content[] | {{versionId, versionName, versionStatus}}]` \
         for `list_versions` or `[.content[] | {{packageName, appName}}]` for `list_apps`. Use \
         it to keep large responses small. Paging data stays outside the filter: a paginated \
         result is still wrapped as `{{\"result\": <filter output>, \"pagination\": {{...}}}}`. \
         Credentials are redacted before the filter runs; the size limit applies to the \
         filtered result. Several outputs are returned as a JSON array, no output as `[]`. \
         Filters cannot read the environment, files or other inputs. A broken filter is \
         reported before the request is sent; a filter that fails at runtime (after the \
         request succeeded) returns an error that says so: do not repeat a request that \
         changed data just to fix the filter. `jq` cannot be combined with `save_to`.\n\
         - Large responses are rejected with an error; narrow the request (a smaller `size` \
         or `pageSize`, a status or date filter) or select less with `jq`.\n\
         - Reference: every operation tool description ends with a link to its page in the \
         RuStore API reference; read it when the arguments or the request body are unclear.\n\
         {files_line}\
         - Secrets: {token} Never ask the user to paste keys, tokens, passwords or other \
         secrets into the conversation. Values that look like credentials are replaced with \
         \"[redacted]\" in every response, including errors, and a note says when that \
         happened: fields such as passwords, tokens, private keys and signatures, and \
         key- or token-shaped values inside any text (RSA private keys in PEM or Base64, JWE \
         and JWT tokens, `Public-Token` and `Authorization` values, passwords in URLs). Purchase \
         and subscription tokens and invoice IDs of payments stay visible: they identify \
         purchases, not access. Do not try to recover hidden values.\n\
         - Personal data: reviews, answers, payments, purchases and access lists contain data \
         of real RuStore users and developers (names, user IDs, review texts, purchase \
         details). Use it only for the current task: do not copy it into logs, commits, code, \
         test fixtures, wiki pages or other systems unless the user asks for exactly that.",
        level = access_name(options.max_access),
    )
}

/// Уровень доступа для инструкций. Удалённому серверу (HTTP) уровень задаёт оператор при
/// запуске, а не пользователь агента.
fn access_text(level: AccessLevel, local: bool) -> &'static str {
    match (level, local) {
        (AccessLevel::Read, true) => {
            "This server is read-only: only tools that read data are available. Tools that \
             change a draft version (creating and editing drafts, uploading builds, icons and \
             screenshots) are enabled when the user starts the server with --allow-write, and \
             tools that reach users or affect money (sending a draft to moderation, publishing, \
             rollout percentage, archiving, public replies to reviews, refunds, cancellations \
             and confirmations of purchases and subscriptions, granting and revoking access) \
             with --allow-destructive. If a task needs them, ask the user instead of looking \
             for workarounds."
        }
        (AccessLevel::Read, false) => {
            "This server is read-only: only tools that read data are available. Tools that \
             change drafts, publish, reply to reviews or affect money need the operator of the \
             server to start it with a higher access level. If a task needs them, ask the user \
             instead of looking for workarounds."
        }
        (AccessLevel::Write, true) => {
            "This server can read data and change draft versions (texts, builds, icons, \
             screenshots), but not reach users or affect money: sending a draft to moderation, \
             publishing, rollout percentage, archiving, public replies to reviews, refunds, \
             purchase and subscription actions and access changes need the user to start the \
             server with --allow-destructive. Changes of a draft stay invisible to users until \
             it passes moderation and is published."
        }
        (AccessLevel::Write, false) => {
            "This server can read data and change draft versions (texts, builds, icons, \
             screenshots), but not reach users or affect money: those tools need the operator \
             of the server to start it with the destructive level. Changes of a draft stay \
             invisible to users until it passes moderation and is published."
        }
        (AccessLevel::Destructive, _) => {
            "This server can read, change, publish and delete data. Sending a draft to \
             moderation can publish it to real users, publishing and rollout changes reach \
             users at once, replies to reviews are public, refunds and cancellations affect \
             money, and deleting is irreversible: confirm such calls with the user unless they \
             explicitly asked for them."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instructions(options: &ServerOptions, files: &FilePolicy) -> String {
        let registry = ToolRegistry::new(options).unwrap();
        server_instructions(options, &registry, files)
    }

    #[test]
    fn instructions_depend_on_access() {
        let files = FilePolicy::new(&[]).unwrap();
        let read = instructions(&ServerOptions::default(), &files);
        assert!(read.contains("(read-only access)"), "{read}");
        assert!(read.contains("--allow-write") && read.contains("--allow-destructive"));
        assert!(read.contains("allowed directories: none"), "{read}");
        assert!(
            read.contains("Enabled toolsets: apps, releases, reviews;"),
            "{read}"
        );
        assert!(read.contains("auth login --profile <profile>"), "{read}");
        assert!(read.contains("`pagination`") || read.contains("\"pagination\""));
        assert!(read.contains("`next_page`"), "{read}");
        assert!(read.contains("never passed in tool arguments"), "{read}");
        assert!(
            read.contains("`api_search`") && read.contains("`api_request`"),
            "{read}"
        );
        assert!(
            read.contains("Never ask the user to paste keys, tokens"),
            "{read}"
        );
        assert!(read.contains("[redacted]"), "{read}");
        assert!(
            read.contains("Personal data") && read.contains("commits"),
            "{read}"
        );

        assert!(read.contains("`Public-Token`"), "{read}");
        assert!(read.contains("Company > RuStore API"), "{read}");
        assert!(!read.contains("default package name"), "{read}");
        // Разделы про RuStore: приложения, жизненный цикл версии, песочница, выдача токеном.
        for part in [
            "RuStore MCP server",
            "`list_apps`",
            "`appId`",
            "`create_draft_version`",
            "only one draft",
            "`submit_draft_for_moderation`",
            "`publish_version`",
            "`statusComment`",
            "`PARTIAL_ACTIVE`",
            "`_sandbox`",
            "`continuationToken`",
            "`pageNumber`",
            "`export_reviews_csv`",
            "`release_summary`",
            "`upload_build` (write)",
            "`release_submit` (destructive)",
            "`release_rollout` (destructive)",
            "`dry_run: true`",
            "cannot lower it",
        ] {
            assert!(read.contains(part), "{part}: {read}");
        }
        for absent in ["Tracker", "issue", "queue", "perPage"] {
            assert!(!read.contains(absent), "{absent}: {read}");
        }

        let write = instructions(
            &ServerOptions {
                max_access: AccessLevel::Write,
                toolsets: Some(vec!["catalog".into()]),
                profile: Some("codex".into()),
                packages: vec!["com.example.one".into(), "com.example.two".into()],
                ..ServerOptions::default()
            },
            &files,
        );
        assert!(write.contains("(write access)"), "{write}");
        assert!(
            write.contains(
                "default package name(s) for this profile: `com.example.one`, `com.example.two`"
            ),
            "{write}"
        );
        assert!(write.contains("Enabled toolsets: catalog;"), "{write}");
        assert!(write.contains("auth login --profile codex"), "{write}");
        assert!(write.contains("the profile `codex`"), "{write}");
        assert!(write.contains("--allow-destructive"), "{write}");

        let all = instructions(
            &ServerOptions {
                max_access: AccessLevel::Destructive,
                ..ServerOptions::default()
            },
            &files,
        );
        assert!(all.contains("(destructive access)"), "{all}");
        assert!(all.contains("irreversible"), "{all}");
    }

    #[test]
    fn remote_instructions_have_no_local_features() {
        let remote = ServerOptions {
            local_machine: false,
            profile: Some("codex".into()),
            ..ServerOptions::default()
        };
        let text = instructions(&remote, &FilePolicy::disabled());
        for absent in ["auth login", "--allow-path", "allowed directories"] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
        assert!(text.contains("HTTP mode"), "{text}");
        assert!(text.contains("`Authorization`"), "{text}");
        assert!(text.contains("operator of the"), "{text}");
        // Загрузки сборок в HTTP-режиме нет: выпуск — только отправка и публикация.
        assert!(!text.contains("`upload_build`"), "{text}");
        assert!(
            text.contains("`release_submit`") && text.contains("`release_rollout`"),
            "{text}"
        );
    }
}
