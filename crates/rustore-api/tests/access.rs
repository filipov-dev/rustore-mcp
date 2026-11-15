//! Тесты токенов RuStore API на `wiremock`: ключ RSA генерируется в тесте, сети нет.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::sha2::Sha512;
use rsa::signature::Verifier;
use rustore_api::access::{ManualClock, TokenError};
use rustore_api::executor::{ApiClient, ApiRequest, ClientConfig, ResponseBody};
use rustore_api::testing::{TEST_KEY_ID, api_key, private_key_base64, public_key};
use rustore_api::{HttpMethod, KeyTokens, TokenSource};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Узнаваемые фиктивные токены, чтобы искать утечки.
const JWE_1: &str = "eyJhbGciOiJSU0EtT0FFUCJ9.LEAK1key.LEAK1iv.LEAK1text.LEAK1tag";
const JWE_2: &str = "eyJhbGciOiJSU0EtT0FFUCJ9.LEAK2key.LEAK2iv.LEAK2text.LEAK2tag";

/// 2024-06-18T08:49:08.290+00:00.
fn start() -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_millis(1_718_700_548_290)
}

fn token_response(jwe: &str, ttl: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "code": "OK",
        "message": null,
        "body": {"jwe": jwe, "ttl": ttl},
        "timestamp": "2024-06-18T11:49:08.290+03:00"
    }))
}

fn rejection(message: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "code": "ERROR",
        "message": message,
        "body": null,
        "timestamp": "2024-06-18T11:49:08.290+03:00"
    }))
}

fn key_tokens(server: &MockServer, clock: Arc<ManualClock>) -> KeyTokens {
    let base = url::Url::parse(&server.uri()).unwrap();
    KeyTokens::with_base_url(api_key(), &base)
        .unwrap()
        .with_clock(clock)
}

fn request_json(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

#[tokio::test]
async fn token_request_is_signed_with_sha512_rsa() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/public/auth/"))
        .and(header("content-type", "application/json"))
        .respond_with(token_response(JWE_1, 900))
        .expect(1)
        .mount(&server)
        .await;

    let tokens = key_tokens(&server, Arc::new(ManualClock::new(start())));
    let token = tokens.token().await.unwrap();
    assert_eq!(token.expose(), JWE_1);

    let received = server.received_requests().await.unwrap();
    let body = request_json(&received[0]);
    assert_eq!(body["keyId"], TEST_KEY_ID);
    assert_eq!(body["timestamp"], "2024-06-18T08:49:08.290+00:00");
    assert_eq!(body.as_object().unwrap().len(), 3, "{body}");
    // Подпись проверяется открытым ключом: SHA512withRSA строки keyId + timestamp.
    let signature = STANDARD
        .decode(body["signature"].as_str().unwrap())
        .unwrap();
    let signature = Signature::try_from(signature.as_slice()).unwrap();
    let message = format!("{TEST_KEY_ID}2024-06-18T08:49:08.290+00:00");
    VerifyingKey::<Sha512>::new(public_key())
        .verify(message.as_bytes(), &signature)
        .unwrap();
}

#[tokio::test]
async fn tokens_are_cached_and_refreshed_before_expiry() {
    let server = MockServer::start().await;
    Mock::given(path("/public/auth/"))
        .respond_with(token_response(JWE_1, 900))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/public/auth/"))
        .respond_with(token_response(JWE_2, 900))
        .expect(1)
        .mount(&server)
        .await;

    let clock = Arc::new(ManualClock::new(start()));
    let tokens = key_tokens(&server, clock.clone());
    assert_eq!(tokens.token().await.unwrap().expose(), JWE_1);
    // Параллельные запросы получают тот же токен без новых запросов.
    let (a, b) = tokio::join!(tokens.token(), tokens.token());
    assert_eq!(a.unwrap().expose(), JWE_1);
    assert_eq!(b.unwrap().expose(), JWE_1);
    // За минуту до истечения токен ещё действует, позже — заменяется.
    clock.advance(Duration::from_secs(900 - 61));
    assert_eq!(tokens.token().await.unwrap().expose(), JWE_1);
    clock.advance(Duration::from_secs(2));
    assert_eq!(tokens.token().await.unwrap().expose(), JWE_2);
    // Прежние токены известны для вырезания из текстов.
    let known: Vec<String> = tokens
        .known_tokens()
        .iter()
        .map(|t| t.expose().to_owned())
        .collect();
    assert_eq!(known, [JWE_1, JWE_2]);
}

#[tokio::test]
async fn invalidated_token_is_replaced_and_clock_rewind_refreshes() {
    let server = MockServer::start().await;
    Mock::given(path("/public/auth/"))
        .respond_with(token_response(JWE_1, 900))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/public/auth/"))
        .respond_with(token_response(JWE_2, 900))
        .mount(&server)
        .await;

    let clock = Arc::new(ManualClock::new(start()));
    let tokens = key_tokens(&server, clock.clone());
    let first = tokens.token().await.unwrap();
    // Сброс чужого (уже заменённого) токена ничего не меняет.
    tokens
        .invalidate(&rustore_api::AccessToken::new("other"))
        .await;
    assert_eq!(tokens.token().await.unwrap(), first);
    tokens.invalidate(&first).await;
    assert_eq!(tokens.token().await.unwrap().expose(), JWE_2);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    // Часы отвели назад раньше времени получения: токен заменяется.
    clock.set(start() - Duration::from_secs(10));
    tokens.token().await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn ttl_is_capped_and_defaulted() {
    let server = MockServer::start().await;
    Mock::given(path("/public/auth/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK", "body": {"jwe": JWE_1, "ttl": 999_999}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/public/auth/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK", "body": {"jwe": JWE_2}
        })))
        .mount(&server)
        .await;
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = key_tokens(&server, clock.clone());
    assert_eq!(
        tokens.fetch().await.unwrap().lifetime,
        Duration::from_secs(3600)
    );
    assert_eq!(
        tokens.fetch().await.unwrap().lifetime,
        Duration::from_secs(900)
    );
}

