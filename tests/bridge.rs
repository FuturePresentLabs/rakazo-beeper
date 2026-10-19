use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, Request, StatusCode, Uri},
    routing::any,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rakazo_beeper::{
    config::{RegistrationBundle, Tenant},
    matrix::Matrix,
    rakazo::Rakazo,
    service::{Runtime, TenantRuntime, text_chunks},
    stable_id,
    store::{Inbound, Store},
    transport,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tempfile::TempDir;
use tower::ServiceExt;

fn key() -> String {
    STANDARD.encode([42; 32])
}

#[derive(Default)]
struct Remote {
    avatar_color: Option<String>,
    bot_name: Option<String>,
    fail_profile: bool,
    requests: Vec<(String, Value, HeaderMap)>,
    messages: Vec<Value>,
    busy: bool,
    encryption: bool,
    intruder: bool,
    fail_send: bool,
    fail_matrix_send: bool,
    fail_create: bool,
    created_room: Option<Value>,
    rakazo_nonces: HashSet<String>,
    matrix_transactions: HashMap<String, String>,
}
type Shared = Arc<Mutex<Remote>>;

async fn mock(
    State(remote): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let input: Value = if uri.path().contains("/media/v3/upload") {
        assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(headers["content-type"], "image/png");
        json!({"bytes":body.len()})
    } else if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let path = uri.path().to_string();
    let mut state = remote.lock().unwrap();
    state
        .requests
        .push((uri.to_string(), input.clone(), headers.clone()));
    let ok = |value| (StatusCode::OK, Json(value));
    if path.starts_with("/rpc/") {
        assert_eq!(headers["authorization"], "Bearer test-rakazo-session");
        assert_eq!(headers["x-rakazo-space-id"], "space-a");
        assert!(input.get("json").is_some());
        let args = &input["json"];
        return match path.as_str() {
            "/rpc/bots/list" => ok(
                json!({"json":[{"id":"agent-a","name":state.bot_name.as_deref().unwrap_or("Ada"),"color":state.avatar_color.as_deref().unwrap_or("#3EC5A8")},{"id":"agent-b","name":"Grace","color":"#9B5CF6"}]}),
            ),
            "/rpc/me" => ok(json!({"json":{"avatarStyle":"organic"}})),
            "/rpc/threads/get" => {
                let latest = state
                    .messages
                    .iter()
                    .rev()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>();
                ok(
                    json!({"json":{"messages":latest,"run":if state.busy {json!({"status":"running"})}else{Value::Null},"activeRuns":[]}}),
                )
            }
            "/rpc/threads/messages" => {
                let before = args["before"].as_i64().unwrap_or(i64::MAX);
                let mut rows = state
                    .messages
                    .iter()
                    .filter(|m| m["seq"].as_i64().unwrap() < before)
                    .rev()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>();
                rows.reverse();
                let first = rows.first().and_then(|m| m["seq"].as_i64());
                let more = first.is_some_and(|seq| {
                    state
                        .messages
                        .iter()
                        .any(|m| m["seq"].as_i64().unwrap() < seq)
                });
                ok(json!({"json":{"messages":rows,"olderCursor":if more{first}else{None}}}))
            }
            "/rpc/threads/send" => {
                state
                    .rakazo_nonces
                    .insert(args["clientNonce"].as_str().unwrap().to_string());
                if std::mem::take(&mut state.fail_send) {
                    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
                } else {
                    ok(json!({"json":{"taskId":"task-a","runId":"run-a","seq":2}}))
                }
            }
            _ => panic!("unexpected RPC route {path}"),
        };
    }
    assert_eq!(
        headers["authorization"],
        "Bearer appservice-token-at-least-20-chars"
    );
    if path.ends_with("/register") {
        return ok(json!({"user_id":"@ghost:beeper.local"}));
    }
    if path.ends_with("/displayname") {
        return ok(json!({}));
    }
    if path.ends_with("/media/v3/upload") {
        let count = state
            .requests
            .iter()
            .filter(|(path, _, _)| path.contains("/media/v3/upload"))
            .count();
        return ok(json!({"content_uri":format!("mxc://beeper.local/avatar-{count}")}));
    }
    if path.ends_with("/avatar_url")
        || path.contains("/state/m.room.avatar/")
        || path.contains("/state/m.room.name/")
    {
        if std::mem::take(&mut state.fail_profile) {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        }
        return ok(json!({}));
    }
    if path.ends_with("/joined_rooms") {
        return ok(
            json!({"joined_rooms":if state.created_room.is_some(){vec!["!agent-a:beeper.local"]}else{vec![]}}),
        );
    }
    if path.ends_with("/createRoom") {
        assert_eq!(input["power_level_content_override"]["invite"], 100);
        assert_eq!(
            input["power_level_content_override"]["users"]["@owner:beeper.com"],
            0
        );
        assert!(
            input["power_level_content_override"]["users"]
                .as_object()
                .unwrap()
                .keys()
                .any(|key| key.starts_with("@sh-rakazo_"))
        );
        assert!(
            input["initial_state"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["type"] != "m.room.encryption")
        );
        assert!(input.get("room_alias_name").is_none());
        state.created_room = Some(input.clone());
        if std::mem::take(&mut state.fail_create) {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        }
        return ok(json!({"room_id":"!agent-a:beeper.local"}));
    }
    if path.ends_with("/state") {
        let user = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .find(|(k, _)| k == "user_id")
            .unwrap()
            .1
            .into_owned();
        let mut members = vec![
            json!({"type":"m.room.member","state_key":user,"content":{"membership":"join"}}),
            json!({"type":"m.room.member","state_key":"@owner:beeper.com","content":{"membership":"join"}}),
        ];
        if let Some(created) = &state.created_room {
            members.extend(created["initial_state"].as_array().unwrap().iter().cloned());
        }
        if state.encryption {
            members.push(
                json!({"type":"m.room.encryption","content":{"algorithm":"m.megolm.v1.aes-sha2"}}),
            );
        }
        if state.intruder {
            members.push(json!({"type":"m.room.member","state_key":"@intruder:beeper.com","content":{"membership":"invite"}}));
        }
        return ok(json!(members));
    }
    if method == Method::PUT && path.contains("/send/m.room.message/") {
        state
            .matrix_transactions
            .insert(path.clone(), input["body"].as_str().unwrap().to_string());
        if std::mem::take(&mut state.fail_matrix_send) {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        }
        return ok(json!({"event_id":"$delivered"}));
    }
    panic!("unexpected mock request: {method} {path}");
}

struct Fixture {
    _temp: TempDir,
    remote: Shared,
    runtime: Arc<TenantRuntime>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture() -> Fixture {
    let remote: Shared = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(any(mock)).with_state(remote.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let config = Tenant {
        id: "tenant-a".into(),
        owner: "@owner:beeper.com".into(),
        registration: temp.path().join("registration.json"),
        homeserver: base.parse().unwrap(),
        rakazo_url: base.parse().unwrap(),
        rakazo_token_env: "UNUSED_TEST_TOKEN".into(),
        rakazo_space_id: "space-a".into(),
        bot_ids: vec!["agent-a".into()],
        websocket: false,
    };
    let bundle = json!({"homeserver_url":base,"homeserver_domain":"beeper.local","your_user_id":config.owner,
        "registration":{"id":"registration-a","sender_localpart":"sh-rakazobot","as_token":"appservice-token-at-least-20-chars","hs_token":"homeserver-token-at-least-20-chars",
        "namespaces":{"users":[{"regex":"@sh-rakazo_.+:beeper.local","exclusive":true}]}}});
    std::fs::write(&config.registration, bundle.to_string()).unwrap();
    let bundle: RegistrationBundle = serde_json::from_value(bundle).unwrap();
    let store = Arc::new(Store::open(&temp.path().join("bridge.db"), &key()).unwrap());
    let runtime = Arc::new(TenantRuntime {
        matrix: Matrix::new(&config, &bundle).unwrap(),
        rakazo: Rakazo::with_token(&config, "test-rakazo-session").unwrap(),
        config,
        store,
        hs_token: bundle.registration.hs_token,
        as_token: bundle.registration.as_token,
        connected: AtomicBool::new(true),
        last_success: AtomicU64::new(0),
        unsupported_encryption: AtomicBool::new(false),
        initialized: AtomicBool::new(false),
    });
    Fixture {
        _temp: temp,
        remote,
        runtime,
        server,
    }
}

fn message(seq: i64, role: &str, text: &str) -> Value {
    json!({"id":format!("message-{seq}"),"seq":seq,"role":role,"blocks":[{"kind":"text","text":text}]})
}
fn event(id: &str, sender: &str) -> Value {
    json!({"event_id":id,"room_id":"!agent-a:beeper.local","sender":sender,"type":"m.room.message","content":{"msgtype":"m.text","body":"Hello Ada"}})
}

#[tokio::test]
async fn profile_pictures_sync_without_duplicate_uploads_and_retry_partial_updates() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let uploads = || {
        f.remote
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(path, _, _)| path.contains("/media/v3/upload"))
            .count()
    };
    assert_eq!(uploads(), 1);
    let profile = f
        .runtime
        .store
        .profile("tenant-a", "agent-a")
        .unwrap()
        .unwrap();
    assert_eq!(profile.avatar_uri, "mxc://beeper.local/avatar-1");
    f.runtime.discover().await.unwrap();
    assert_eq!(uploads(), 1);
    {
        let mut remote = f.remote.lock().unwrap();
        remote.bot_name = Some("Ada Lovelace".into());
        remote.fail_profile = true;
    }
    assert!(f.runtime.discover().await.is_err());
    f.runtime.discover().await.unwrap();
    assert_eq!(
        uploads(),
        1,
        "renames and profile retries reuse uploaded media"
    );
    f.remote.lock().unwrap().avatar_color = Some("#D9508A::shape_3".into());
    f.runtime.discover().await.unwrap();
    assert_eq!(uploads(), 2);
    let state = f.remote.lock().unwrap();
    assert!(
        state
            .requests
            .iter()
            .any(|(path, body, _)| path.contains("/state/m.room.avatar/")
                && body["url"] == "mxc://beeper.local/avatar-2")
    );
    assert!(
        state
            .requests
            .iter()
            .any(|(path, body, _)| path.contains("/state/m.room.name/")
                && body["name"] == "Ada Lovelace · Rakazo")
    );
    let profile = f
        .runtime
        .store
        .profile("tenant-a", "agent-a")
        .unwrap()
        .unwrap();
    assert!(!profile.applied_hash.is_empty());
    assert!(
        f.runtime
            .store
            .profile("another-tenant", "agent-a")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn round_trip_retries_use_stable_remote_idempotency_keys() {
    let f = fixture().await;
    f.remote.lock().unwrap().messages = vec![message(0, "bot", "old history")];
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    assert_eq!(binding.cursor, 0); // Never replay old history into a newly connected room.
    let payload = json!({"events":[event("$hello","@owner:beeper.com")]});
    f.runtime.ingest("txn-1", &payload).unwrap();
    f.runtime.ingest("txn-1", &payload).unwrap();
    f.runtime.ingest("txn-2", &payload).unwrap();
    assert_eq!(f.runtime.store.pending_count().unwrap(), 1);
    f.remote.lock().unwrap().fail_send = true;
    assert!(f.runtime.deliver_inbound().await.is_err());
    let pending = f.runtime.store.next("tenant-a", i64::MAX).unwrap().unwrap();
    assert_eq!(pending.attempts, 1);
    f.runtime
        .matrix
        .check_room(&binding.room, &binding.ghost, &f.runtime.config.owner)
        .await
        .unwrap();
    f.runtime
        .rakazo
        .send(
            &binding.bot,
            &pending.event.text,
            &stable_id(&["tenant-a", "$hello"]),
        )
        .await
        .unwrap();
    f.runtime.store.complete("tenant-a", pending.id).unwrap();
    assert_eq!(f.remote.lock().unwrap().rakazo_nonces.len(), 1);
    {
        let mut remote = f.remote.lock().unwrap();
        remote.messages.extend([
            message(1, "user", "Hello Ada"),
            message(2, "bot", "Hello Avery"),
        ]);
        remote.fail_matrix_send = true;
    }
    assert!(f.runtime.poll_binding(&binding).await.is_err());
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    assert_eq!(binding.cursor, 1);
    f.runtime.poll_binding(&binding).await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.runtime.poll_binding(&binding).await.unwrap();
    let state = f.remote.lock().unwrap();
    assert_eq!(state.matrix_transactions.len(), 1);
    assert_eq!(
        state.matrix_transactions.values().next().unwrap(),
        "Hello Avery"
    );
    assert_eq!(binding.cursor, 2);
}

#[tokio::test]
async fn interrupted_room_creation_recovers_without_resetting_the_initial_cursor() {
    let f = fixture().await;
    {
        let mut state = f.remote.lock().unwrap();
        state.messages.push(message(0, "bot", "old history"));
        state.fail_create = true;
    }
    assert!(f.runtime.discover().await.is_err());
    assert!(f.runtime.store.bindings("tenant-a").unwrap().is_empty());
    f.remote
        .lock()
        .unwrap()
        .messages
        .push(message(1, "bot", "reply after interrupted creation"));
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    assert_eq!(binding.cursor, 0);
    f.runtime.poll_binding(&binding).await.unwrap();
    let state = f.remote.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(path, _, _)| path.contains("/createRoom"))
            .count(),
        1
    );
    assert_eq!(state.matrix_transactions.len(), 1);
}

#[tokio::test]
async fn history_pagination_recovers_every_message_in_order_and_waits_for_completion() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    {
        let mut remote = f.remote.lock().unwrap();
        remote.messages = (0..9)
            .map(|i| message(i, "bot", &format!("reply {i}")))
            .collect();
        remote.busy = true;
    }
    f.runtime.poll_binding(&binding).await.unwrap();
    assert!(f.remote.lock().unwrap().matrix_transactions.is_empty());
    f.remote.lock().unwrap().busy = false;
    f.runtime.poll_binding(&binding).await.unwrap();
    let state = f.remote.lock().unwrap();
    let bodies: Vec<_> = state
        .requests
        .iter()
        .filter(|(p, _, _)| p.contains("/send/m.room.message/"))
        .map(|(_, v, _)| v["body"].as_str().unwrap())
        .collect();
    assert_eq!(
        bodies,
        (0..9).map(|i| format!("reply {i}")).collect::<Vec<_>>()
    );
    assert_eq!(f.runtime.store.bindings("tenant-a").unwrap()[0].cursor, 8);
}

