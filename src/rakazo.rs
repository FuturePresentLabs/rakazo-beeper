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
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub seq: i64,
    pub role: String,
    pub blocks: Vec<Value>,
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
        ensure!(
            response.status().is_success(),
            "Rakazo RPC returned HTTP {}",
            response.status()
        );
        let body: Value = response.json().await.context("invalid Rakazo response")?;
        let value = body.get("json").context("missing oRPC json envelope")?;
        serde_json::from_value(value.clone()).context("Rakazo contract mismatch")
    }

    /// @see github:elie222/rakazo@535de9600bdd90530aa7cd954bf2d29b4a083aaf packages/contracts/src/rpc.ts
    pub async fn bots(&self) -> Result<Vec<Bot>> {
        self.rpc(&["bots", "list"], Value::Null).await
    }
    pub async fn avatar_style(&self) -> Result<String> {
        let me: Value = self.rpc(&["me"], Value::Null).await?;
        let style = me["avatarStyle"]
            .as_str()
            .context("Rakazo avatar style missing")?;
        ensure!(
            matches!(style, "organic" | "robot"),
            "unknown Rakazo avatar style"
        );
        Ok(style.to_string())
    }
    pub async fn snapshot(&self, bot: &str) -> Result<Snapshot> {
        self.rpc(&["threads", "get"], json!({"botId": bot})).await
    }
    pub async fn send(&self, bot: &str, text: &str, nonce: &str) -> Result<()> {
        let _: Value = self
            .rpc(
                &["threads", "send"],
                json!({"botId": bot, "text": text, "clientNonce": nonce}),
            )
            .await?;
        Ok(())
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