#[tokio::test]
async fn rejected_timestamp_is_retried_once_with_a_new_time() {
    let server = MockServer::start().await;
    Mock::given(path("/public/auth/"))
        .respond_with(rejection("Range timestamp not valid"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/public/auth/"))
        .respond_with(token_response(JWE_1, 900))
        .expect(1)
        .mount(&server)
        .await;
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = key_tokens(&server, clock);
    assert_eq!(tokens.token().await.unwrap().expose(), JWE_1);

    // Отказ повторился: часы машины неверны.
    let server = MockServer::start().await;
    Mock::given(path("/public/auth/"))
        .respond_with(rejection("Range timestamp not valid"))
        .expect(2)
        .mount(&server)
        .await;
    let tokens = key_tokens(&server, Arc::new(ManualClock::new(start())));
    let err = tokens.token().await.unwrap_err();
    assert!(matches!(err, TokenError::ClockSkew(_)), "{err:?}");
    assert!(err.to_string().contains("system clock"), "{err}");
}

#[tokio::test]
async fn rejections_are_readable_and_leak_nothing() {
    let server = MockServer::start().await;
    // Сервер отражает присланную подпись и keyId в сообщении.
    Mock::given(path("/public/auth/"))
        .respond_with(|request: &Request| {
            let body = request_json(request);
            let signature = body["signature"].as_str().unwrap().to_owned();
            rejection(&format!("Signature encode error: {signature}"))
        })
        .mount(&server)
        .await;
    let tokens = key_tokens(&server, Arc::new(ManualClock::new(start())));
    let err = tokens.token().await.unwrap_err();
    assert!(
        matches!(err, TokenError::Rejected { status: 400, .. }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("Signature encode error"), "{text}");
    assert!(text.contains("does not belong to this key ID"), "{text}");
    let received = server.received_requests().await.unwrap();
    let signature = request_json(&received[0])["signature"]
        .as_str()
        .unwrap()
        .to_owned();
    let debug = format!("{err:?} {tokens:?}");
    for out in [&text, &debug] {
        assert!(!out.contains(&signature), "{out}");
        assert!(!out.contains(&private_key_base64()[..40]), "{out}");
    }
    assert!(!debug.contains(TEST_KEY_ID), "{debug}");

    for (response, expected) in [
        (rejection("KeyId must not be null"), "Check the key ID"),
        (
            ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"),
            "HTTP 502",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"code": "OK", "body": {}})),
            "no token",
        ),
        (
            ResponseTemplate::new(200).set_body_string("not json"),
            "not JSON",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(path("/public/auth/"))
            .respond_with(response)
            .mount(&server)
            .await;
        let tokens = key_tokens(&server, Arc::new(ManualClock::new(start())));
        let err = tokens.token().await.unwrap_err().to_string();
        assert!(err.contains(expected), "{expected}: {err}");
    }
}

#[tokio::test]
async fn redirects_are_not_followed_and_config_is_checked() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(token_response(JWE_1, 900))
        .expect(0)
        .mount(&other)
        .await;
    Mock::given(path("/public/auth/"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{}/steal", other.uri())),
        )
        .mount(&server)
        .await;
    let tokens = key_tokens(&server, Arc::new(ManualClock::new(start())));
    assert!(tokens.token().await.is_err());

    for base in ["http://public-api-m.rustore.ru", "ftp://example.test"] {
        let err = KeyTokens::with_base_url(api_key(), &url::Url::parse(base).unwrap()).unwrap_err();
        assert!(matches!(err, TokenError::Config(_)), "{base}: {err:?}");
    }
    assert!(KeyTokens::new(api_key()).is_ok());
}

/// Исполнитель с токенами по ключу: заголовок `Public-Token`, повтор 401 с новым токеном,
/// токены не попадают в тексты.
#[tokio::test]
async fn executor_uses_key_tokens() {
    let server = MockServer::start().await;
    Mock::given(path("/public/auth/"))
        .respond_with(token_response(JWE_1, 900))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/public/auth/"))
        .respond_with(token_response(JWE_2, 900))
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/application"))
        .and(header("public-token", JWE_1))
        .respond_with(|request: &Request| {
            let token = request
                .headers
                .get("public-token")
                .unwrap()
                .to_str()
                .unwrap();
            ResponseTemplate::new(401).set_body_json(json!({
                "code": "ERROR", "message": format!("expired token {token}"), "body": null
            }))
        })
        .mount(&server)
        .await;
    Mock::given(path("/public/v1/application"))
        .and(header("public-token", JWE_2))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": "OK", "body": {"content": [], "pageNumber": 0, "totalPages": 1}
        })))
        .mount(&server)
        .await;

    let base = url::Url::parse(&server.uri()).unwrap();
    let source = Arc::new(KeyTokens::with_base_url(api_key(), &base).unwrap());
    let mut config = ClientConfig::new(Some(source)).unwrap();
    config.base_url = base;
    let client = ApiClient::new(config).unwrap();
    let response = client
        .execute(ApiRequest::new(HttpMethod::Get, "/public/v1/application"))
        .await
        .unwrap();
    assert!(matches!(response.body, ResponseBody::Json(_)));
    assert_eq!(
        client.scrub_token(format!("{JWE_1} and {JWE_2}")),
        "*** and ***"
    );
}
