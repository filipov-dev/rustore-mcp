//! `serve --http` настоящим бинарником с логами уровня `trace`: ключ RuStore API из файла
//! секрета (keyId и приватный ключ) и токены доступа клиентов из заголовков не попадают ни
//! в stderr, ни в stdout, ни в ответы.
//!
//! Отдельный процесс, а не подписчик `tracing` внутри тестов: при параллельных тестах кеш
//! interest у `tracing` отключает часть событий, и проверка была бы неполной. К RuStore API
//! тест не обращается: вызывается только инструмент с ошибкой в аргументах, а токен RuStore
//! выпускается лениво, при первом запросе к API.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use rustore_api::testing;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// Токен доступа клиента: собирается из частей.
fn access_token() -> String {
    format!("log-secret-access-{}", "9f8e7d6c5b4a".repeat(3))
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Останавливает сервер, даже если тест упал.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[tokio::test]
async fn key_and_tokens_never_appear_in_logs_or_responses() {
    let dir = tempfile::tempdir().unwrap();
    let private_key = testing::private_key_base64();
    // Фрагмент из середины ключа: если он есть в выводе, туда попал ключ.
    let key_mark = private_key[100..164].to_owned();
    let key_id = "log-secret-key-id-4c0ffee";
    let key = json!({"keyId": key_id, "privateKey": private_key});
    let key_path = dir.path().join("rustore-key.json");
    std::fs::write(&key_path, key.to_string()).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let tokens_path = dir.path().join("tokens.toml");
    std::fs::write(
        &tokens_path,
        format!(
            "[[tokens]]\nname = \"reader\"\nsha256 = \"{}\"\n",
            sha256_hex(&access_token()),
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rustore-mcp"))
        .args([
            "serve",
            "--http",
            "--listen",
            "127.0.0.1:0",
            "--log-level",
            "trace",
        ])
        .arg("--access-tokens")
        .arg(&tokens_path)
        .arg("--key-file")
        .arg(&key_path)
        // Профили — во временном каталоге, а не в домашнем.
        .env("RUSTORE_MCP_CONFIG_DIR", dir.path().join("config"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let child = Child(child);

    // stderr читается целиком в фоне; адрес эндпоинта — из строки запуска.
    let logs = Arc::new(Mutex::new(String::new()));
    let (endpoint_tx, endpoint_rx) = mpsc::channel();
    let reader_logs = Arc::clone(&logs);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if let Some(url) = line.strip_prefix("rustore-mcp: MCP endpoint ") {
                let _ = endpoint_tx.send(url.to_owned());
            }
            let mut logs = reader_logs.lock().unwrap();
            logs.push_str(&line);
            logs.push('\n');
        }
    });
    let endpoint = match endpoint_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(endpoint) => endpoint,
        Err(_) => panic!(
            "server did not print its endpoint: {}",
            logs.lock().unwrap()
        ),
    };

    let http = reqwest::Client::new();
    let post = |body: Value| {
        http.post(&endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(body.to_string())
    };
    let init = rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }),
    );
    let secrets = [key_mark.clone(), key_id.to_owned(), access_token()];
    let clean = |text: &str| {
        for secret in &secrets {
            assert!(!text.contains(secret.as_str()), "secret in: {text}");
        }
    };
    let accepted = [
        init.clone(),
        rpc(2, "tools/list", json!({})),
        // Без обязательного `path`: ошибка до запроса к API.
        rpc(
            3,
            "tools/call",
            json!({"name": "api_request", "arguments": {"method": "GET"}}),
        ),
    ];
    for body in accepted {
        let response = post(body)
            .header("authorization", format!("Bearer {}", access_token()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        clean(&response.text().await.unwrap());
    }
    // Отклонённые запросы тоже не пишут значения заголовков.
    let rejected = [
        (post(init.clone()), 401),
        (
            post(init.clone()).header("authorization", format!("Token {}", access_token())),
            401,
        ),
        (
            post(init.clone()).header("authorization", format!("Bearer {}x", access_token())),
            401,
        ),
        (
            post(init.clone())
                .header("authorization", format!("Bearer {}", access_token()))
                .header("host", "evil.example"),
            403,
        ),
    ];
    for (request, status) in rejected {
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), status);
        clean(&response.text().await.unwrap());
    }

    drop(child);
    reader.join().unwrap();
    let mut out = String::new();
    stdout.read_to_string(&mut out).unwrap();
    assert!(out.is_empty(), "stdout: {out}");
    let logs = logs.lock().unwrap().clone();
    // Логи действительно подробные: видны вызов инструмента и имя токена.
    assert!(logs.contains("api_request"), "{logs}");
    assert!(logs.contains("reader"), "{logs}");
    assert!(logs.contains("TRACE") || logs.contains("DEBUG"), "{logs}");
    clean(&logs);
    assert!(!logs.contains(&private_key[..64]), "{logs}");
}
