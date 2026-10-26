use crate::config::{Tenant, secret};
use anyhow::{Context, Result, ensure};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt;
use url::Url;

/// The server answered a request with `401 Unauthorized`.
///
/// A typed error (rather than a message) lets `orch status` report
/// `token=invalid` without ever treating an expired session as healthy.
#[derive(Clone, Copy, Debug)]
pub struct Unauthorized;

impl fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Rakazo rejected the bearer token (HTTP 401)")
    }
}

impl std::error::Error for Unauthorized {}

#[derive(Clone)]
pub struct Rakazo {
    client: Client,
    base: Url,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bot {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub space_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub parent_bot_id: Option<String>,
    #[serde(default)]
    pub spawn_key: Option<String>,
    #[serde(default)]
    pub archived_at: Option<String>,
    /// Present only for a bot backed by a messaging attachment. A normal
    /// Agent bot omits this object entirely.
    #[serde(default)]
    pub capabilities: Option<Value>,
    /// Server-side relay-only flag (pm-fpl owns it). `None` when the deployment
    /// does not report it yet; status prints that honestly instead of guessing.
    #[serde(default)]
    pub relay_only: Option<bool>,
}

/// `bots.create` input; `spawnKey` makes a create idempotent within a space.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBot<'a> {
    pub name: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub instructions: &'a str,
    pub notify_on_finish: bool,
    pub spawn_key: &'a str,
    pub computer_mode: &'a str,
    /// Relay-only: the bot may append assistant messages but never runs a
    /// model. Every fleet bot this bridge creates sets this; the server flag is
    /// owned by pm-fpl and unknown-field-safe (zod strips it until it lands).
    pub relay_only: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub avatar_style: String,
    #[serde(default)]
    pub max_upload_size_bytes: Option<usize>,
    #[serde(default)]
    pub attachment_mime_types: Option<Vec<String>>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub seq: i64,
    pub role: String,
    pub blocks: Vec<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub client_nonce: Option<String>,
    #[serde(default)]
    pub reply_to_message_id: Option<String>,
    #[serde(default)]
    pub reply_quote: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendReceipt {
    pub message_id: String,
    pub seq: i64,
}
/// `serviceTokens.issue` result. The raw token is returned exactly once; the
/// bridge writes it owner-only and never prints it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceTokenReceipt {
    pub id: String,
    pub token: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendReceipt {
    pub seq: i64,
    pub run_id: String,
    #[serde(default)]
    pub run_ids: Vec<String>,
}
impl Message {
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter(|b| matches!(b["kind"].as_str(), Some("text" | "ask")))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
/// One conversation attachment linked to this session's space.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedAttachment {
    pub id: String,
    #[serde(default)]
    pub binding_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub linked_at: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub messages: Vec<Message>,
    pub older_cursor: Option<i64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub messages: Vec<Message>,
    pub run: Option<Value>,
    #[serde(default)]
    pub active_runs: Vec<Value>,
}
impl Snapshot {
    pub fn busy(&self) -> bool {
        self.run.iter().chain(self.active_runs.iter()).any(|r| {
            !matches!(
                r["status"].as_str(),
                Some("completed" | "failed" | "cancelled" | "waiting_input" | "waiting_takeover")
            )
        })
    }
}

impl Rakazo {
    pub fn new(config: &Tenant) -> Result<Self> {
        Self::with_token(config, &secret(&config.rakazo_token_env)?)
    }
    /// Build a client from explicit parts so tooling does not need a tenant bundle.
    pub fn from_parts(base: Url, token: &str, space: &str) -> Result<Self> {
        Self::build(base, token, space)
    }
    /// A bot-bound service principal: same RPC envelope, different bearer.
    pub fn from_service_parts(base: Url, token: &str, space: &str) -> Result<Self> {
        Self::build(base, token, space)
    }
    pub fn with_token(config: &Tenant, token: &str) -> Result<Self> {
        Self::build(config.rakazo_url.clone(), token, &config.rakazo_space_id)
    }
    fn build(base: Url, token: &str, space: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert("x-rakazo-space-id", HeaderValue::from_str(space)?);
        Ok(Self {
            client: Client::builder()
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base,
        })
    }

