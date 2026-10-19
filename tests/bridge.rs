use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, get},
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
use sha2::Digest;
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
    fail_backfill: bool,
    history_events: HashMap<String, Value>,
    timeline: Vec<String>,
    avatar_color: Option<String>,
    bot_name: Option<String>,
    fail_profile: bool,
    requests: Vec<(String, Value, HeaderMap)>,
    messages: Vec<Value>,
    busy: bool,
    encryption: bool,
    intruder: bool,
    fail_send: bool,
    fail_after_commit: bool,
    fail_matrix_send: bool,
    fail_create: bool,
    created_room: Option<Value>,
    power_levels: Value,
    rakazo_nonces: HashSet<String>,
    matrix_transactions: HashMap<String, String>,
    incoming_events: HashMap<String, Value>,
    artifacts: HashMap<String, Value>,
}
type Shared = Arc<Mutex<Remote>>;

async fn mock_media(State(remote): State<Shared>, uri: Uri, headers: HeaderMap) -> Response {
    assert_eq!(
        headers["authorization"],
        "Bearer appservice-token-at-least-20-chars"
    );
    assert!(uri.query().unwrap().contains("allow_redirect=false"));
    remote
        .lock()
        .unwrap()
        .requests
        .push((uri.to_string(), Value::Null, headers));
    if uri.path().ends_with("/redirect") {
        return Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("location", "/media-cdn/fixture")
            .body(Body::empty())
            .unwrap();
    }
    if uri.path().ends_with("/untrusted") {
        return Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("location", "https://127.0.0.1/private")
            .body(Body::empty())
            .unwrap();
    }
    (StatusCode::OK, b"attachment fixture".to_vec()).into_response()
}

async fn mock_media_cdn(headers: HeaderMap) -> Vec<u8> {
    assert!(
        !headers.contains_key("authorization"),
        "CDN request must not receive appservice credentials"
    );
    assert!(!headers.contains_key("cookie"));
    b"attachment fixture".to_vec()
}

