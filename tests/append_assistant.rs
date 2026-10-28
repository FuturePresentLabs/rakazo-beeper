//! `threads.appendAssistant` must carry `replyToMessageId` only when the caller
//! asked to thread the answer under an inbound message.

use axum::{Json, Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use rakazo_beeper::rakazo::Rakazo;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use url::Url;

type Shared = Arc<Mutex<Vec<Value>>>;

async fn mock(State(seen): State<Shared>, body: Bytes) -> (StatusCode, Json<Value>) {
    let envelope: Value = serde_json::from_slice(&body).unwrap();
    seen.lock().unwrap().push(envelope["json"].clone());
    (
        StatusCode::OK,
        Json(json!({"json": {"messageId": "m-out", "seq": 9}})),
    )
}

async fn server() -> (String, Shared) {
    let seen: Shared = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/rpc/threads/appendAssistant", post(mock))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    (format!("http://{addr}"), seen)
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
async fn reply_to_is_sent_only_when_requested() {
    let (base, seen) = server().await;
    let rakazo = client(&base);

    rakazo
        .append_assistant("bot-1", "plain answer", "nonce-1", None)
        .await
        .unwrap();
    rakazo
        .append_assistant("bot-1", "threaded answer", "nonce-2", Some("inbound-7"))
        .await
        .unwrap();

    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0].get("replyToMessageId").is_none(),
        "absent by default"
    );
    assert_eq!(calls[1]["replyToMessageId"], json!("inbound-7"));
    assert_eq!(calls[1]["botId"], json!("bot-1"));
    assert_eq!(calls[1]["text"], json!("threaded answer"));
}
