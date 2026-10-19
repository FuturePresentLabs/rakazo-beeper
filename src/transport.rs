use crate::service::{Runtime, TenantRuntime, now};

#[derive(Debug)]
pub struct ConnectionReplaced;
impl std::fmt::Display for ConnectionReplaced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Beeper connection replaced by another instance; stop the competing bridge")
    }
}
impl std::error::Error for ConnectionReplaced {}
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, put},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

pub fn router(runtime: Arc<Runtime>) -> Router {
    Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"alive"})) }),
        )
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics))
        .route(
            "/tenants/{tenant}/_matrix/app/v1/transactions/{txn}",
            put(transaction),
        )
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(runtime)
}
async fn ready(State(runtime): State<Arc<Runtime>>) -> impl IntoResponse {
    let ready = runtime.ready();
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({"ready":ready})),
    )
}
async fn metrics(State(runtime): State<Arc<Runtime>>) -> impl IntoResponse {
    match runtime.store.pending_count() {
        Ok(pending) => (
            StatusCode::OK,
            format!(
                "# TYPE rakazo_beeper_ready gauge\nrakazo_beeper_ready {}\n# TYPE rakazo_beeper_pending gauge\nrakazo_beeper_pending {pending}\n",
                u8::from(runtime.ready())
            ),
        ),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database unavailable\n".to_string(),
        ),
    }
}
async fn transaction(
    State(runtime): State<Arc<Runtime>>,
    Path((tenant_id, txn)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(tenant) = runtime.tenants.iter().find(|t| t.config.id == tenant_id) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"errcode":"M_UNAUTHORIZED"})),
        );
    };
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if !bool::from(token.as_bytes().ct_eq(tenant.hs_token.as_bytes())) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"errcode":"M_UNAUTHORIZED"})),
        );
    }
    if !tenant.initialized.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"errcode":"M_UNKNOWN","error":"initializing agent rooms"})),
        );
    }
    match tenant.ingest(&txn, &body) {
        Ok(()) => (StatusCode::OK, Json(json!({}))),
        Err(error) => {
            tracing::error!(tenant=%tenant.config.id,error=%error,"transaction not acknowledged");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"errcode":"M_UNKNOWN","error":"transaction not committed"})),
            )
        }
    }
}

/// @derives-from url:https://github.com/mautrix/go/blob/main/appservice/websocket.go -- Beeper as_sync v3 framing, independently implemented in Rust
pub async fn websocket_session(tenant: Arc<TenantRuntime>) -> Result<()> {
    let mut url = tenant.config.homeserver.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid homeserver URL"))?
        .pop_if_empty()
        .extend(["_matrix", "client", "unstable", "fi.mau.as_sync"]);
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid websocket URL"))?;
    let mut request = url.as_str().into_client_request()?;
    let mut token = format!("Bearer {}", tenant.as_token).parse::<axum::http::HeaderValue>()?;
    token.set_sensitive(true);
    request.headers_mut().insert("Authorization", token);
    request
        .headers_mut()
        .insert("X-Mautrix-Websocket-Version", "3".parse()?);
    request
        .headers_mut()
        .insert("User-Agent", "rakazo-beeper/0.1.0".parse()?);
    let process = crate::stable_id(&[&tenant.config.id, &std::process::id().to_string()]);
    request
        .headers_mut()
        .insert("X-Mautrix-Process-ID", process.parse()?);
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(30), connect_async(request))
        .await
        .context("Beeper websocket handshake timed out")?
        .map_err(|_| anyhow::anyhow!("Beeper websocket handshake failed"))?;
    tenant.connected.store(true, Ordering::Relaxed);
    tracing::info!(tenant=%tenant.config.id,"Beeper websocket connected");
    socket
        .send(Message::Text(
            json!({"command":"bridge_status","data":{"state_event":"CONNECTED"}})
                .to_string()
                .into(),
        ))
        .await?;
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await;
    let mut awaiting_pong = false;
    loop {
        tokio::select! {
            _ = ping.tick() => {
                ensure!(!awaiting_pong,"Beeper websocket heartbeat timed out");
                awaiting_pong = true;
                socket.send(Message::Text(json!({"id":-1,"command":"ping","data":{"timestamp":now()*1000}}).to_string().into())).await?;
            }
            frame = socket.next() => {
                let frame = frame.context("Beeper websocket closed")??;
                match frame {
                    Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await?,
                    Message::Pong(_) => {},
                    Message::Close(close) => {
                        if close.as_ref().is_some_and(|c| u16::from(c.code) == 4001
                            || serde_json::from_str::<Value>(&c.reason).ok().is_some_and(|reason| reason["status"] == "conn_replaced")) {
                            return Err(ConnectionReplaced.into());
                        }
                        bail!("Beeper websocket closed");
                    }
                    Message::Text(text) => {
                        ensure!(text.len() <= 8*1024*1024,"Beeper websocket frame too large");
                        let msg: Value = serde_json::from_str(&text).context("invalid Beeper websocket JSON")?;
                        let command = msg["command"].as_str().unwrap_or("");
                        let id = msg["id"].as_i64().unwrap_or(0);
                        if command == "response" && id == -1 { awaiting_pong = false; continue }
                        if matches!(command,"connect"|"response") { continue }
                        if command == "error" { bail!("Beeper websocket returned a protocol error") }
                        let response = if command == "transaction" || command.is_empty() {
                            let txn = msg["txn_id"].as_str().context("Beeper transaction missing txn_id")?;
                            match tenant.ingest(txn,&msg) {
                                Ok(()) => json!({"id":id,"command":"response","data":{"txn_id":txn}}),
                                Err(error) => {
                                    tracing::error!(tenant=%tenant.config.id,error=%error,"Beeper transaction not committed");
                                    json!({"id":id,"command":"error","data":{"code":"M_UNKNOWN","message":"transaction not committed"}})
                                }
                            }
                        } else if command == "ping" {
                            json!({"id":id,"command":"response","data":{"timestamp":now()*1000}})
                        } else {
                            json!({"id":id,"command":"error","data":{"code":"M_UNRECOGNIZED","message":"unsupported websocket command"}})
                        };
                        if id != 0 { socket.send(Message::Text(response.to_string().into())).await?; }
                    }
                    _ => bail!("unexpected binary Beeper websocket frame"),
                }
            }
        }
    }
}

pub async fn websocket_loop(tenant: Arc<TenantRuntime>) -> Result<()> {
    let mut backoff = 2;
    loop {
        // Leave initial events queued on the homeserver until room mappings exist locally.
        if !tenant.initialized.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let started = now();
        let result = websocket_session(tenant.clone()).await;
        tenant.connected.store(false, Ordering::Relaxed);
        if let Err(error) = result {
            if error.downcast_ref::<ConnectionReplaced>().is_some() {
                return Err(error);
            }
            tracing::warn!(tenant=%tenant.config.id,error=%error,"Beeper connection interrupted; reconnecting");
        }
        if now().saturating_sub(started) > 300 {
            backoff = 2;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(120);
    }
}