async fn mock(
    State(remote): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let input: Value = if uri.path().contains("/media/v3/upload") {
        if headers["content-type"] == "image/png" {
            assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));
        }
        json!({"bytes":body.len(),"digest":hex::encode(sha2::Sha256::digest(&body))})
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
                if let Some(id) = args["around"]["messageId"].as_str() {
                    return ok(
                        json!({"json":{"messages":state.messages.iter().filter(|message|message["id"]==id).collect::<Vec<_>>(),"olderCursor":null}}),
                    );
                }
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
                    let seq=state.messages.iter().find(|message|message["clientNonce"]==args["clientNonce"]).and_then(|message|message["seq"].as_i64()).unwrap_or_else(|| {
                        let seq=state.messages.iter().filter_map(|message|message["seq"].as_i64()).max().unwrap_or(-1)+1;
                        let mut row=message(seq,"user",args["text"].as_str().unwrap_or(""));
                        if let Some(ids)=args["artifactIds"].as_array() {
                            let mut blocks=if args["text"].as_str().is_some_and(|text|!text.is_empty()) {row["blocks"].as_array().unwrap().clone()} else {vec![]};
                            for id in ids {let artifact=state.artifacts.get(id.as_str().unwrap()).unwrap();blocks.push(json!({"kind":if artifact["mimeType"].as_str().unwrap().starts_with("image/") {"image"} else {"file"},"artifactId":id,"mimeType":artifact["mimeType"],"name":artifact["name"],"size":artifact["size"]}));}
                            row["blocks"]=json!(blocks);
                        }
                        row["clientNonce"]=args["clientNonce"].clone();
                        if let Some(reply)=args.get("replyToMessageId") {row["replyToMessageId"]=reply.clone();}
                        state.messages.push(row);seq
                    });
                    if std::mem::take(&mut state.fail_after_commit) {
                        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
                    } else {
                        ok(json!({"json":{"taskId":"task-a","runId":"run-a","seq":seq}}))
                    }
                }
            }
            "/rpc/artifacts/create" => {
                let id = format!("artifact-{}", args["clientNonce"].as_str().unwrap());
                let bytes = STANDARD
                    .decode(args["contentBase64"].as_str().unwrap())
                    .unwrap();
                let artifact = json!({"id":id,"name":args["name"],"mimeType":args["mimeType"],"contentBase64":args["contentBase64"],"size":bytes.len()});
                state.artifacts.insert(id.clone(), artifact.clone());
                ok(json!({"json":artifact}))
            }
            "/rpc/artifacts/get" => {
                if let Some(artifact) = state.artifacts.get(args["artifactId"].as_str().unwrap()) {
                    ok(json!({"json":artifact}))
                } else {
                    (StatusCode::NOT_FOUND, Json(json!({})))
                }
            }
            "/rpc/threads/react" => {
                if !state
                    .messages
                    .iter()
                    .any(|message| message["clientNonce"] == args["clientNonce"])
                {
                    let seq = state
                        .messages
                        .iter()
                        .filter_map(|message| message["seq"].as_i64())
                        .max()
                        .unwrap_or(-1)
                        + 1;
                    let mut row = message(seq, "user", args["reaction"].as_str().unwrap());
                    row["clientNonce"] = args["clientNonce"].clone();
                    row["replyToMessageId"] = args["messageId"].clone();
                    state.messages.push(row);
                }
                ok(json!({"json":{"ok":true}}))
            }
            _ => panic!("unexpected RPC route {path}"),
        };
    }
    assert_eq!(
        headers["authorization"],
        "Bearer appservice-token-at-least-20-chars"
    );
    if path.ends_with("/batch_send") {
        assert!(path.contains("/unstable/com.beeper.backfill/rooms/"));
        assert_eq!(input["send_notification"], input["forward"]);
        let events = input["events"].as_array().unwrap();
        let ids: Vec<_> = events
            .iter()
            .map(|event| event["event_id"].clone())
            .collect();
        let fresh = events
            .iter()
            .filter_map(|event| {
                let id = event["event_id"].as_str().unwrap();
                (!state.history_events.contains_key(id)).then(|| id.to_string())
            })
            .collect::<Vec<_>>();
        if input["forward"] == true {
            state.timeline.extend(fresh);
        } else {
            state.timeline.splice(0..0, fresh);
        }
        for event in events {
            if input["forward"] == true
                && event["type"] == "m.room.message"
                && event["sender"]
                    .as_str()
                    .is_some_and(|sender| sender.starts_with("@sh-rakazo_"))
            {
                state.matrix_transactions.insert(
                    event["event_id"].as_str().unwrap().to_string(),
                    event["content"]["body"].as_str().unwrap().to_string(),
                );
            }
            state.history_events.insert(
                event["event_id"].as_str().unwrap().to_string(),
                event.clone(),
            );
        }
        // Simulate a committed request whose successful response was lost.
        if std::mem::take(&mut state.fail_backfill)
            || (input["forward"] == true && std::mem::take(&mut state.fail_matrix_send))
        {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        }
        return ok(json!({"event_ids":ids}));
    }
    if path.ends_with("/messages") {
        return ok(json!({"chunk":state.history_events.values().collect::<Vec<_>>() }));
    }
    if path.contains("/event/") {
        let id = path.rsplit('/').next().unwrap();
        let id = percent_encoding::percent_decode_str(id)
            .decode_utf8()
            .unwrap();
        return ok(state
            .incoming_events
            .get(id.as_ref())
            .or_else(|| state.history_events.get(id.as_ref()))
            .cloned()
            .unwrap_or_else(|| event(&id, "@owner:beeper.com")));
    }
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
        || path.contains("/state/m.bridge/")
        || path.contains("/state/uk.half-shot.bridge/")
        || path.contains("/state/com.beeper.room_features/")
    {
        if std::mem::take(&mut state.fail_profile) {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        }
        return ok(json!({}));
    }
    if path.contains("/state/m.room.power_levels/") {
        if method == Method::PUT {
            state.power_levels = input;
        }
        return ok(state.power_levels.clone());
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
        state.power_levels = input["power_level_content_override"].clone();
        assert_eq!(state.power_levels["events"]["m.reaction"], 0);
        assert_eq!(state.power_levels["events"]["m.sticker"], 0);
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
    let app = Router::new()
        .route(
            "/_matrix/client/v1/media/download/{server}/{id}",
            get(mock_media),
        )
        .route("/media-cdn/fixture", get(mock_media_cdn))
        .fallback(any(mock))
        .with_state(remote.clone());
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
    json!({"id":format!("message-{seq}"),"seq":seq,"role":role,"createdAt":"2026-10-18T00:16:24.962Z","blocks":[{"kind":"text","text":text}]})
}
fn event(id: &str, sender: &str) -> Value {
    json!({"event_id":id,"room_id":"!agent-a:beeper.local","sender":sender,"type":"m.room.message","content":{"msgtype":"m.text","body":"Hello Ada"}})
}

