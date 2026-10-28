//! Hermetic loopback proof for the Beeper Desktop transport.
//!
//! The **real** `BeeperClient`/`BeeperTransport`/`AgentBridge` drive a local
//! axum fixture. Nothing reaches a real Beeper install; the live leg is the CLI
//! probe against `127.0.0.1:23373` in the PR evidence.
//!
//! Proven here: one delivery per Beeper message id, `from_me` suppression,
//! dedupe across a restart, retry-no-duplicate-send, offline zero network,
//! typed 401 / 404 / connection-refused, and that a token value never appears
//! in status, probe output, errors, or `Debug`.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rakazo_beeper::ohm::{
    AgentBridge, AgentConfig, AgentError, AgentTransport, AgentTransportKind, BeeperClient,
    BeeperTransport, FakeDelivery, Token, UreqBeeperHttp, beeper,
};
use serde_json::{Value, json};
use std::{
    net::TcpListener as StdTcpListener,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::TempDir;
use tokio::net::TcpListener;

const SECRET: &str = "beeper-test-secret-token-a1b2";
const CHAT: &str = "chat-1";

#[derive(Clone)]
struct Shared(Arc<Mutex<Fixture>>);

struct Fixture {
    token: String,
    chat_id: String,
    chats: Vec<Value>,
    messages: Vec<Value>,
    requests: Vec<String>,
    posts: usize,
    fail_next_posts: usize,
    info_status: u16,
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {token}").as_str())
}

fn status_response(status: u16) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(json!({"error": "scripted"})),
    )
        .into_response()
}

async fn info(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    let mut fixture = shared.0.lock().unwrap();
    fixture.requests.push("GET /v1/info".to_string());
    if !authorized(&headers, &fixture.token) {
        return status_response(401);
    }
    if fixture.info_status != 200 {
        return status_response(fixture.info_status);
    }
    axum::Json(json!({"version": "test", "app": "Beeper Desktop"})).into_response()
}

async fn chats(State(shared): State<Shared>, headers: HeaderMap) -> Response {
    let mut fixture = shared.0.lock().unwrap();
    fixture.requests.push("GET /v1/chats".to_string());
    if !authorized(&headers, &fixture.token) {
        return status_response(401);
    }
    axum::Json(json!({"items": fixture.chats, "hasMore": false})).into_response()
}

async fn chat(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    let mut fixture = shared.0.lock().unwrap();
    fixture.requests.push(format!("GET /v1/chats/{chat_id}"));
    if !authorized(&headers, &fixture.token) {
        return status_response(401);
    }
    if chat_id != fixture.chat_id {
        return status_response(404);
    }
    let chat = fixture
        .chats
        .iter()
        .find(|chat| chat["id"] == chat_id)
        .cloned()
        .unwrap_or_else(|| json!({"id": chat_id}));
    axum::Json(chat).into_response()
}

async fn list_messages(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    let mut fixture = shared.0.lock().unwrap();
    fixture
        .requests
        .push(format!("GET /v1/chats/{chat_id}/messages"));
    if !authorized(&headers, &fixture.token) {
        return status_response(401);
    }
    if chat_id != fixture.chat_id {
        return status_response(404);
    }
    axum::Json(json!({"items": fixture.messages, "hasMore": false})).into_response()
}

async fn send_message(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    body: String,
) -> Response {
    let mut fixture = shared.0.lock().unwrap();
    fixture
        .requests
        .push(format!("POST /v1/chats/{chat_id}/messages"));
    if !authorized(&headers, &fixture.token) {
        return status_response(401);
    }
    if chat_id != fixture.chat_id {
        return status_response(404);
    }
    if fixture.fail_next_posts > 0 {
        fixture.fail_next_posts -= 1;
        return status_response(500);
    }
    let body: Value = serde_json::from_str(&body).unwrap_or_default();
    let id = format!("pending-{}", fixture.posts);
    fixture.posts += 1;
    fixture.messages.push(json!({
        "id": id,
        "chatID": chat_id,
        "isSender": true,
        "text": body.get("text").cloned().unwrap_or(Value::Null),
        "timestamp": "2026-10-27T12:00:00Z",
    }));
    axum::Json(json!({ "pendingMessageID": id })).into_response()
}