#[tokio::test]
async fn encrypted_or_shared_rooms_fail_closed_in_both_directions() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.runtime
        .ingest(
            "txn",
            &json!({"events":[event("$hello","@owner:beeper.com")]}),
        )
        .unwrap();
    f.remote
        .lock()
        .unwrap()
        .messages
        .push(message(0, "bot", "private reply"));
    f.remote.lock().unwrap().encryption = true;
    assert!(f.runtime.deliver_inbound().await.is_err());
    assert!(f.runtime.poll_binding(&binding).await.is_err());
    {
        let mut remote = f.remote.lock().unwrap();
        remote.encryption = false;
        remote.intruder = true;
    }
    assert!(f.runtime.poll_binding(&binding).await.is_err());
    let state = f.remote.lock().unwrap();
    assert!(state.rakazo_nonces.is_empty());
    assert!(state.matrix_transactions.is_empty());
    assert_eq!(f.runtime.store.pending_count().unwrap(), 1);
}

#[tokio::test]
async fn strangers_ghost_echoes_unknown_rooms_and_disabled_bots_cannot_trigger_agents() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let ghost = f.runtime.store.bindings("tenant-a").unwrap()[0]
        .ghost
        .clone();
    let mut unknown = event("$unknown", "@owner:beeper.com");
    unknown["room_id"] = json!("!another-room");
    let mut edit = event("$edit", "@owner:beeper.com");
    edit["content"]["m.relates_to"] = json!({"rel_type":"m.replace"});
    f.runtime.ingest("txn",&json!({"events":[event("$stranger","@stranger:beeper.com"),event("$echo",&ghost),unknown,edit]})).unwrap();
    assert_eq!(f.runtime.store.pending_count().unwrap(), 0);
    let mut encrypted = event("$encrypted", "@owner:beeper.com");
    encrypted["type"] = json!("m.room.encrypted");
    f.runtime
        .ingest("encrypted", &json!({"events":[encrypted]}))
        .unwrap();
    assert!(f.runtime.unsupported_encryption.load(Ordering::Relaxed));
}