#[test]
fn room_capabilities_follow_server_limits_and_do_not_advertise_edits_or_removals() {
    let preferences = rakazo_beeper::rakazo::Preferences {
        avatar_style: "organic".into(),
        max_upload_size_bytes: Some(1024),
        attachment_mime_types: Some(vec!["audio/ogg".into(), "image/png".into()]),
    };
    let features = rakazo_beeper::rich::room_features(&preferences);
    assert_eq!(features["edit"], -2);
    assert_eq!(features["delete"], -2);
    assert_eq!(features["file"]["m.audio"]["mime_types"]["audio/ogg"], 2);
    assert_eq!(features["file"]["m.image"]["max_size"], 1024);
    assert!(features["file"]["m.video"].is_null());
    assert_eq!(features["allowed_reactions"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn unavailable_source_attachments_are_visible_and_do_not_block_later_replies() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    let mut missing = message(0, "bot", "");
    missing["blocks"] = json!([{"kind":"file","artifactId":"missing","name":"missing.txt","mimeType":"text/plain","size":18}]);
    f.remote.lock().unwrap().messages = vec![missing, message(1, "bot", "later reply")];
    f.runtime.poll_binding(&binding).await.unwrap();
    let remote = f.remote.lock().unwrap();
    assert!(
        remote
            .history_events
            .values()
            .any(|event| event["content"]["msgtype"] == "m.notice")
    );
    assert!(
        remote
            .history_events
            .values()
            .any(|event| event["content"]["body"] == "later reply")
    );
    assert_eq!(f.runtime.store.bindings("tenant-a").unwrap()[0].cursor, 1);
}

#[tokio::test]
async fn threaded_agent_reply_waits_for_an_ambiguous_send_receipt_and_keeps_its_thread() {
    let f = fixture().await;
    f.remote.lock().unwrap().messages = vec![message(0, "bot", "thread root")];
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.runtime.backfill_binding(&binding).await.unwrap();
    let root = f.runtime.store.links(&binding, "message-0").unwrap()[0]
        .matrix_id
        .clone();
    let raw = json!({"event_id":"$thread-input","room_id":binding.room,"type":"m.room.message","sender":"@owner:beeper.com","content":{"msgtype":"m.text","body":"threaded input","m.relates_to":{"rel_type":"m.thread","event_id":root,"m.in_reply_to":{"event_id":root}}}});
    f.remote
        .lock()
        .unwrap()
        .incoming_events
        .insert("$thread-input".into(), raw.clone());
    f.runtime
        .ingest("thread-txn", &json!({"events":[raw]}))
        .unwrap();
    f.remote.lock().unwrap().fail_after_commit = true;
    assert!(f.runtime.deliver_inbound().await.is_err());
    let mut reply = message(2, "bot", "threaded output");
    reply["runId"] = json!("run-a");
    f.remote.lock().unwrap().messages.push(reply);
    f.runtime.poll_binding(&binding).await.unwrap();
    assert!(f.remote.lock().unwrap().matrix_transactions.is_empty());
    let pending = f.runtime.store.next("tenant-a", i64::MAX).unwrap().unwrap();
    let seq = f
        .runtime
        .inbound_content(&binding, &pending.event)
        .await
        .unwrap()
        .unwrap();
    f.runtime
        .store
        .complete_send(&binding, pending.id, "$thread-input", seq)
        .unwrap();
    f.runtime.poll_binding(&binding).await.unwrap();
    let remote = f.remote.lock().unwrap();
    assert_eq!(remote.rakazo_nonces.len(), 1);
    let reply = remote
        .history_events
        .values()
        .find(|event| event["content"]["body"] == "threaded output")
        .unwrap();
    assert_eq!(reply["content"]["m.relates_to"]["rel_type"], "m.thread");
    assert_eq!(reply["content"]["m.relates_to"]["event_id"], root);
    assert_eq!(
        reply["content"]["m.relates_to"]["m.in_reply_to"]["event_id"],
        "$thread-input"
    );
}

#[tokio::test]
async fn authenticated_media_redirects_strip_credentials_and_reject_untrusted_targets() {
    let f = fixture().await;
    let content = json!({"msgtype":"m.file","body":"notes.txt","url":"mxc://media.example.test/redirect","info":{"mimetype":"text/plain","size":18}});
    let media = f
        .runtime
        .matrix
        .download_media(&f.runtime.matrix.sender, &content, 1024)
        .await
        .unwrap();
    assert_eq!(media.bytes, b"attachment fixture");
    let mut untrusted = content;
    untrusted["url"] = json!("mxc://media.example.test/untrusted");
    let error = f
        .runtime
        .matrix
        .download_media(&f.runtime.matrix.sender, &untrusted, 1024)
        .await
        .err()
        .unwrap();
    assert!(
        error
            .downcast_ref::<rakazo_beeper::media::Rejected>()
            .is_some()
    );
}

#[tokio::test]
async fn attachments_upload_once_and_survive_a_lost_send_response_without_echoes() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    let raw = json!({"event_id":"$file","room_id":binding.room,"sender":"@owner:beeper.com","type":"m.room.message","content":{"msgtype":"m.file","filename":"notes.txt","body":"A caption","url":"mxc://media.example.test/fixture","info":{"mimetype":"text/plain","size":18}}});
    f.remote
        .lock()
        .unwrap()
        .incoming_events
        .insert("$file".into(), raw.clone());
    f.runtime
        .ingest("file-txn", &json!({"events":[raw.clone()]}))
        .unwrap();
    f.remote.lock().unwrap().fail_send = true;
    assert!(f.runtime.deliver_inbound().await.is_err());
    let pending = f.runtime.store.next("tenant-a", i64::MAX).unwrap().unwrap();
    let seq = f
        .runtime
        .inbound_content(&binding, &pending.event)
        .await
        .unwrap()
        .unwrap();
    f.runtime
        .store
        .complete_send(&binding, pending.id, "$file", seq)
        .unwrap();
    f.runtime
        .ingest("file-txn-retry", &json!({"events":[raw]}))
        .unwrap();
    f.runtime.poll_binding(&binding).await.unwrap();
    let remote = f.remote.lock().unwrap();
    assert_eq!(remote.artifacts.len(), 1);
    assert_eq!(
        remote.artifacts.values().next().unwrap()["contentBase64"],
        STANDARD.encode(b"attachment fixture")
    );
    assert_eq!(remote.rakazo_nonces.len(), 1);
    assert_eq!(remote.messages.len(), 1);
    assert_eq!(remote.messages[0]["blocks"][0]["text"], "A caption");
    assert_eq!(remote.messages[0]["blocks"][1]["kind"], "file");
    assert!(
        remote.history_events.is_empty(),
        "source echo must use the original Beeper attachment"
    );
    assert_eq!(f.runtime.store.pending_count().unwrap(), 0);
    assert_eq!(
        f.runtime.store.links(&binding, "message-0").unwrap()[0].matrix_id,
        "$file"
    );
}

#[tokio::test]
async fn source_media_preserves_types_and_replies_without_duplicate_uploads() {
    let f = fixture().await;
    f.remote.lock().unwrap().messages = vec![message(0, "bot", "parent")];
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.runtime.backfill_binding(&binding).await.unwrap();
    let parent = f.runtime.store.links(&binding, "message-0").unwrap()[0]
        .matrix_id
        .clone();
    let types = [
        ("image/jpeg", "m.image"),
        ("video/mp4", "m.video"),
        ("audio/ogg", "m.audio"),
        ("application/pdf", "m.file"),
    ];
    for (index, (mime, _)) in types.iter().enumerate() {
        let id = format!("file-{index}");
        let mut message = message(index as i64 + 1, "bot", "");
        message["replyToMessageId"] = json!("message-0");
        message["blocks"] =
            json!([{"kind":"file","artifactId":id,"name":"fixture.bin","mimeType":mime,"size":18}]);
        let mut remote = f.remote.lock().unwrap();
        remote.artifacts.insert(id,json!({"name":"fixture.bin","mimeType":mime,"size":18,"contentBase64":STANDARD.encode(b"attachment fixture")}));
        remote.messages.push(message);
    }
    f.runtime.poll_binding(&binding).await.unwrap();
    let upload_count = || {
        f.remote
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(path, _, _)| path.contains("/media/v3/upload"))
            .count()
    };
    assert_eq!(upload_count(), 6); // Bridge logo, agent picture, four attachments.
    f.runtime.poll_binding(&binding).await.unwrap();
    assert_eq!(upload_count(), 6);
    let remote = f.remote.lock().unwrap();
    for (_, expected_type) in types {
        let event = remote
            .history_events
            .values()
            .find(|event| event["content"]["msgtype"] == expected_type)
            .unwrap();
        assert_eq!(
            event["content"]["m.relates_to"]["m.in_reply_to"]["event_id"],
            parent
        );
        assert!(
            event["content"]["url"]
                .as_str()
                .unwrap()
                .starts_with("mxc://")
        );
    }
    for (_, body, _) in remote
        .requests
        .iter()
        .filter(|(path, body, _)| path.contains("/media/v3/upload") && body["bytes"] == 18)
    {
        assert_eq!(
            body["digest"],
            hex::encode(sha2::Sha256::digest(b"attachment fixture"))
        );
    }
}

