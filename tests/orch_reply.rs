//! The interim reply surface must reach a messaging attachment, never
//! `threads.send` (which writes a user message and queues a model run).

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{StatusCode, Uri},
    routing::post,
};
use rakazo_beeper::{
    orch::reply::{self, Plan},
    rakazo::Rakazo,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use url::Url;

#[derive(Default)]
struct Remote {
    attachment_backed: bool,
    unauthorized: bool,
    calls: Vec<String>,
}

type Shared = Arc<Mutex<Remote>>;

async fn mock(State(remote): State<Shared>, uri: Uri, _body: Bytes) -> (StatusCode, Json<Value>) {
    let path = uri.path().to_string();
    let (attachment_backed, unauthorized) = {
        let mut remote = remote.lock().unwrap();
        remote.calls.push(path.clone());
        (remote.attachment_backed, remote.unauthorized)
    };
    if unauthorized {
        return (StatusCode::UNAUTHORIZED, Json(json!({"json": null})));
    }
    let body = match path.as_str() {
        "/rpc/me" => json!({"email": "owner@example.test", "spaceId": "space"}),
        "/rpc/bots/list" => json!([{
            "id": "bot-1",
            "spaceId": "space",
            "name": "Fleet Orchestrator",
            "threadId": "thread-1",
            "thread_id": "thread-1",
            "spawnKey": "fleet-orch-v1",
            "archivedAt": null,
            "capabilities": if attachment_backed {
                json!({"computer": false, "attachments": false, "voice": false, "skills": false})
            } else {
                Value::Null
            }
        }]),
        "/rpc/conversationAttachments/linked" => json!([{
            "id": "att-1",
            "bindingId": "binding-1",
            "label": "Fleet Orchestrator",
            "channel": "phone",
            "linkedAt": "2026-01-01T00:00:00Z"
        }]),
        "/rpc/conversationAttachments/directSend" => json!({
            "id": "send-1",
            "attachmentId": "att-1",
            "clientNonce": "nonce",
            "text": "reply",
            "status": "accepted",
            "createdAt": "2026-01-01T00:00:00Z"
        }),
        "/rpc/threads/send" => json!({
            "taskId": "task-1", "runId": "run-1", "seq": 1
        }),
        other => panic!("unexpected RPC {other}"),
    };
    (StatusCode::OK, Json(json!({"json": body})))
}

async fn server(attachment_backed: bool) -> (String, Shared) {
    server_with(Remote {
        attachment_backed,
        unauthorized: false,
        calls: Vec::new(),
    })
    .await
}

async fn server_with(remote: Remote) -> (String, Shared) {
    let remote = Arc::new(Mutex::new(remote));
    let app = Router::new()
        .route("/rpc/{*rest}", post(mock))
        .with_state(remote.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the listener a moment to accept on slower CI filesystems.
    tokio::time::sleep(Duration::from_millis(10)).await;
    (format!("http://{addr}"), remote)
}

fn client(base: &str) -> Rakazo {
    Rakazo::from_parts(
        Url::parse(base).unwrap(),
        "session-token-not-logged",
        "space",
    )
    .unwrap()
}

#[tokio::test]
async fn attachment_reply_posts_only_to_the_attachment_surface() {
    let (base, remote) = server(true).await;
    let rakazo = client(&base);
    let bots = rakazo.bots().await.unwrap();
    let plan = reply::reply(&rakazo, &bots[0], "status is green", "nonce-1")
        .await
        .unwrap();
    assert_eq!(
        plan,
        Plan::Attachment {
            attachment_id: "att-1".into(),
            label: "Fleet Orchestrator".into()
        }
    );
    let calls = remote.lock().unwrap().calls.clone();
    assert!(
        calls
            .iter()
            .any(|path| path == "/rpc/conversationAttachments/directSend"),
        "attachment POST missing: {calls:?}"
    );
    assert!(
        !calls.iter().any(|path| path == "/rpc/threads/send"),
        "reply must never call user-role threads.send: {calls:?}"
    );
}

#[tokio::test]
async fn normal_agent_reply_is_unsupported_and_sends_nothing() {
    let (base, remote) = server(false).await;
    let rakazo = client(&base);
    let bots = rakazo.bots().await.unwrap();
    let plan = reply::reply(&rakazo, &bots[0], "status is green", "nonce-1")
        .await
        .unwrap();
    assert_eq!(
        plan,
        Plan::Unsupported("unsupported: no assistant-append RPC".into())
    );
    let calls = remote.lock().unwrap().calls.clone();
    assert!(
        !calls.iter().any(|path| path == "/rpc/threads/send"),
        "unsupported reply must send nothing: {calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|path| path == "/rpc/conversationAttachments/directSend"),
        "unsupported reply must not guess an attachment: {calls:?}"
    );
}

#[tokio::test]
async fn a_401_is_a_typed_unauthorized_error_not_a_healthy_session() {
    use rakazo_beeper::rakazo::Unauthorized;
    use serde_json::Value;

    let (base, _remote) = server_with(Remote {
        attachment_backed: false,
        unauthorized: true,
        calls: Vec::new(),
    })
    .await;
    let rakazo = client(&base);
    let error = rakazo.rpc::<Value>(&["me"], Value::Null).await.unwrap_err();
    assert!(
        error.downcast_ref::<Unauthorized>().is_some(),
        "401 must be distinguishable for status: {error:#}"
    );
}