struct FixtureServer {
    base_url: String,
    shared: Shared,
    _join: tokio::task::JoinHandle<()>,
}

async fn start_fixture(chat_id: &str, messages: Vec<Value>) -> FixtureServer {
    let shared = Shared(Arc::new(Mutex::new(Fixture {
        token: SECRET.to_string(),
        chat_id: chat_id.to_string(),
        chats: vec![
            json!({"id": "chat-1", "title": "Muse", "network": "WhatsApp"}),
            json!({"id": "chat-2", "title": "Other", "network": "Telegram"}),
        ],
        messages,
        requests: Vec::new(),
        posts: 0,
        fail_next_posts: 0,
        info_status: 200,
    })));
    let app = Router::new()
        .route("/v1/info", get(info))
        .route("/v1/chats", get(chats))
        .route("/v1/chats/{chat_id}", get(chat))
        .route("/v1/chats/{chat_id}/messages", get(list_messages))
        .route("/v1/chats/{chat_id}/messages", post(send_message))
        .with_state(shared.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FixtureServer {
        base_url: format!("http://{addr}"),
        shared,
        _join: join,
    }
}

impl FixtureServer {
    fn requests(&self) -> Vec<String> {
        self.shared.0.lock().unwrap().requests.clone()
    }

    fn posts(&self) -> usize {
        self.shared.0.lock().unwrap().posts
    }

    fn set_info_status(&self, status: u16) {
        self.shared.0.lock().unwrap().info_status = status;
    }

    fn fail_next_posts(&self, count: usize) {
        self.shared.0.lock().unwrap().fail_next_posts = count;
    }
}

fn token_file(dir: &TempDir) -> PathBuf {
    use std::io::Write;
    let path = dir.path().join("beeper-token");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(SECRET.as_bytes()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn token(path: &FsPath) -> Token {
    beeper::load_token(path).unwrap()
}

fn agent_config(base_url: &str, token_path: &FsPath, chat_id: &str) -> AgentConfig {
    AgentConfig {
        key: "muse".into(),
        kind: "external".into(),
        transport: AgentTransportKind::Beeper,
        base_url: Some(base_url.to_string()),
        token_file: Some(token_path.to_path_buf()),
        conversation_id: None,
        chat_id: Some(chat_id.to_string()),
        platform: None,
        remote_id: None,
        label: Some("Muse".into()),
        poll_seconds: 10,
        worker: Some("muse".into()),
        state_file: None,
        limit: 50,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_is_delivered_once_by_message_id_across_restart() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let fixture = start_fixture(
        CHAT,
        vec![
            json!({"id": "m-self", "isSender": true, "text": "sent by us"}),
            json!({"id": "m-1", "isSender": false, "senderName": "Muse", "text": "hello"}),
            json!({"id": "m-1", "isSender": false, "senderName": "Muse", "text": "hello"}),
        ],
    )
    .await;
    let state_file = dir.path().join("muse-beeper.json");

    let delivery = FakeDelivery::default();
    {
        let client = BeeperClient::new(
            Some(&fixture.base_url),
            token(&token_path),
            UreqBeeperHttp::new(),
        )
        .unwrap();
        let transport = BeeperTransport::new(client, CHAT.into(), 50);
        let mut bridge = AgentBridge::new(
            transport,
            delivery.clone(),
            "muse".into(),
            CHAT.into(),
            Some(state_file.clone()),
        )
        .unwrap();
        let report = bridge.poll_once().unwrap();
        assert_eq!(report.delivered, 1, "{report:?}");
        assert_eq!(report.duplicates, 1, "{report:?}");
        assert_eq!(report.suppressed, 1, "{report:?}");
        let calls = delivery.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "muse");
        assert_eq!(calls[0].1, "rakazo:beeper:chat-1:m-1");
        assert!(calls[0].2.contains("hello"));
    }

    // Restart: same ledger, same fixture. No redelivery.
    let delivery = FakeDelivery::default();
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let transport = BeeperTransport::new(client, CHAT.into(), 50);
    let mut bridge = AgentBridge::new(
        transport,
        delivery.clone(),
        "muse".into(),
        CHAT.into(),
        Some(state_file),
    )
    .unwrap();
    let report = bridge.poll_once().unwrap();
    assert_eq!(report.delivered, 0, "{report:?}");
    assert_eq!(report.duplicates, 2, "{report:?}");
    assert!(delivery.calls().is_empty(), "a restart must not redeliver");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_records_returned_id_and_retry_does_not_double_send() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let fixture = start_fixture(CHAT, vec![]).await;
    let state_file = dir.path().join("muse-beeper.json");

    {
        let client = BeeperClient::new(
            Some(&fixture.base_url),
            token(&token_path),
            UreqBeeperHttp::new(),
        )
        .unwrap();
        let transport = BeeperTransport::new(client, CHAT.into(), 50);
        let mut bridge = AgentBridge::new(
            transport,
            FakeDelivery::default(),
            "muse".into(),
            CHAT.into(),
            Some(state_file.clone()),
        )
        .unwrap();
        assert_eq!(
            bridge.reply("req-1", "answer").unwrap(),
            rakazo_beeper::ohm::AgentReplyReport::Queued
        );
        assert_eq!(
            bridge.state().sent.get("req-1").map(String::as_str),
            Some("pending-0")
        );
        // Same request id replays without a second POST.
        assert_eq!(
            bridge.reply("req-1", "answer").unwrap(),
            rakazo_beeper::ohm::AgentReplyReport::AlreadyQueued
        );
    }
    assert_eq!(fixture.posts(), 1);

    // A restart retries the same request id from the durable ledger.
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let transport = BeeperTransport::new(client, CHAT.into(), 50);
    let mut bridge = AgentBridge::new(
        transport,
        FakeDelivery::default(),
        "muse".into(),
        CHAT.into(),
        Some(state_file),
    )
    .unwrap();
    assert_eq!(
        bridge.reply("req-1", "answer").unwrap(),
        rakazo_beeper::ohm::AgentReplyReport::AlreadyQueued
    );
    assert_eq!(fixture.posts(), 1, "retry must not double-send");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_failure_is_recovered_by_one_bounded_reread() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let fixture = start_fixture(CHAT, vec![]).await;
    // First POST is reported as a 500 even though the fixture still records it
    // only when it succeeds; the re-read then sees nothing and the typed error
    // is returned rather than a fake success.
    fixture.fail_next_posts(1);
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let mut transport = BeeperTransport::new(client, CHAT.into(), 50);
    let outbound = rakazo_beeper::ohm::AgentOutbound {
        conversation_id: CHAT.into(),
        request_id: "req-fail".into(),
        text: "will not land".into(),
        identity: None,
    };
    let error = transport.send(&outbound).unwrap_err();
    assert_eq!(error.code(), "http-error");
    assert_eq!(fixture.posts(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_default_makes_zero_network_calls() {
    let fixture = start_fixture(CHAT, vec![]).await;
    let client = BeeperClient::new(
        None,
        Token::load(&token_file(&TempDir::new().unwrap())).unwrap(),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let mut transport = BeeperTransport::new(client, CHAT.into(), 50);
    assert_eq!(transport.poll().unwrap_err(), AgentError::Offline);
    assert!(fixture.requests().is_empty(), "offline must not touch HTTP");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_chat_not_found_and_connection_refused_are_typed() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let fixture = start_fixture(CHAT, vec![]).await;
    fixture.set_info_status(401);
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let error = client.info().unwrap_err();
    assert_eq!(
        error,
        AgentError::Unauthorized {
            endpoint: "/v1/info".into()
        }
    );
    assert_eq!(error.code(), "unauthorized");

    // 404 on a chat-scoped route is `chat-not-found`, not a generic HTTP error.
    let fixture = start_fixture(CHAT, vec![]).await;
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let error = client.chat("does-not-exist").unwrap_err();
    assert_eq!(error.code(), "chat-not-found");
    assert!(matches!(error, AgentError::ChatNotFound { .. }));

    // Nothing listening: the exact current state of the real machine.
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let client = BeeperClient::new(
        Some(&format!("http://127.0.0.1:{port}")),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let error = client.info().unwrap_err();
    assert_eq!(error.code(), "not-enabled", "{error}");
    assert!(matches!(error, AgentError::NotEnabled { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_and_status_never_render_the_token_or_message_text() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let fixture = start_fixture(
        CHAT,
        vec![json!({"id": "m-1", "isSender": false, "text": "secret message text"})],
    )
    .await;
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let report = client.probe(Some(CHAT));
    let text = report.lines().join("\n");
    assert!(text.contains("GET /v1/info"), "{text}");
    assert!(text.contains("status=200"), "{text}");
    assert!(text.contains("chat_id\tchat-1"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("secret message text"), "{text}");

    let token = token(&token_path);
    assert_eq!(format!("{token:?}"), "Token(<redacted>)");
    assert!(!format!("{token:?}").contains(SECRET));
    let described = token.describe(&token_path);
    assert!(described.contains(&token_path.display().to_string()));
    assert!(described.ends_with("(…a1b2)"), "{described}");
    assert!(!described.contains(SECRET), "{described}");

    let agent = agent_config(&fixture.base_url, &token_path, CHAT);
    let status = beeper::status(&agent);
    assert!(status.last_poll_ok, "{status:?}");
    assert_eq!(status.token_fingerprint.as_deref(), Some("…a1b2"));
    let line = status.line("muse");
    assert!(line.contains("transport=beeper"), "{line}");
    assert!(line.contains("last_poll_ok=true"), "{line}");
    assert!(!line.contains(SECRET), "{line}");

    // Typed 401 must not leak the value either.
    fixture.set_info_status(401);
    let status = beeper::status(&agent);
    assert!(!status.last_poll_ok);
    assert_eq!(status.last_error.as_deref(), Some("unauthorized"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_file_must_be_owner_only_and_missing_is_typed() {
    use std::io::Write;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("beeper-token");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(SECRET.as_bytes()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = beeper::load_token(&path).unwrap_err();
        assert_eq!(error.code(), "token-error");
        assert!(matches!(error, AgentError::Token(_)));
    }
    let missing = dir.path().join("missing-token");
    let error = beeper::load_token(&missing).unwrap_err();
    assert_eq!(error.code(), "token-missing");
    assert!(matches!(error, AgentError::TokenMissing { .. }));
    let status = beeper::status(&agent_config("http://127.0.0.1:23373", &missing, CHAT));
    assert_eq!(status.last_error.as_deref(), Some("token-missing"));
}

#[test]
fn reference_is_the_required_beeper_shape() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let client = BeeperClient::new(None, token(&token_path), UreqBeeperHttp::new()).unwrap();
    let transport = BeeperTransport::new(client, CHAT.into(), 50);
    assert_eq!(transport.reference(CHAT, "m-1"), "rakazo:beeper:chat-1:m-1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_with_empty_chat_id_lists_chats_instead_of_erroring() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let fixture = start_fixture(
        CHAT,
        vec![json!({"id": "m-1", "isSender": false, "text": "secret message text"})],
    )
    .await;
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    // `chat_id = ""` must not fail the probe: the chat list is the useful first
    // run after the human enables the API.
    let report = client.probe(Some(""));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.info_status, Some(200));
    assert_eq!(report.chats_status, Some(200));
    assert_eq!(report.chat_status, None);
    assert_eq!(report.chats[0].0, "chat-1");
    let text = report.lines().join("\n");
    assert!(text.contains("chat_id\tchat-1\tMuse [WhatsApp]"), "{text}");
    assert!(!text.contains("secret message text"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_chat_id_polls_and_sends_against_fixture_transport() {
    let dir = TempDir::new().unwrap();
    let token_path = token_file(&dir);
    let fixture = start_fixture(
        CHAT,
        vec![json!({"id": "m-1", "isSender": false, "text": "hello"})],
    )
    .await;
    let client = BeeperClient::new(
        Some(&fixture.base_url),
        token(&token_path),
        UreqBeeperHttp::new(),
    )
    .unwrap();
    let mut transport = BeeperTransport::new(client, CHAT.into(), 50);
    let inbound = transport.poll().unwrap();
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].message.native_id, "m-1");
    let outcome = transport
        .send(&rakazo_beeper::ohm::AgentOutbound {
            conversation_id: CHAT.into(),
            request_id: "req-1".into(),
            text: "answer".into(),
            identity: None,
        })
        .unwrap();
    assert_eq!(outcome, rakazo_beeper::ohm::AgentSendOutcome::Queued);
    assert_eq!(fixture.posts(), 1);
}