#[tokio::test]
async fn replies_and_tapbacks_reference_the_original_message_and_do_not_start_runs() {
    let f = fixture().await;
    f.remote.lock().unwrap().messages = vec![message(0, "bot", "parent")];
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.runtime.backfill_binding(&binding).await.unwrap();
    let parent = f.runtime.store.links(&binding, "message-0").unwrap()[0]
        .matrix_id
        .clone();
    let reaction = json!({"event_id":"$reaction","room_id":binding.room,"sender":"@owner:beeper.com","type":"m.reaction","content":{"m.relates_to":{"rel_type":"m.annotation","event_id":parent,"key":"❤️"}}});
    f.remote
        .lock()
        .unwrap()
        .incoming_events
        .insert("$reaction".into(), reaction.clone());
    f.runtime
        .ingest("reaction-txn", &json!({"events":[reaction]}))
        .unwrap();
    f.runtime.deliver_inbound().await.unwrap();
    f.runtime.poll_binding(&binding).await.unwrap();
    {
        let mut remote = f.remote.lock().unwrap();
        assert!(
            remote.rakazo_nonces.is_empty(),
            "react must never invoke threads/send"
        );
        let mut reply = message(2, "user", "👍");
        reply["replyToMessageId"] = json!("message-0");
        remote.messages.push(reply);
    }
    f.runtime.poll_binding(&binding).await.unwrap();
    {
        let remote = f.remote.lock().unwrap();
        let reaction = remote
            .history_events
            .values()
            .find(|event| event["type"] == "m.reaction")
            .unwrap();
        assert_eq!(reaction["content"]["m.relates_to"]["event_id"], parent);
    }
    let reply = json!({"event_id":"$reply","room_id":binding.room,"sender":"@owner:beeper.com","type":"m.room.message","content":{"msgtype":"m.text","body":"> <@agent:example.test> parent\n\nReal reply","m.relates_to":{"m.in_reply_to":{"event_id":parent}}}});
    f.runtime
        .ingest("reply-txn", &json!({"events":[reply]}))
        .unwrap();
    f.runtime.deliver_inbound().await.unwrap();
    let remote = f.remote.lock().unwrap();
    let (_, body, _) = remote
        .requests
        .iter()
        .rev()
        .find(|(path, _, _)| path == "/rpc/threads/send")
        .unwrap();
    assert_eq!(body["json"]["text"], "Real reply");
    assert_eq!(body["json"]["replyToMessageId"], "message-0");
}