    /// Base deployment URL, for building a per-agent service principal without
    /// re-reading the environment.
    pub fn base(&self) -> &Url {
        &self.base
    }
    /// @implements url:https://orpc.dev/docs/rpc/protocol -- JSON serializer envelope and slash routing
    pub async fn rpc<T: serde::de::DeserializeOwned>(
        &self,
        path: &[&str],
        input: Value,
    ) -> Result<T> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid RPC base"))?
            .pop_if_empty()
            .extend(["rpc"])
            .extend(path);
        let response = self
            .client
            .post(url)
            .json(&json!({"json": input}))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Rakazo transport failed: {}", e.without_url()))?;
        if response.status().as_u16() == 401 {
            return Err(Unauthorized.into());
        }
        if response.status().as_u16() == 404 {
            return Err(
                crate::media::Rejected("That Rakazo resource is no longer available.").into(),
            );
        }
        if matches!(response.status().as_u16(), 400 | 413 | 415 | 422) {
            return Err(crate::media::Rejected(
                "Rakazo rejected this message or attachment format.",
            )
            .into());
        }
        ensure!(
            response.status().is_success(),
            "Rakazo RPC returned HTTP {}",
            response.status()
        );
        use futures_util::StreamExt;
        let limit = crate::media::MAX_BYTES.div_ceil(3) * 4 + 1024 * 1024;
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= limit as u64),
            crate::media::Rejected("Rakazo response exceeds the bridge size limit.")
        );
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("Rakazo response stream failed"))?;
            ensure!(
                chunk.len() <= limit.saturating_sub(bytes.len()),
                crate::media::Rejected("Rakazo response exceeds the bridge size limit.")
            );
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).context("invalid Rakazo response")?;
        let value = body.get("json").context("missing oRPC json envelope")?;
        serde_json::from_value(value.clone()).context("Rakazo contract mismatch")
    }

    /// @see github:elie222/rakazo@535de9600bdd90530aa7cd954bf2d29b4a083aaf packages/contracts/src/rpc.ts
    pub async fn bots(&self) -> Result<Vec<Bot>> {
        self.rpc(&["bots", "list"], Value::Null).await
    }
    /// Unauthenticated liveness probe (`GET /healthz`). The bearer token is
    /// deliberately not sent to the health endpoint.
    pub async fn healthz(&self) -> Result<u16> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid RPC base"))?
            .pop_if_empty()
            .extend(["healthz"]);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Rakazo health probe failed: {}", e.without_url()))?;
        Ok(response.status().as_u16())
    }
    /// Messaging attachments linked to this session's user and space.
    pub async fn linked_attachments(&self) -> Result<Vec<LinkedAttachment>> {
        self.rpc(&["conversationAttachments", "linked"], Value::Null)
            .await
    }
    /// Post plain text through a linked messaging attachment. This is the
    /// interim reply surface: it never writes a user-role thread message and
    /// never starts a model run. `clientNonce` must be a UUID.
    pub async fn direct_send_attachment(
        &self,
        attachment_id: &str,
        text: &str,
        nonce: &str,
    ) -> Result<()> {
        let _: Value = self
            .rpc(
                &["conversationAttachments", "directSend"],
                json!({"attachmentId": attachment_id, "text": text, "clientNonce": nonce}),
            )
            .await?;
        Ok(())
    }
    /// Create one agent. `spawnKey` is the server-side idempotency key.
    pub async fn create_bot(&self, bot: &NewBot<'_>) -> Result<Bot> {
        self.rpc(&["bots", "create"], serde_json::to_value(bot)?)
            .await
    }
    /// Archive a bot we own (the orchestrator or a relay-only worker).
    pub async fn archive_bot(&self, bot: &str) -> Result<()> {
        let _: Value = self
            .rpc(&["bots", "archive"], json!({"botId": bot}))
            .await?;
        Ok(())
    }
    /// Issue a bot-bound service token from the owner session.
    ///
    /// Owner-only, per `pm/rakazo/service-token-proposal.md`: the two scopes the
    /// bridge needs are `threads:read` and `threads:write:assistant`. The raw
    /// token is returned once and must never be logged.
    pub async fn issue_service_token(&self, bot: &str) -> Result<ServiceTokenReceipt> {
        self.rpc(
            &["serviceTokens", "issue"],
            json!({"botId": bot, "scopes": ["threads:read", "threads:write:assistant"]}),
        )
        .await
    }
    pub async fn avatar_style(&self) -> Result<String> {
        Ok(self.preferences().await?.avatar_style)
    }
    pub async fn preferences(&self) -> Result<Preferences> {
        let me: Preferences = self.rpc(&["me"], Value::Null).await?;
        ensure!(
            matches!(me.avatar_style.as_str(), "organic" | "robot"),
            "unknown Rakazo avatar style"
        );
        ensure!(
            me.max_upload_size_bytes.is_none_or(|limit| limit > 0),
            "Rakazo advertised an invalid upload limit"
        );
        Ok(me)
    }
    pub async fn snapshot(&self, bot: &str) -> Result<Snapshot> {
        self.rpc(&["threads", "get"], json!({"botId": bot})).await
    }
    pub async fn send_content(
        &self,
        bot: &str,
        text: &str,
        artifacts: &[String],
        reply: Option<&str>,
        nonce: &str,
    ) -> Result<i64> {
        Ok(self
            .send_routed(bot, text, artifacts, reply, nonce)
            .await?
            .seq)
    }
    /// Write a bot message in-thread without starting a run. This is the
    /// service-token-only reply path; the session token must not be used here.
    pub async fn append_assistant(
        &self,
        bot: &str,
        text: &str,
        nonce: &str,
        reply_to: Option<&str>,
    ) -> Result<AppendReceipt> {
        let mut input = json!({"botId":bot,"text":text,"clientNonce":nonce});
        if let Some(reply) = reply_to {
            input["replyToMessageId"] = json!(reply);
        }
        self.rpc(&["threads", "appendAssistant"], input).await
    }
    pub async fn send_routed(
        &self,
        bot: &str,
        text: &str,
        artifacts: &[String],
        reply: Option<&str>,
        nonce: &str,
    ) -> Result<SendReceipt> {
        let mut input =
            json!({"botId":bot,"text":text,"artifactIds":artifacts,"clientNonce":nonce});
        if let Some(reply) = reply {
            input["replyToMessageId"] = json!(reply);
        }
        self.rpc(&["threads", "send"], input).await
    }
    pub async fn react(&self, bot: &str, message: &str, reaction: &str, nonce: &str) -> Result<()> {
        let _: Value = self
            .rpc(
                &["threads", "react"],
                json!({"botId":bot,"messageId":message,"reaction":reaction,"clientNonce":nonce}),
            )
            .await?;
        Ok(())
    }
    pub async fn create_artifact(
        &self,
        bot: &str,
        media: &crate::media::Media,
        nonce: &str,
    ) -> Result<String> {
        use base64::Engine;
        let result:Value=self.rpc(&["artifacts","create"],json!({"botId":bot,"name":media.name,"mimeType":media.mime,"contentBase64":base64::engine::general_purpose::STANDARD.encode(&media.bytes),"clientNonce":nonce})).await?;
        Ok(result["id"]
            .as_str()
            .context("Rakazo artifact missing id")?
            .to_string())
    }
    pub async fn artifact(&self, bot: &str, id: &str) -> Result<crate::media::Media> {
        use base64::Engine;
        let result: Value = self
            .rpc(&["artifacts", "get"], json!({"botId":bot,"artifactId":id}))
            .await?;
        let encoded = result["contentBase64"]
            .as_str()
            .context("Rakazo artifact missing content")?;
        ensure!(
            encoded.len() <= crate::media::MAX_BYTES.div_ceil(3) * 4,
            crate::media::Rejected("Attachment exceeds the bridge's 10 MiB limit.")
        );
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        ensure!(
            bytes.len() <= crate::media::MAX_BYTES,
            crate::media::Rejected("Attachment exceeds the bridge's 10 MiB limit.")
        );
        ensure!(
            result["size"].as_u64() == Some(bytes.len() as u64),
            "invalid artifact size"
        );
        Ok(crate::media::Media {
            name: crate::media::filename(
                result["name"].as_str().context("artifact name missing")?,
            )?,
            mime: crate::media::mime(
                result["mimeType"]
                    .as_str()
                    .context("artifact MIME type missing")?,
            )?,
            bytes,
        })
    }
    pub async fn send(&self, bot: &str, text: &str, nonce: &str) -> Result<()> {
        self.send_content(bot, text, &[], None, nonce).await?;
        Ok(())
    }
    pub async fn history_page(&self, bot: &str, before: i64) -> Result<Page> {
        let page: Page = self
            .rpc(
                &["threads", "messages"],
                json!({"botId":bot,"before":before}),
            )
            .await?;
        ensure!(
            page.messages.iter().all(|message| message.seq < before),
            "Rakazo history crossed its frozen boundary"
        );
        if let Some(next) = page.older_cursor {
            ensure!(
                next < before && page.messages.iter().all(|message| message.seq >= next),
                "Rakazo history pagination did not progress"
            );
        }
        Ok(page)
    }
    /// Highest committed sequence in the conversation (one newest page).
    /// Used to seed a fresh bridge watermark so enabling a watcher does not
    /// replay history. The server rejects an out-of-range `before`, so this
    /// deliberately omits the cursor instead of passing `i64::MAX`.
    pub async fn head_seq(&self, bot: &str) -> Result<i64> {
        let page: Page = self
            .rpc(&["threads", "messages"], json!({"botId": bot}))
            .await?;
        Ok(page
            .messages
            .iter()
            .map(|message| message.seq)
            .max()
            .unwrap_or(0))
    }
    /// Paginate backwards to the committed message sequence; never advance across a gap.
    pub async fn after(&self, bot: &str, seq: i64) -> Result<Vec<Message>> {
        let mut before: Option<i64> = None;
        let mut result = Vec::new();
        for _ in 0..1000 {
            let mut input = json!({"botId": bot});
            if let Some(cursor) = before {
                input["before"] = json!(cursor);
            }
            let page: Page = self.rpc(&["threads", "messages"], input).await?;
            let reached = page.messages.iter().any(|m| m.seq <= seq);
            result.extend(page.messages.into_iter().filter(|m| m.seq > seq));
            if reached || page.older_cursor.is_none() {
                result.sort_by_key(|m| m.seq);
                result.dedup_by_key(|m| m.seq);
                return Ok(result);
            }
            ensure!(
                page.older_cursor != before,
                "Rakazo pagination did not progress"
            );
            before = page.older_cursor;
        }
        anyhow::bail!("Rakazo history exceeds pagination limit; cursor was not advanced")
    }
}
