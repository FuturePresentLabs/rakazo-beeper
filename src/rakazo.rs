use crate::config::{Tenant, secret};
use anyhow::{Context, Result, ensure};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

#[derive(Clone)]
pub struct Rakazo {
    client: Client,
    base: Url,
}
#[derive(Clone, Deserialize)]
pub struct Bot {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: String,
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
    pub fn with_token(config: &Tenant, token: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(
            "x-rakazo-space-id",
            HeaderValue::from_str(&config.rakazo_space_id)?,
        );
        Ok(Self {
            client: Client::builder()
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base: config.rakazo_url.clone(),
        })
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