#[tokio::test]
async fn cli_accepts_private_secret_files_and_rejects_ambiguous_credentials() {
    let f = fixture().await;
    let root = f._temp.path();
    let token_path = root.join("rakazo-token");
    std::fs::write(&token_path, "test-rakazo-session\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config_path = root.join("config.toml");
    let value = json!({"database":"data/bridge.db","encryption_key_env":"BRIDGE_TEST_KEY","tenants":[{
        "id":"tenant-a","owner":f.runtime.config.owner,"registration":"registration.json",
        "homeserver":f.runtime.config.homeserver,"rakazo_url":f.runtime.config.rakazo_url,
        "rakazo_token_env":"BRIDGE_TEST_TOKEN","rakazo_space_id":"space-a","bot_ids":["agent-a"],"websocket":false
    }]});
    std::fs::write(&config_path, toml::to_string(&value).unwrap()).unwrap();
    let output = tokio::task::spawn_blocking({
        let config_path = config_path.clone();
        let token_path = token_path.clone();
        move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_rakazo-beeper"))
                .args(["--config", config_path.to_str().unwrap(), "check"])
                .env_remove("BRIDGE_TEST_TOKEN")
                .env_remove("BRIDGE_TEST_KEY_FILE")
                .env("BRIDGE_TEST_TOKEN_FILE", token_path)
                .env("BRIDGE_TEST_KEY", key())
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("data/bridge.db").exists());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rakazo-beeper"))
        .args(["--config", config_path.to_str().unwrap(), "check"])
        .env("BRIDGE_TEST_TOKEN", "another-token")
        .env("BRIDGE_TEST_TOKEN_FILE", &token_path)
        .env("BRIDGE_TEST_KEY", key())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("set only one"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_rakazo-beeper"))
            .args(["--config", config_path.to_str().unwrap(), "check"])
            .env_remove("BRIDGE_TEST_TOKEN")
            .env("BRIDGE_TEST_TOKEN_FILE", &token_path)
            .env("BRIDGE_TEST_KEY", key())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("accessible only to its owner"));
    }
}

#[tokio::test]
async fn http_requires_tenant_specific_homeserver_token_and_does_not_ack_invalid_payloads() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let mut other = fixture().await;
    let other_runtime = Arc::get_mut(&mut other.runtime).unwrap();
    other_runtime.config.id = "tenant-b".into();
    other_runtime.hs_token = "different-tenant-homeserver-token".into();
    other.runtime.discover().await.unwrap();
    let app = transport::router(Arc::new(Runtime {
        tenants: vec![f.runtime.clone(), other.runtime.clone()],
        poll_seconds: 5,
        store: f.runtime.store.clone(),
    }));
    let request = |token: &str, tenant: &str, body: Value| {
        Request::builder()
            .method("PUT")
            .uri(format!("/tenants/{tenant}/_matrix/app/v1/transactions/txn"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let payload = json!({"events":[event("$hello","@owner:beeper.com")]});
    assert_eq!(
        app.clone()
            .oneshot(request("wrong", "tenant-a", payload.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(request(&f.runtime.hs_token, "tenant-b", payload.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(request(&f.runtime.hs_token, "tenant-a", json!({})))
            .await
            .unwrap()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        app.clone()
            .oneshot(request(&f.runtime.hs_token, "tenant-a", payload))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(f.runtime.store.pending_count().unwrap(), 1);
    assert_eq!(
        app.oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .unwrap()
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn queue_survives_restart_is_encrypted_and_tenant_scoped() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bridge.db");
    let event = Inbound {
        event_id: "$same".into(),
        room: "!room".into(),
        text: "TOP SECRET MESSAGE BODY".into(),
    };
    {
        let store = Store::open(&path, &key()).unwrap();
        store.bind_tenant("a", "identity-a").unwrap();
        store
            .cache_avatar("a", "agent", "avatar-hash", "mxc://beeper.local/picture")
            .unwrap();
        store.applied_profile("a", "agent", "profile-hash").unwrap();
        assert!(store.bind_tenant("a", "identity-b").is_err());
        store.ingest("a", "same-txn", &[event]).unwrap();
        store
            .ingest(
                "b",
                "same-txn",
                &[Inbound {
                    event_id: "$same".into(),
                    room: "!room".into(),
                    text: "different tenant".into(),
                }],
            )
            .unwrap();
        assert_eq!(
            store.next("a", 0).unwrap().unwrap().event.text,
            "TOP SECRET MESSAGE BODY"
        );
        assert_eq!(
            store.next("b", 0).unwrap().unwrap().event.text,
            "different tenant"
        );
        assert!(Store::open(&path, &key()).is_err());
    }
    assert!(
        !String::from_utf8_lossy(&std::fs::read(&path).unwrap())
            .contains("TOP SECRET MESSAGE BODY")
    );
    assert!(Store::open(&path, &STANDARD.encode([7; 32])).is_err());
    let store = Store::open(&path, &key()).unwrap();
    let profile = store.profile("a", "agent").unwrap().unwrap();
    assert_eq!(profile.avatar_uri, "mxc://beeper.local/picture");
    assert_eq!(profile.applied_hash, "profile-hash");
    assert!(store.profile("b", "agent").unwrap().is_none());
    assert_eq!(store.pending_count().unwrap(), 2);
    let pending = store.next("a", 0).unwrap().unwrap();
    store.retry("a", pending.id, 0, 10).unwrap();
    assert!(store.next("a", 10).unwrap().is_none());
    assert!(store.next("a", 11).unwrap().is_some());
    store.complete("b", pending.id).unwrap(); // Another tenant cannot mark this event sent.
    assert_eq!(store.pending_count().unwrap(), 2);
    store.complete("a", pending.id).unwrap();
    assert_eq!(store.pending_count().unwrap(), 1);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE inbox SET tenant='c' WHERE tenant='b'", [])
        .unwrap();
    assert!(store.next("c", i64::MAX).is_err()); // Ciphertext cannot move between tenant identities.
}

#[test]
fn chunking_preserves_unicode_and_identifiers_cannot_collide_by_concatenation() {
    let text = "😀hello!".repeat(20_000);
    let chunks = text_chunks(&text, 40_000);
    assert!(chunks.iter().all(|c| c.len() <= 40_000));
    assert_eq!(chunks.concat(), text);
    assert_ne!(stable_id(&["ab", "c"]), stable_id(&["a", "bc"]));
    assert_ne!(
        stable_id(&["tenant-a", "bot"]),
        stable_id(&["tenant-b", "bot"])
    );
}

#[test]
fn thread_run_states_keep_leased_and_unknown_runs_out_of_the_transcript() {
    for status in ["queued", "leased", "running", "unknown-future-state"] {
        let snapshot: rakazo_beeper::rakazo::Snapshot =
            serde_json::from_value(json!({"messages":[],"run":{"status":status}})).unwrap();
        assert!(snapshot.busy(), "{status}");
    }
    for status in [
        "completed",
        "failed",
        "cancelled",
        "waiting_input",
        "waiting_takeover",
    ] {
        let snapshot: rakazo_beeper::rakazo::Snapshot =
            serde_json::from_value(json!({"messages":[],"run":{"status":status}})).unwrap();
        assert!(!snapshot.busy(), "{status}");
    }
}

#[tokio::test]
// Tungstenite's callback signature fixes the unboxed ErrorResponse type.
#[allow(clippy::result_large_err)]
async fn native_beeper_websocket_commits_before_acknowledging() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_hdr_async, tungstenite::Message};
    let mut f = fixture().await;
    f.runtime.discover().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let runtime = Arc::get_mut(&mut f.runtime).unwrap();
    runtime.config.homeserver = base.parse().unwrap();
    let store = runtime.store.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(
            stream,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                assert_eq!(
                    request.uri().path(),
                    "/_matrix/client/unstable/fi.mau.as_sync"
                );
                assert_eq!(request.headers()["x-mautrix-websocket-version"], "3");
                assert_eq!(
                    request.headers()["authorization"],
                    "Bearer appservice-token-at-least-20-chars"
                );
                Ok(response)
            },
        )
        .await
        .unwrap();
        let status: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(status["command"], "bridge_status");
        ws.send(Message::Text(json!({"id":23,"command":"transaction","txn_id":"ws-txn","events":[event("$ws","@owner:beeper.com")]}).to_string().into())).await.unwrap();
        let ack: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(
            ack,
            json!({"id":23,"command":"response","data":{"txn_id":"ws-txn"}})
        );
        assert_eq!(store.pending_count().unwrap(), 1);
        ws.close(None).await.unwrap();
    });
    assert!(
        transport::websocket_session(f.runtime.clone())
            .await
            .is_err()
    );
    server.await.unwrap();
}