#[tokio::test]
async fn invalid_media_and_cross_room_targets_are_rejected_without_blocking_the_queue() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let raw = json!({"event_id":"$bad-file","room_id":"!agent-a:beeper.local","sender":"@owner:beeper.com","type":"m.room.message","content":{"msgtype":"m.file","body":"notes.txt","url":"https://untrusted.example.test/steal","info":{"mimetype":"text/plain"}}});
    let cross = json!({"event_id":"$cross-reply","room_id":"!agent-a:beeper.local","sender":"@owner:beeper.com","type":"m.room.message","content":{"msgtype":"m.text","body":"hello","m.relates_to":{"m.in_reply_to":{"event_id":"$different-room"}}}});
    f.runtime
        .ingest(
            "bad-txn",
            &json!({"events":[raw,cross,event("$valid","@owner:beeper.com")]}),
        )
        .unwrap();
    f.runtime.deliver_inbound().await.unwrap();
    let remote = f.remote.lock().unwrap();
    assert_eq!(remote.rakazo_nonces.len(), 1);
    assert_eq!(f.runtime.store.pending_count().unwrap(), 0);
    assert!(
        !remote
            .requests
            .iter()
            .any(|(path, _, _)| path.contains("/media/download/"))
    );
    assert_eq!(
        remote
            .requests
            .iter()
            .filter(|(path, body, _)| path.contains("/send/m.room.message/")
                && body["msgtype"] == "m.notice")
            .count(),
        2
    );
}

#[tokio::test]
async fn upgrade_recovers_old_live_ids_and_imports_only_previously_omitted_media() {
    let f = fixture().await;
    let mut old = message(0, "bot", "old text");
    old["blocks"].as_array_mut().unwrap().push(json!({"kind":"file","artifactId":"old-file","name":"notes.txt","mimeType":"text/plain","size":18}));
    f.remote.lock().unwrap().messages = vec![old.clone()];
    f.runtime.discover().await.unwrap();
    let mut binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.runtime.store.history_before(&binding).unwrap();
    f.runtime.store.history_progress(&binding, None).unwrap();
    let history_id =
        f.runtime
            .source_event_id(&binding, &serde_json::from_value(old).unwrap(), "text:0", 0);
    {
        let mut remote = f.remote.lock().unwrap();
        remote.messages.push(message(1, "bot", "old live reply"));
        remote.artifacts.insert("old-file".into(),json!({"name":"notes.txt","mimeType":"text/plain","size":18,"contentBase64":STANDARD.encode(b"attachment fixture")}));
        remote.history_events.insert(history_id.clone(),json!({"event_id":history_id,"room_id":binding.room,"sender":binding.ghost,"type":"m.room.message","content":{"msgtype":"m.text","body":"old text"}}));
        remote.history_events.insert("$legacy-live".into(),json!({"event_id":"$legacy-live","room_id":binding.room,"sender":binding.ghost,"type":"m.room.message","content":{"msgtype":"m.text","body":"old live reply"},"unsigned":{"transaction_id":stable_id(&["tenant-a","agent-a","message-1","0"])}}));
    }
    f.runtime.store.cursor(&binding, 1).unwrap();
    binding.cursor = 1;
    f.runtime.backfill_binding(&binding).await.unwrap();
    f.runtime.backfill_binding(&binding).await.unwrap();
    assert_eq!(
        f.runtime.store.links(&binding, "message-1").unwrap()[0].matrix_id,
        "$legacy-live"
    );
    assert_eq!(
        f.runtime.store.links(&binding, "message-0").unwrap().len(),
        2
    );
    let remote = f.remote.lock().unwrap();
    assert_eq!(remote.history_events.len(), 3);
    assert_eq!(
        remote
            .requests
            .iter()
            .filter(|(path, _, _)| path.contains("/media/v3/upload"))
            .count(),
        3
    );
    assert!(
        remote
            .history_events
            .values()
            .any(|event| event["content"]["body"] == "notes.txt")
    );
}

