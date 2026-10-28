//! Beeper Desktop local HTTP API transport (`transport = "beeper"`).
//!
//! CITATION: contract mirrored from Beeper Desktop 4.3.209's bundled
//! `beeper-desktop-api` and its OpenAPI document (`Beeper Client API 5.0.0`,
//! `build/main/openapi-*.mjs` inside `app.asar`):
//!
//! - `GET  {base}/v1/info`
//! - `GET  {base}/v1/chats?limit=N`
//! - `GET  {base}/v1/chats/{chatID}`
//! - `GET  {base}/v1/chats/{chatID}/messages?limit=N`
//! - `POST {base}/v1/chats/{chatID}/messages` → `{"pendingMessageID": "…"}`
//!
//! Auth is `Authorization: Bearer <token>`. The API is **off unless the human
//! enables it in Beeper Desktop**; a refused connection is the typed
//! [`AgentError::NotEnabled`], never a generic transport failure.
//!
//! This is a provider beside `ohmos`/`msgd`, not a second bridge. It reuses
//! [`super::ohmos::AgentBridge`] for the durable ledger, `from_me` suppression
//! and `pi-notify` delivery; this module only speaks HTTP and maps Beeper's
//! message shape onto the existing `NativeMessage`.
//!
//! Message identity is Beeper's opaque `id` (serialised as `native_id`), never
//! text and never an index. Beeper has no idempotency key on send, so on a
//! transport error we do **one** bounded re-read of the newest messages looking
//! for our exact text from ourselves before returning the error. If it landed we
//! report queued with its id; otherwise the caller retries (and the ledger is
//! only advanced after a confirmed id, so a retry cannot silently double-send).
//!
//! The token is read from a path only. The value is redacted by `Debug`, never
//! printed by probe/status/errors, and the file must be owner-only (`0600`).

use super::{
    AgentConfig,
    agent_conversations::{AgentError, HttpResponse, Token},
    ohmos::{AgentInbound, AgentOutbound, AgentSendOutcome, AgentTransport},
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Local loopback Beeper listens on 23373 by default when the API is enabled.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:23373";

/// The human step that creates the token. Printed only when it is missing.
pub const TOKEN_ACQUISITION: &str = concat!(
    "open Beeper Desktop → Settings → Integrations, enable the local API, then ",
    "run: mkdir -p ~/.config/rakazo && chmod 700 ~/.config/rakazo && ",
    "printf '%s' '<token>' > ~/.config/rakazo/beeper-token && ",
    "chmod 600 ~/.config/rakazo/beeper-token"
);

/// The exact config switch to the agora `sh-whatsapp` path once access exists.
/// This is deliberately a config edit, not a rediscovery.
pub const SH_WHATSAPP_MIGRATION: &str = concat!(
    "later switch: transport = \"sh-whatsapp\", base_url = \"http://<agora-one>:<port>\", ",
    "token_file = \"/path/to/agora-token\"; needs sshd/cert access on agora-one ",
    "OR a reachable endpoint+token. BLOCKED on host access: agora-one answers ping ",
    "but its sshd closes connections, so no cert can be installed and no endpoint verified."
);

const TIMEOUT: Duration = Duration::from_secs(10);
/// Beeper message ids are opaque but never contain `/`; still escape the path
/// segment so a malformed chat id cannot change the endpoint.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b':')
    .remove(b'@');

pub const INFO_PATH: &str = "/v1/info";
pub const CHATS_PATH: &str = "/v1/chats";

/// Resolve `BEEPER_<KEY>_TOKEN_FILE`, mirroring the ohmOS env override.
pub fn token_file_from_env(key: &str) -> Option<PathBuf> {
    let var = format!("BEEPER_{}_TOKEN_FILE", key.to_uppercase().replace('-', "_"));
    std::env::var_os(var).map(PathBuf::from)
}

/// Load an owner-only token from a path. Missing is typed `token-missing`.
pub fn load_token(path: &Path) -> Result<Token, AgentError> {
    Token::load(path)
}

/// The small HTTP seam. Unlike the ohmOS client this keeps the transport error
/// typed so a refused loopback connection can become `not-enabled` rather than
/// a stringly "request failed".
pub trait BeeperHttp {
    fn get(&self, url: &str, token: &Token) -> Result<HttpResponse, AgentError>;
    fn post(&self, url: &str, token: &Token, body: &str) -> Result<HttpResponse, AgentError>;
}

/// Production `ureq` exchange: short timeout (local API), no redirects.
pub struct UreqBeeperHttp {
    agent: ureq::Agent,
}

impl UreqBeeperHttp {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(TIMEOUT)
                .redirects(0)
                .build(),
        }
    }
}

