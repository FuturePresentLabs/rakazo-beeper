//! Relay-only `threads.send` stores the message but starts no model run, so the
//! server's success receipt carries `runId: null`. Both receipt shapes must
//! parse, and only the run-starting shape may report a run.

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use rakazo_beeper::rakazo::Rakazo;
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

async fn mock(State(receipt): State<Value>) -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({"json": receipt})))
}

async fn server(receipt: Value) -> String {
    let app = Router::new()
        .route("/rpc/threads/send", post(mock))
        .with_state(receipt);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the listener a moment to accept on slower CI filesystems.
    tokio::time::sleep(Duration::from_millis(10)).await;
    format!("http://{addr}")
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
async fn relay_only_receipt_parses_and_reports_no_run() {
    // The live relay-only shape: no run starts, so every run field is null.
    let base = server(json!({
        "taskId": null,
        "runId": null,
        "runIds": null,
        "seq": 7
    }))
    .await;
    let receipt = client(&base)
        .send_routed("bot-1", "hello", &[], None, "nonce-1")
        .await
        .unwrap();
    assert_eq!(receipt.seq, 7);
    assert_eq!(receipt.run(), None, "relay-only must report no run");
    assert!(receipt.run_ids.is_empty());
}

#[tokio::test]
async fn run_starting_receipt_still_reports_its_run() {
    // The pre-relay-only shape, still valid for surfaces that start a run.
    let base = server(json!({
        "taskId": "task-1",
        "runId": "run-1",
        "runIds": ["run-1"],
        "seq": 4
    }))
    .await;
    let receipt = client(&base)
        .send_routed("bot-1", "hello", &[], None, "nonce-2")
        .await
        .unwrap();
    assert_eq!(receipt.seq, 4);
    assert_eq!(receipt.run(), Some("run-1"));
    assert_eq!(receipt.run_ids, ["run-1"]);
}