#[tokio::test]
async fn missing_legacy_live_events_pause_upgrade_instead_of_resurrecting_messages() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let mut binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.remote
        .lock()
        .unwrap()
        .messages
        .push(message(0, "bot", "possibly removed"));
    f.runtime.store.cursor(&binding, 0).unwrap();
    binding.cursor = 0;
    assert!(f.runtime.bootstrap_links(&binding).await.is_err());
    assert!(!f.runtime.store.links_initialized(&binding).unwrap());
    assert!(f.remote.lock().unwrap().history_events.is_empty());
}

#[test]
fn rich_queue_keys_receipts_links_and_media_cache_survive_restart_without_cross_tenant_access() {
    use rakazo_beeper::store::{Binding, MessageLink};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bridge.db");
    let store = Store::open(&path, &key()).unwrap();
    let binding = Binding {
        tenant: "a".into(),
        bot: "agent".into(),
        room: "!room".into(),
        ghost: "@ghost".into(),
        cursor: 0,
    };
    store
        .ingest(
            "a",
            "txn",
            &[Inbound {
                event_id: "$file".into(),
                room: "!room".into(),
                text: "private caption".into(),
                content: json!({"file":{"key":{"k":"PRIVATE_ATTACHMENT_KEY"}}}),
            }],
        )
        .unwrap();
    store
        .link(
            &binding,
            &MessageLink {
                rakazo_id: "source-id".into(),
                matrix_id: "$file".into(),
                part: "text:0".into(),
                role: "user".into(),
                kind: "m.room.message".into(),
                hash: "hash".into(),
            },
        )
        .unwrap();
    store
        .cache_media(&binding, "artifact-id", "mxc://media.example.test/id")
        .unwrap();
    let nonce = stable_id(&["a", "$file"]);
    store
        .save_run_route(&binding, "run", "$file", Some("$root"))
        .unwrap();
    drop(store);
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes
            .windows(b"PRIVATE_ATTACHMENT_KEY".len())
            .any(|window| window == b"PRIVATE_ATTACHMENT_KEY")
    );
    let store = Store::open(&path, &key()).unwrap();
    assert_eq!(
        store.next("a", 0).unwrap().unwrap().event.content["file"]["key"]["k"],
        "PRIVATE_ATTACHMENT_KEY"
    );
    assert_eq!(
        store.event_for_nonce(&binding, &nonce).unwrap().as_deref(),
        Some("$file")
    );
    assert_eq!(
        store
            .cached_media(&binding, "artifact-id")
            .unwrap()
            .as_deref(),
        Some("mxc://media.example.test/id")
    );
    let other = Binding {
        tenant: "b".into(),
        ..binding.clone()
    };
    assert!(store.matrix_link(&other, "$file").unwrap().is_none());
    assert!(store.event_for_nonce(&other, &nonce).unwrap().is_none());
    assert_eq!(
        store.run_route(&binding, "run").unwrap(),
        Some(("$file".into(), Some("$root".into())))
    );
    assert!(store.run_route(&other, "run").unwrap().is_none());
    assert!(store.cached_media(&other, "artifact-id").unwrap().is_none());
    let pending = store.next("a", 0).unwrap().unwrap();
    store
        .complete_send(&binding, pending.id, "$file", 7)
        .unwrap();
    drop(store);
    let store = Store::open(&path, &key()).unwrap();
    assert_eq!(store.receipt(&binding, "$file").unwrap(), Some(7));
    assert_eq!(store.pending_count().unwrap(), 0);
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
    assert_eq!(uploads(), 2); // One bridge logo and one agent picture.
    let profile = f
        .runtime
        .store
        .profile("tenant-a", "agent-a")
        .unwrap()
        .unwrap();
    assert_eq!(profile.avatar_uri, "mxc://beeper.local/avatar-2");
    f.runtime.discover().await.unwrap();
    assert_eq!(uploads(), 2);
    {
        let mut remote = f.remote.lock().unwrap();
        remote.bot_name = Some("Ada Lovelace".into());
        remote.fail_profile = true;
    }
    assert!(f.runtime.discover().await.is_err());
    f.runtime.discover().await.unwrap();
    assert_eq!(
        uploads(),
        2,
        "renames and profile retries reuse uploaded media"
    );
    f.remote.lock().unwrap().avatar_color = Some("#D9508A::shape_3".into());
    f.runtime.discover().await.unwrap();
    assert_eq!(uploads(), 3);
    let state = f.remote.lock().unwrap();
    for event_type in ["m.bridge", "uk.half-shot.bridge"] {
        let (_, content, _) = state
            .requests
            .iter()
            .rev()
            .find(|(path, _, _)| path.contains(&format!("/state/{event_type}/")))
            .unwrap();
        assert_eq!(
            content["protocol"]["avatar_url"],
            "mxc://beeper.local/avatar-1"
        );
        assert_eq!(content["protocol"]["displayname"], "FPL Agents");
        assert_eq!(
            content["channel"]["avatar_url"],
            "mxc://beeper.local/avatar-3"
        );
        assert_eq!(content["bridgebot"], "@sh-rakazobot:beeper.local");
    }
    assert!(
        state
            .requests
            .iter()
            .any(|(path, body, _)| path.contains("/state/m.room.avatar/")
                && body["url"] == "mxc://beeper.local/avatar-3")
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
    assert_eq!(binding.cursor, 0); // Live polling has a separate cursor from historical import.
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
        remote.messages.push(message(2, "bot", "Hello Avery"));
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
async fn historical_backfill_preserves_senders_dates_and_frozen_boundary_without_echoes() {
    let f = fixture().await;
    f.remote.lock().unwrap().messages = (0..5)
        .map(|seq| {
            message(
                seq,
                if seq % 2 == 0 { "bot" } else { "user" },
                &format!("old {seq}"),
            )
        })
        .collect();
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.remote
        .lock()
        .unwrap()
        .messages
        .push(message(5, "bot", "new reply"));
    f.remote.lock().unwrap().fail_backfill = true;
    assert!(f.runtime.backfill_binding(&binding).await.is_err());
    assert_eq!(f.runtime.store.history_before(&binding).unwrap(), Some(5));
    let echo = f
        .remote
        .lock()
        .unwrap()
        .history_events
        .values()
        .find(|event| event["sender"] == "@owner:beeper.com")
        .unwrap()
        .clone();
    f.runtime
        .ingest("early-history-echo", &json!({"events":[echo]}))
        .unwrap();
    assert_eq!(f.runtime.store.pending_count().unwrap(), 0);
    f.runtime.backfill_binding(&binding).await.unwrap();
    assert_eq!(f.runtime.store.history_before(&binding).unwrap(), Some(3));
    f.runtime.backfill_binding(&binding).await.unwrap();
    f.runtime.backfill_binding(&binding).await.unwrap();
    assert_eq!(f.runtime.store.history_before(&binding).unwrap(), None);
    f.runtime.backfill_binding(&binding).await.unwrap();
    {
        let remote = f.remote.lock().unwrap();
        assert_eq!(remote.history_events.len(), 5);
        let bodies = remote
            .timeline
            .iter()
            .map(|id| {
                remote.history_events[id]["content"]["body"]
                    .as_str()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(bodies, ["old 0", "old 1", "old 2", "old 3", "old 4"]);

        for event in remote.history_events.values() {
            assert_eq!(event["origin_server_ts"], 1792282584962_i64);
            let seq: i64 = event["content"]["body"]
                .as_str()
                .unwrap()
                .strip_prefix("old ")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                event["sender"],
                if seq % 2 == 0 {
                    binding.ghost.as_str()
                } else {
                    "@owner:beeper.com"
                }
            );
        }
        assert!(remote.rakazo_nonces.is_empty());
    }
    f.runtime.poll_binding(&binding).await.unwrap();
    assert_eq!(f.remote.lock().unwrap().matrix_transactions.len(), 1);
}

#[tokio::test]
async fn history_import_fails_closed_for_shared_rooms_and_invalid_dates() {
    let f = fixture().await;
    f.remote.lock().unwrap().messages = vec![message(0, "user", "private history")];
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.remote.lock().unwrap().intruder = true;
    assert!(f.runtime.backfill_binding(&binding).await.is_err());
    assert!(f.remote.lock().unwrap().history_events.is_empty());
    f.remote.lock().unwrap().intruder = false;
    f.remote.lock().unwrap().messages[0]["createdAt"] = json!("bad-date");
    assert!(f.runtime.backfill_binding(&binding).await.is_err());
    assert_eq!(f.runtime.store.history_before(&binding).unwrap(), Some(1));
    assert!(f.remote.lock().unwrap().history_events.is_empty());
}

#[test]
fn history_progress_and_echo_reservations_survive_restart_and_isolate_tenants() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bridge.db");
    let binding = rakazo_beeper::store::Binding {
        tenant: "a".into(),
        bot: "bot".into(),
        room: "!room".into(),
        ghost: "@ghost".into(),
        cursor: 10,
    };
    let store = Store::open(&path, &key()).unwrap();
    assert_eq!(store.history_before(&binding).unwrap(), Some(11));
    store.reserve_history("a", &["$history".into()]).unwrap();
    store.history_progress(&binding, Some(5)).unwrap();
    drop(store);
    let store = Store::open(&path, &key()).unwrap();
    assert_eq!(store.history_before(&binding).unwrap(), Some(5));
    let echo = Inbound {
        event_id: "$history".into(),
        room: "!room".into(),
        text: "old prompt".into(),
        content: Value::Null,
    };
    store
        .ingest("a", "echo", std::slice::from_ref(&echo))
        .unwrap();
    assert_eq!(store.pending_count().unwrap(), 0);
    store.ingest("b", "echo", &[echo]).unwrap();
    assert_eq!(store.pending_count().unwrap(), 1);
    store.history_progress(&binding, None).unwrap();
    drop(store);
    let store = Store::open(&path, &key()).unwrap();
    assert_eq!(store.history_before(&binding).unwrap(), None);
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
        .filter(|(p, v, _)| p.contains("/batch_send") && v["forward"] == true)
        .flat_map(|(_, v, _)| v["events"].as_array().unwrap().iter())
        .map(|event| event["content"]["body"].as_str().unwrap())
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
    f.runtime.ingest("txn",&json!({"events":[event("$stranger","@stranger:beeper.com"),event("$echo",&ghost),unknown]})).unwrap();
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
        content: Value::Null,
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
                    content: Value::Null,
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

#[tokio::test]
async fn rich_permissions_upgrade_legacy_rooms_without_relaxing_state_or_membership() {
    let f = fixture().await;
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    let before = {
        let mut remote = f.remote.lock().unwrap();
        remote.power_levels["events"]
            .as_object_mut()
            .unwrap()
            .remove("m.reaction");
        remote.power_levels["events"]
            .as_object_mut()
            .unwrap()
            .remove("m.sticker");
        remote.power_levels.clone()
    };
    f.runtime.matrix.rich_permissions(&binding).await.unwrap();
    let mut expected = before;
    expected["events"]["m.reaction"] = json!(0);
    expected["events"]["m.sticker"] = json!(0);
    assert_eq!(f.remote.lock().unwrap().power_levels, expected);
    let requests = f.remote.lock().unwrap().requests.len();
    f.runtime.matrix.rich_permissions(&binding).await.unwrap();
    let remote = f.remote.lock().unwrap();
    assert_eq!(
        remote.requests.len(),
        requests + 1,
        "unchanged permissions need only a GET"
    );
}

#[tokio::test]
async fn malformed_matrix_media_uris_are_permanent_rejections_without_network_requests() {
    let f = fixture().await;
    for uri in [
        "mxc:///missing-server",
        "mxc://beeper.local/",
        "mxc://beeper.local/one/two",
    ] {
        let content = json!({"msgtype":"m.file","filename":"fixture.txt","url":uri,"info":{"mimetype":"text/plain"}});
        let error = f
            .runtime
            .matrix
            .download_media("@owner:beeper.com", &content, 1024)
            .await
            .err()
            .unwrap();
        assert!(
            error
                .downcast_ref::<rakazo_beeper::media::Rejected>()
                .is_some(),
            "malformed media must not retry indefinitely: {uri}"
        );
    }
    assert!(f.remote.lock().unwrap().requests.is_empty());
}

#[tokio::test]
async fn replies_to_unrendered_messages_do_not_stall_later_delivery() {
    let f = fixture().await;
    let mut parent = message(0, "bot", "");
    parent["blocks"] = json!([{"kind":"tool-call","name":"fixture"}]);
    f.remote.lock().unwrap().messages = vec![parent];
    f.runtime.discover().await.unwrap();
    let binding = f.runtime.store.bindings("tenant-a").unwrap().remove(0);
    f.runtime.backfill_binding(&binding).await.unwrap();
    for (seq, role, text) in [(1, "bot", "reply to tool result"), (2, "user", "👍")] {
        let mut reply = message(seq, role, text);
        reply["replyToMessageId"] = json!("message-0");
        f.remote.lock().unwrap().messages.push(reply);
    }
    f.remote
        .lock()
        .unwrap()
        .messages
        .push(message(3, "bot", "later delivery"));
    f.runtime.poll_binding(&binding).await.unwrap();
    f.runtime.poll_binding(&binding).await.unwrap();
    let remote = f.remote.lock().unwrap();
    for text in ["reply to tool result", "👍", "later delivery"] {
        let events: Vec<_> = remote
            .history_events
            .values()
            .filter(|e| e["content"]["body"] == text)
            .collect();
        assert_eq!(events.len(), 1);
        assert!(events[0]["content"]["m.relates_to"].is_null());
    }
}