impl Default for UreqBeeperHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl BeeperHttp for UreqBeeperHttp {
    fn get(&self, url: &str, token: &Token) -> Result<HttpResponse, AgentError> {
        let request = self
            .agent
            .get(url)
            .set("authorization", &token.header_value())
            .set("accept", "application/json");
        finish(request.call(), url)
    }

    fn post(&self, url: &str, token: &Token, body: &str) -> Result<HttpResponse, AgentError> {
        let request = self
            .agent
            .post(url)
            .set("authorization", &token.header_value())
            .set("content-type", "application/json")
            .set("accept", "application/json");
        finish(request.send_string(body), url)
    }
}

fn finish(
    result: Result<ureq::Response, ureq::Error>,
    url: &str,
) -> Result<HttpResponse, AgentError> {
    match result {
        Ok(response) => Ok(HttpResponse {
            status: response.status(),
            body: response
                .into_string()
                .map_err(|error| AgentError::Transport {
                    endpoint: url.to_string(),
                    detail: error.to_string(),
                })?,
        }),
        Err(ureq::Error::Status(status, response)) => Ok(HttpResponse {
            status,
            body: response.into_string().unwrap_or_default(),
        }),
        Err(error) => {
            let detail = error.to_string();
            if is_not_enabled(&error, &detail) {
                Err(AgentError::NotEnabled {
                    endpoint: url.to_string(),
                })
            } else {
                Err(AgentError::Transport {
                    endpoint: url.to_string(),
                    detail,
                })
            }
        }
    }
}

fn is_not_enabled(error: &ureq::Error, detail: &str) -> bool {
    let ureq::Error::Transport(transport) = error else {
        return false;
    };
    match transport.kind() {
        ureq::ErrorKind::ConnectionFailed => true,
        ureq::ErrorKind::Io => {
            detail.contains("Connection refused")
                || detail.contains("connection refused")
                || detail.contains("os error 61")
                || detail.contains("os error 111")
        }
        _ => false,
    }
}

/// One `GET /v1/chats` item. Only the fields this bridge prints are modelled;
/// `deny_unknown_fields` is deliberately not used because the API is versioned
/// and additive.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeeperChat {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub local_chat_id: Option<serde_json::Value>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub is_archived: bool,
}

impl BeeperChat {
    pub fn label(&self) -> String {
        match (&self.title, &self.network) {
            (Some(title), Some(network)) if !title.trim().is_empty() => {
                format!("{title} [{network}]")
            }
            (Some(title), _) if !title.trim().is_empty() => title.clone(),
            _ => self.id.clone(),
        }
    }
}

/// One Beeper message. Identity is `id`; `isSender` means authored by us.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeeperMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub sort_key: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub sender_id: Option<String>,
    #[serde(default)]
    pub sender_name: Option<String>,
    #[serde(default)]
    pub is_sender: bool,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
}

