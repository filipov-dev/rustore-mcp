//! Сквозная проверка подсказок MCP и сообщений о ходе вызова: сервер в процессе, клиент
//! `rmcp` через in-memory транспорт, RuStore API подменён `wiremock`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, GetPromptRequestParams, JsonObject, ProgressNotificationParam,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use rustore_api::executor::{ApiClient, ClientConfig, RetryPolicy};
use rustore_api::{AccessLevel, StaticToken};
use rustore_mcp_core::composite::{CompositeTool, common};
use rustore_mcp_core::{FilePolicy, RuStoreServer, ServerOptions};
use serde_json::{Value, json};
use wiremock::MockServer;

fn client(api: &MockServer) -> ApiClient {
    let mut config = ClientConfig::new(Some(Arc::new(StaticToken::new("test-token")))).unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    ApiClient::new(config).unwrap()
}

/// Клиент, который записывает уведомления о ходе.
#[derive(Clone, Default)]
struct ProgressClient {
    seen: Arc<Mutex<Vec<ProgressNotificationParam>>>,
}

impl ClientHandler for ProgressClient {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.seen.lock().unwrap().push(params);
    }
}

async fn start(
    api: &MockServer,
    options: ServerOptions,
) -> (
    RunningService<RoleClient, ProgressClient>,
    ProgressClient,
    tokio::task::JoinHandle<()>,
) {
    let server = RuStoreServer::new(client(api), options, FilePolicy::disabled()).unwrap();
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let handle = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let handler = ProgressClient::default();
    let client = handler.clone().serve(client_io).await.unwrap();
    (client, handler, handle)
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => panic!("не объект"),
    }
}

/// Составной инструмент, который дважды сообщает о ходе.
fn progress_tool() -> CompositeTool {
    CompositeTool {
        name: "fake_progress",
        toolset: "releases",
        access: AccessLevel::Read,
        title: "Fake progress",
        description: "Reports progress twice.",
        input_schema: || common::object_schema(json!({}), &[]),
        uses_local_files: false,
        run: |ctx, _args| {
            Box::pin(async move {
                ctx.report_progress(1.0, Some(2.0), "half way".into()).await;
                ctx.report_progress(2.0, Some(2.0), "done".into()).await;
                Ok(json!({"ok": true}))
            })
        },
    }
}

#[tokio::test]
async fn composite_tools_report_progress() {
    let api = MockServer::start().await;
    let options = ServerOptions {
        composite_tools: vec![progress_tool()],
        toolsets: Some(vec!["releases".into()]),
        ..ServerOptions::default()
    };
    let (client, handler, server) = start(&api, options).await;
    // Клиент `rmcp` передаёт `progressToken` в каждом запросе.
    let result = client
        .call_tool(CallToolRequestParams::new("fake_progress").with_arguments(object(json!({}))))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    // Уведомления обрабатываются клиентом отдельно от ответа: ждём их недолго.
    for _ in 0..100 {
        if handler.seen.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = handler.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].progress, 1.0);
    assert_eq!(seen[0].total, Some(2.0));
    assert_eq!(seen[0].message.as_deref(), Some("half way"));
    assert_eq!(seen[1].message.as_deref(), Some("done"));
    assert_eq!(seen[0].progress_token, seen[1].progress_token);

    client.cancel().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn prompts_depend_on_access_level() {
    let api = MockServer::start().await;
    let (client, _, server) = start(&api, ServerOptions::default()).await;
    let info = client.peer_info().unwrap();
    assert!(info.capabilities.prompts.is_some());
    let names: Vec<String> = client
        .list_all_prompts()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name.to_string())
        .collect();
    assert_eq!(
        names,
        ["triage_reviews", "reconcile_payments", "apps_health_check"]
    );

    // Подстановка аргументов.
    let prompt = client
        .get_prompt(
            GetPromptRequestParams::new("reconcile_payments").with_arguments(object(json!({
                "package_name": "com.example.app",
                "from": "2026-10-01",
                "to": "2026-10-31"
            }))),
        )
        .await
        .unwrap();
    let text = serde_json::to_string(&prompt.messages).unwrap();
    assert!(text.contains("from 2026-10-01 to 2026-10-31"), "{text}");
    assert!(text.contains("packageName com.example.app"), "{text}");
    assert!(text.contains("`payments_report`"), "{text}");

    // Подсказка уровня удаления на сервере чтения не регистрируется.
    let err = client
        .get_prompt(GetPromptRequestParams::new("release_version"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown prompt"), "{err}");
    client.cancel().await.unwrap();
    server.abort();

    let (client, _, server) = start(
        &api,
        ServerOptions {
            max_access: AccessLevel::Destructive,
            ..ServerOptions::default()
        },
    )
    .await;
    let names: Vec<String> = client
        .list_all_prompts()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name.to_string())
        .collect();
    assert_eq!(names[0], "release_version");
    assert_eq!(names.len(), 4);
    let prompt = client
        .get_prompt(
            GetPromptRequestParams::new("release_version").with_arguments(object(json!({
                "package_name": "com.example.app",
                "publish_type": "MANUAL",
                "rollout_percent": 25
            }))),
        )
        .await
        .unwrap();
    let text = serde_json::to_string(&prompt.messages).unwrap();
    assert!(
        text.contains("publishType MANUAL and partialValue 25"),
        "{text}"
    );
    assert!(text.contains("`wait_for_moderation`"), "{text}");
    client.cancel().await.unwrap();
    server.abort();
}