impl BeeperMessage {
    /// Map onto the existing ohmOS-shaped message so `AgentBridge` needs no
    /// second dedupe or delivery path. `deleted` is not part of the documented
    /// message schema; Beeper sends `message.deleted` events instead.
    pub fn into_native(self) -> super::agent_conversations::NativeMessage {
        super::agent_conversations::NativeMessage {
            native_id: self.id,
            from_me: self.is_sender,
            sender: self
                .sender_name
                .or(self.sender_id)
                .unwrap_or_else(|| "beeper".to_string()),
            text: self.text,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct List<T> {
    #[serde(default)]
    items: Vec<T>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendResponse {
    #[serde(default)]
    pending_message_id: Option<String>,
    /// Some Beeper builds also return a final `messageID`; accept either.
    #[serde(default)]
    message_id: Option<String>,
}

impl SendResponse {
    fn id(self) -> Option<String> {
        self.pending_message_id.or(self.message_id)
    }
}

/// A client for one Beeper API base. `base: None` means offline: every method
/// refuses before touching the HTTP seam.
pub struct BeeperClient<H: BeeperHttp = UreqBeeperHttp> {
    base: Option<String>,
    token: Token,
    http: H,
}

impl<H: BeeperHttp> BeeperClient<H> {
    pub fn new(base_url: Option<&str>, token: Token, http: H) -> Result<Self, AgentError> {
        let base = match base_url {
            Some(base) => Some(validate_base_url(base)?),
            None => None,
        };
        Ok(Self { base, token, http })
    }

    pub fn is_offline(&self) -> bool {
        self.base.is_none()
    }

    fn url(&self, path: &str) -> Result<String, AgentError> {
        let base = self.base.as_ref().ok_or(AgentError::Offline)?;
        Ok(format!("{base}{path}"))
    }

    fn get(&self, path: &str) -> Result<HttpResponse, AgentError> {
        let url = self.url(path)?;
        self.http.get(&url, &self.token)
    }

    fn post(&self, path: &str, body: &str) -> Result<HttpResponse, AgentError> {
        let url = self.url(path)?;
        self.http.post(&url, &self.token, body)
    }

    /// `GET /v1/info`: proves the app API is enabled and the token is accepted.
    pub fn info(&self) -> Result<serde_json::Value, AgentError> {
        let response = self.get(INFO_PATH)?;
        self.decode(INFO_PATH, response)
    }

    /// `GET /v1/chats?limit=N`.
    pub fn chats(&self, limit: usize) -> Result<Vec<BeeperChat>, AgentError> {
        validate_limit(limit)?;
        let path = format!("{CHATS_PATH}?limit={limit}");
        let response = self.get(&path)?;
        let list: List<BeeperChat> = self.decode(&path, response)?;
        Ok(list.items)
    }

    /// `GET /v1/chats/{chatID}` (used by probe to print the configured chat).
    pub fn chat(&self, chat_id: &str) -> Result<BeeperChat, AgentError> {
        let path = chat_path(chat_id)?;
        let response = self.get(&path)?;
        self.decode(&path, response)
    }

    /// `GET /v1/chats/{chatID}/messages?limit=N`.
    pub fn messages(&self, chat_id: &str, limit: usize) -> Result<Vec<BeeperMessage>, AgentError> {
        let path = messages_path(chat_id, limit)?;
        let response = self.get(&path)?;
        let list: List<BeeperMessage> = self.decode(&path, response)?;
        Ok(list.items)
    }

    /// `POST /v1/chats/{chatID}/messages`. Returns Beeper's pending message id,
    /// which is recorded in the durable ledger before success is reported.
    pub fn send(&self, chat_id: &str, text: &str) -> Result<String, AgentError> {
        if text.trim().is_empty() || text.len() > 16_384 {
            return Err(AgentError::Refused(
                "text must be non-empty and <=16384 bytes".to_string(),
            ));
        }
        let path = messages_path(chat_id, 1)?;
        // The path helper appends `?limit=1`; POST must not carry it.
        let path = path.split('?').next().unwrap_or(&path).to_string();
        let body =
            serde_json::to_string(&serde_json::json!({ "text": text })).map_err(|error| {
                AgentError::Protocol {
                    endpoint: path.clone(),
                    detail: error.to_string(),
                }
            })?;
        let response = self.post(&path, &body)?;
        let receipt: SendResponse = self.decode(&path, response)?;
        receipt.id().ok_or_else(|| AgentError::Protocol {
            endpoint: path,
            detail: "send returned no pendingMessageID".to_string(),
        })
    }

    /// Bounded "did my id land?" re-read used only after a transport error.
    /// Returns the id of a self-authored message with exactly this text.
    pub fn find_recent_self_text(
        &self,
        chat_id: &str,
        text: &str,
        limit: usize,
    ) -> Result<Option<String>, AgentError> {
        let messages = self.messages(chat_id, limit)?;
        Ok(messages
            .into_iter()
            .rev()
            .find(|message| message.is_sender && message.text.as_deref() == Some(text))
            .map(|message| message.id))
    }

    /// Read-only probe: statuses plus chat ids/names only. Never message text.
    pub fn probe(&self, chat_id: Option<&str>) -> BeeperProbeReport {
        let mut report = BeeperProbeReport::default();
        if self.is_offline() {
            report.offline = true;
            return report;
        }
        match self.info() {
            Ok(_) => report.info_status = Some(200),
            Err(error) => {
                report.info_status = error.status();
                report.errors.push(error);
            }
        }
        match self.chats(50) {
            Ok(chats) => {
                report.chats_status = Some(200);
                report.chats = chats
                    .into_iter()
                    .map(|chat| {
                        let label = chat.label();
                        (chat.id, label)
                    })
                    .collect();
            }
            Err(error) => {
                report.chats_status = error.status();
                report.errors.push(error);
            }
        }
        if let Some(chat_id) = chat_id {
            report.chat_path = Some(chat_path(chat_id).unwrap_or_else(|_| chat_id.to_string()));
            match self.chat(chat_id) {
                Ok(chat) => {
                    report.chat_status = Some(200);
                    report.chat_label = Some(chat.label());
                }
                Err(error) => {
                    report.chat_status = error.status();
                    report.errors.push(error);
                }
            }
        }
        report
    }

    fn decode<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        response: HttpResponse,
    ) -> Result<T, AgentError> {
        classify(endpoint, response.status)?;
        serde_json::from_str(&response.body).map_err(|_| AgentError::Protocol {
            endpoint: endpoint.to_string(),
            // Never echo the body: it contains message text.
            detail: "invalid JSON body".to_string(),
        })
    }
}

fn validate_limit(limit: usize) -> Result<(), AgentError> {
    if !(1..=200).contains(&limit) {
        return Err(AgentError::Refused(
            "message/chat limit must be 1-200".to_string(),
        ));
    }
    Ok(())
}

fn segment(chat_id: &str) -> Result<String, AgentError> {
    if chat_id.trim().is_empty() {
        return Err(AgentError::Refused(
            "beeper chat_id is not configured".to_string(),
        ));
    }
    Ok(utf8_percent_encode(chat_id, PATH_SEGMENT).to_string())
}

pub fn chat_path(chat_id: &str) -> Result<String, AgentError> {
    Ok(format!("{CHATS_PATH}/{}", segment(chat_id)?))
}

pub fn messages_path(chat_id: &str, limit: usize) -> Result<String, AgentError> {
    validate_limit(limit)?;
    Ok(format!("{}/messages?limit={limit}", chat_path(chat_id)?))
}

/// Validate a configured base URL. Plain HTTP is allowed only on loopback: a
/// bearer token must not be sent in clear text to a remote host by accident.
pub fn validate_base_url(base: &str) -> Result<String, AgentError> {
    let url = url::Url::parse(base)
        .map_err(|error| AgentError::Refused(format!("invalid beeper base_url: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AgentError::Refused(
            "beeper base_url must be http or https".to_string(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AgentError::Refused(
            "beeper base_url must not contain credentials, query, or fragment".to_string(),
        ));
    }
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if url.scheme() == "http" && !loopback {
        return Err(AgentError::Refused(
            "beeper base_url over http must be loopback (use https for a remote Beeper Server)"
                .to_string(),
        ));
    }
    Ok(base.trim_end_matches('/').to_string())
}

fn classify(endpoint: &str, status: u16) -> Result<(), AgentError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(AgentError::Unauthorized {
            endpoint: endpoint.to_string(),
        }),
        404 if endpoint.starts_with(CHATS_PATH) => Err(AgentError::ChatNotFound {
            endpoint: endpoint.to_string(),
        }),
        other => Err(AgentError::Http {
            endpoint: endpoint.to_string(),
            status: other,
        }),
    }
}

/// Probe output. Statuses and chat ids/names only; never message text or token.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BeeperProbeReport {
    pub offline: bool,
    pub info_status: Option<u16>,
    pub chats_status: Option<u16>,
    pub chats: Vec<(String, String)>,
    pub chat_path: Option<String>,
    pub chat_status: Option<u16>,
    pub chat_label: Option<String>,
    pub errors: Vec<AgentError>,
}

impl BeeperProbeReport {
    pub fn lines(&self) -> Vec<String> {
        if self.offline {
            return vec!["offline\tno base_url configured; zero network calls".to_string()];
        }
        let mut lines = Vec::new();
        lines.push(format!(
            "GET {INFO_PATH}\tstatus={}\t{}",
            status_word(self.info_status),
            outcome_word(self.info_status)
        ));
        lines.push(format!(
            "GET {CHATS_PATH}\tstatus={}\t{}",
            status_word(self.chats_status),
            outcome_word(self.chats_status)
        ));
        for (id, label) in &self.chats {
            lines.push(format!("chat_id\t{id}\t{label}"));
        }
        if let Some(path) = &self.chat_path {
            lines.push(format!(
                "GET {path}\tstatus={}\t{}",
                status_word(self.chat_status),
                outcome_word(self.chat_status)
            ));
            if let Some(label) = &self.chat_label {
                lines.push(format!("configured_chat\t{label}"));
            }
        }
        for error in &self.errors {
            lines.push(format!("error\t{}\t{error}", error.code()));
        }
        lines
    }
}

fn status_word(status: Option<u16>) -> String {
    status
        .map(|status| status.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn outcome_word(status: Option<u16>) -> &'static str {
    match status {
        Some(200..=299) => "ok",
        Some(401) => "unauthorized",
        Some(404) => "chat-not-found",
        Some(_) => "http_error",
        None => "transport_error",
    }
}

/// The real provider for one Beeper chat.
pub struct BeeperTransport<H: BeeperHttp = UreqBeeperHttp> {
    client: BeeperClient<H>,
    chat_id: String,
    limit: usize,
    last_receipt_id: Option<String>,
}

impl<H: BeeperHttp> BeeperTransport<H> {
    pub fn new(client: BeeperClient<H>, chat_id: String, limit: usize) -> Self {
        Self {
            client,
            chat_id,
            limit,
            last_receipt_id: None,
        }
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }
}

impl<H: BeeperHttp> AgentTransport for BeeperTransport<H> {
    fn poll(&mut self) -> Result<Vec<AgentInbound>, AgentError> {
        let messages = self.client.messages(&self.chat_id, self.limit)?;
        Ok(messages
            .into_iter()
            .map(|message| AgentInbound {
                conversation_id: self.chat_id.clone(),
                message: message.into_native(),
            })
            .collect())
    }

    fn send(&mut self, outbound: &AgentOutbound) -> Result<AgentSendOutcome, AgentError> {
        if outbound.text.trim().is_empty() {
            return Err(AgentError::Refused("refusing empty reply text".into()));
        }
        match self.client.send(&self.chat_id, &outbound.text) {
            Ok(id) => {
                self.last_receipt_id = Some(id);
                Ok(AgentSendOutcome::Queued)
            }
            Err(error) if is_dispatched_error(&error) => {
                // Beeper has no idempotency key on POST. Before reporting a
                // failure (and letting the caller re-post on the next poll), do
                // exactly one bounded re-read and accept the message only if it
                // is ours with byte-identical text. This is the documented
                // "did my id land?" check; a still-refused API returns the
                // original typed error, not a fake success.
                if let Ok(Some(id)) =
                    self.client
                        .find_recent_self_text(&self.chat_id, &outbound.text, 25)
                {
                    self.last_receipt_id = Some(id);
                    return Ok(AgentSendOutcome::Queued);
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    fn last_receipt_id(&self) -> Option<String> {
        self.last_receipt_id.clone()
    }

    fn reference(&self, _conversation_id: &str, native_id: &str) -> String {
        // Required shape: rakazo:beeper:<chatID>:<messageID>.
        format!("rakazo:beeper:{}:{native_id}", self.chat_id)
    }
}

/// A transport-ish failure is the only case where a re-read can tell us whether
/// the POST landed. A typed 401/404 is a decision, not an unknown outcome.
fn is_dispatched_error(error: &AgentError) -> bool {
    matches!(
        error,
        AgentError::Transport { .. } | AgentError::NotEnabled { .. } | AgentError::Protocol { .. }
    )
}

/// Per-agent ledger filename for the beeper transport. Kept separate from
/// `*-ohmos.json` so switching providers cannot reuse another provider's ids.
pub fn default_state_file(key: &str, configured: Option<&Path>) -> PathBuf {
    if let Some(path) = configured {
        return path.to_path_buf();
    }
    let base = std::env::var_os("RAKAZO_BRIDGE_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/state/rakazo/bridge"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(format!("{key}-beeper.json"))
}

/// Truthful status for `bridge status`: one live `GET /v1/info`, typed failure
/// codes, and the token path plus masked fingerprint. Never the token value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BeeperStatus {
    pub token_path: Option<PathBuf>,
    pub token_fingerprint: Option<String>,
    pub last_poll_ok: bool,
    pub last_error: Option<String>,
}

impl BeeperStatus {
    /// `status=muse token=/path (…a1b2) transport=beeper last_poll_ok=false
    /// last_error=not-enabled`
    pub fn line(&self, key: &str) -> String {
        let token = match (&self.token_path, &self.token_fingerprint) {
            (Some(path), Some(fingerprint)) => format!("{} ({fingerprint})", path.display()),
            (Some(path), None) => format!("{} (missing)", path.display()),
            (None, _) => "unconfigured".to_string(),
        };
        format!(
            "agent={key}\ttransport=beeper\ttoken={token}\tlast_poll_ok={}\tlast_error={}",
            self.last_poll_ok,
            self.last_error.as_deref().unwrap_or("none")
        )
    }
}

/// Read the token and make one live `/v1/info` call. This is used by
/// `bridge status` and by `agent-serve` startup, so the stamp is live rather
/// than "the file exists".
pub fn status(agent: &AgentConfig) -> BeeperStatus {
    let path = agent.token_file.clone();
    let mut status = BeeperStatus {
        token_path: path.clone(),
        ..Default::default()
    };
    let Some(path) = path else {
        status.last_error = Some("token-missing".to_string());
        return status;
    };
    let token = match load_token(&path) {
        Ok(token) => token,
        Err(error) => {
            status.last_error = Some(error.code().to_string());
            return status;
        }
    };
    status.token_fingerprint = Some(token.fingerprint());
    let Some(base) = agent.base_url.as_deref() else {
        status.last_error = Some("offline".to_string());
        return status;
    };
    let client = match BeeperClient::new(Some(base), token, UreqBeeperHttp::new()) {
        Ok(client) => client,
        Err(error) => {
            status.last_error = Some(error.code().to_string());
            return status;
        }
    };
    match client.info() {
        Ok(_) => status.last_poll_ok = true,
        Err(error) => status.last_error = Some(error.code().to_string()),
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_rejects_remote_plain_http_but_allows_loopback() {
        assert_eq!(
            validate_base_url("http://127.0.0.1:23373/").unwrap(),
            "http://127.0.0.1:23373"
        );
        assert!(validate_base_url("https://beeper.example").is_ok());
        assert!(validate_base_url("http://beeper.example").is_err());
        assert!(validate_base_url("http://127.0.0.1:23373/?q=1").is_err());
    }

    #[test]
    fn message_maps_to_native_identity_not_text() {
        let message = BeeperMessage {
            id: "msg-1".into(),
            is_sender: false,
            text: Some("hello".into()),
            ..Default::default()
        };
        let native = message.into_native();
        assert_eq!(native.native_id, "msg-1");
        assert!(!native.from_me);
        assert_eq!(native.text.as_deref(), Some("hello"));
    }

    #[test]
    fn send_without_id_is_a_protocol_error() {
        let http = FakeHttp::default();
        let client = BeeperClient::new(
            Some("http://127.0.0.1:23373"),
            Token::from_test("secret-token"),
            http,
        )
        .unwrap();
        let error = client.send("chat:1", "hi").unwrap_err();
        assert_eq!(error.code(), "protocol-error");
    }

    #[derive(Clone, Default)]
    struct FakeHttp;

    impl BeeperHttp for FakeHttp {
        fn get(&self, _url: &str, _token: &Token) -> Result<HttpResponse, AgentError> {
            Ok(HttpResponse {
                status: 200,
                body: "{}".into(),
            })
        }
        fn post(
            &self,
            _url: &str,
            _token: &Token,
            _body: &str,
        ) -> Result<HttpResponse, AgentError> {
            Ok(HttpResponse {
                status: 200,
                body: "{}".into(),
            })
        }
    }
}
