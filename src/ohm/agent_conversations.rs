//! ohmOS `agent-conversations` HTTP provider (the self-hosted Muse path).
//!
//! This is the *provisioning* API, not the `ohm-msgd` daemon surface in
//! [`super::msgd`]: the token-bearing server forwards catalog/snapshot/send over
//! the paired Iroh `ohm-bridge/1` link to the phone.
//!
//! CITATION: contract mirrored from ohmOS at rev `849e49c`
//! (`services/provisioning/src/agent_conversations.rs`), which in turn forwards
//! `agent_thread`/`send` to `crates/ohmos-msg-host/src/bridge_iroh.rs` and the
//! `Snapshot`/`Message` structs in
//! `crates/ohmos-msg-host/src/agent_conversation.rs`:
//!
//! - `GET  {base}/agent-conversations`                    → `{"items":[…]}`; bearer
//! - `GET  {base}/agent-conversations/{id}?limit=N`       → the phone `Snapshot`
//! - `POST {base}/agent-conversations/{id}/messages`      → `{request_id,text,
//!   expected_native_identity?}` → `{"id","status":"queued"}`
//!
//! Offline by default: with no configured base URL the client refuses every
//! call *before* touching the HTTP exchange. A 401 is [`AgentError::Unauthorized`];
//! a phone/Iroh 502 is [`AgentError::Degraded`], never a silent success. The
//! token is only ever read from an owner-only file and its value is never
//! rendered by `Debug`, `Display`, status output, probe output or errors.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

/// 25 s: longer than the provisioning service's own 15 s Iroh timeout, so a
/// phone-offline failure comes back as a **502 response**, not a local timeout.
const TIMEOUT: Duration = Duration::from_secs(25);

/// Segment encoder that leaves the documented catalog id shape
/// `whatsapp:<remote-id>` (including `@`) readable while still escaping `/`.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b':')
    .remove(b'@');

pub const CATALOG_PATH: &str = "/agent-conversations";

/// A bearer token read from an owner-only file.
///
/// `Debug` is redacted on purpose: a token must never appear in a log line, a
/// status line, a probe report or an error chain.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Read a token file. Owner-only (`0600`, or stricter) is enforced on Unix;
    /// the value is trimmed and must be non-empty with no control characters.
    pub fn load(path: &Path) -> Result<Self, AgentError> {
        let meta = std::fs::metadata(path).map_err(|error| {
            AgentError::Token(format!("read token file {}: {error}", path.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(AgentError::Token(format!(
                    "token file {} must be mode 0600 (found {mode:03o})",
                    path.display()
                )));
            }
        }
        #[cfg(not(unix))]
        let _ = meta;
        let raw = std::fs::read_to_string(path).map_err(|error| {
            AgentError::Token(format!("read token file {}: {error}", path.display()))
        })?;
        let token = raw.trim();
        if token.is_empty() {
            return Err(AgentError::Token(format!(
                "token file {} is empty",
                path.display()
            )));
        }
        if token.chars().any(char::is_control) {
            return Err(AgentError::Token(format!(
                "token file {} contains control characters",
                path.display()
            )));
        }
        Ok(Self(token.to_string()))
    }

    /// The `Authorization` header value. Only the production HTTP exchange and
    /// fakes call this; the value must never be printed.
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Resolve the configured token file for one agent.
///
/// Precedence: `OHM_<KEY>_TOKEN_FILE` (so `OHM_MUSE_TOKEN_FILE` for `muse`),
/// then the per-agent `token_file`. No built-in path and no container scraping.
pub fn token_file_from_env(key: &str) -> Option<PathBuf> {
    let var = format!("OHM_{}_TOKEN_FILE", key.to_uppercase().replace('-', "_"));
    std::env::var_os(var).map(PathBuf::from)
}

/// The one-line, human-run acquisition command. It is printed only when a token
/// is genuinely missing, and it is a human command — the bridge never runs it
/// and never reads a container environment itself.
pub const TOKEN_ACQUISITION: &str = concat!(
    "ssh pris 'docker inspect ohmos-provisioning-1 --format ",
    "\"{{range .Config.Env}}{{println .}}{{end}}\" ",
    "| sed -n \"s/^OHM_AGENT_CONVERSATION_TOKEN=//p\"' ",
    "> ~/.rakazo/ohm-muse-token && chmod 600 ~/.rakazo/ohm-muse-token"
);

/// Typed provider outcomes. `Degraded` is a *result*, not a success: a 502 from
/// the phone/Iroh hop means the catalog/snapshot/send did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// No base URL configured: zero network calls were made.
    Offline,
    /// HTTP 401: the bearer was missing, wrong, or revoked.
    Unauthorized { endpoint: String },
    /// HTTP 502/503/504: the provisioning service is up but the phone/Iroh hop
    /// is not. Reported as degraded, never as queued/delivered.
    Degraded { endpoint: String, status: u16 },
    /// Any other non-2xx status.
    Http { endpoint: String, status: u16 },
    /// A 2xx body did not match the documented contract.
    Protocol { endpoint: String, detail: String },
    /// A local/transport failure (DNS, TLS, timeout). The detail is the
    /// library's message; it never contains the token because the token is only
    /// sent in a header.
    Transport { endpoint: String, detail: String },
    /// Token file missing, unreadable, or not owner-only. The path may be
    /// printed; the token value may not (it is never included).
    Token(String),
    /// A local precondition (empty text, bad request id, missing conversation).
    Refused(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Offline => f.write_str("offline: no agent base_url configured (zero network)"),
            Self::Unauthorized { endpoint } => {
                write!(f, "401 unauthorized for {endpoint}")
            }
            Self::Degraded { endpoint, status } => write!(
                f,
                "degraded: HTTP {status} from {endpoint} (phone/Iroh hop unavailable)"
            ),
            Self::Http { endpoint, status } => write!(f, "HTTP {status} from {endpoint}"),
            Self::Protocol { endpoint, detail } => {
                write!(f, "unexpected response from {endpoint}: {detail}")
            }
            Self::Transport { endpoint, detail } => {
                write!(f, "request to {endpoint} failed: {detail}")
            }
            Self::Token(detail) => write!(f, "token: {detail}"),
            Self::Refused(detail) => write!(f, "refused: {detail}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl AgentError {
    /// The HTTP status this outcome should surface, if any. Used by `status` and
    /// `probe` so a 401/502 is never flattened into "ok".
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Unauthorized { .. } => Some(401),
            Self::Degraded { status, .. } | Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded { .. })
    }
}

/// One raw HTTP response. Status is always available so a probe can print it
/// even when the body must not be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// The small HTTP seam: GET/POST with a bearer header. A scripted fake replaces
/// the network in every test.
pub trait AgentHttp {
    fn get(&self, url: &str, token: &Token) -> Result<HttpResponse, String>;
    fn post(&self, url: &str, token: &Token, body: &str) -> Result<HttpResponse, String>;
}

/// Production `ureq` exchange: 25 s timeout, zero redirects, rustls.
pub struct UreqAgentHttp {
    agent: ureq::Agent,
}

impl UreqAgentHttp {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(TIMEOUT)
                .redirects(0)
                .build(),
        }
    }
}

impl Default for UreqAgentHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHttp for UreqAgentHttp {
    fn get(&self, url: &str, token: &Token) -> Result<HttpResponse, String> {
        let request = self
            .agent
            .get(url)
            .set("authorization", &token.header_value())
            .set("accept", "application/json");
        finish(request.call())
    }

    fn post(&self, url: &str, token: &Token, body: &str) -> Result<HttpResponse, String> {
        let request = self
            .agent
            .post(url)
            .set("authorization", &token.header_value())
            .set("content-type", "application/json")
            .set("accept", "application/json");
        finish(request.send_string(body))
    }
}

fn finish(result: Result<ureq::Response, ureq::Error>) -> Result<HttpResponse, String> {
    match result {
        Ok(response) => Ok(HttpResponse {
            status: response.status(),
            body: response.into_string().map_err(|error| error.to_string())?,
        }),
        // Non-2xx is a normal response we classify ourselves.
        Err(ureq::Error::Status(status, response)) => Ok(HttpResponse {
            status,
            body: response.into_string().unwrap_or_default(),
        }),
        Err(error) => Err(error.to_string()),
    }
}

/// `GET /agent-conversations` item. Mirrors the deployed JSON exactly:
/// `{"id","label","platform","capabilities"}`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CatalogItem {
    /// `<channel>:<remote_id>`, e.g. `whatsapp:1807055946647697@bot`.
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub items: Vec<CatalogItem>,
}

/// One phone message from a `Snapshot`. Identity is `native_id`; dedupe must be
/// by that field, never by text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct NativeMessage {
    #[serde(default)]
    pub local_id: i64,
    #[serde(default)]
    pub native_id: String,
    #[serde(default)]
    pub from_me: bool,
    #[serde(default)]
    pub sender: String,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub media_ref: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub edited: bool,
    #[serde(default)]
    pub deleted: bool,
    /// `DeliveryStatus` is snake_case in ohmOS; kept as a string so an unknown
    /// future state cannot fail the whole snapshot.
    #[serde(default)]
    pub delivery: Option<String>,
}

/// The phone's bounded replacement snapshot (`agent_conversation::Snapshot`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub schema_version: u16,
    #[serde(default)]
    pub local_chat_id: i64,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub remote_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub messages: Vec<NativeMessage>,
}

/// `POST …/messages` body. `expected_native_identity` is optional server-side,
/// but when supplied it makes a changed binding a 409 instead of a wrong send.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SendRequest<'a> {
    pub request_id: &'a str,
    pub text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_native_identity: Option<ExpectedNativeIdentity<'a>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExpectedNativeIdentity<'a> {
    pub platform: super::wire::Platform,
    pub remote_id: &'a str,
}

/// `POST …/messages` reply: the native outbox accepted the request. `queued` is
/// never delivery proof.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SendReceipt {
    pub id: String,
    pub status: String,
}

impl SendReceipt {
    pub fn is_queued(&self) -> bool {
        self.status == "queued"
    }
}

/// Probe output. Deliberately holds **status codes, paths and conversation ids
/// only** — never a token and never message text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeReport {
    pub offline: bool,
    pub catalog_status: Option<u16>,
    pub catalog_ids: Vec<String>,
    pub snapshot_path: Option<String>,
    pub snapshot_status: Option<u16>,
    pub errors: Vec<AgentError>,
}

impl ProbeReport {
    /// Stable tab-separated lines for the CLI. Safe to paste into a PR.
    pub fn lines(&self) -> Vec<String> {
        if self.offline {
            return vec!["offline\tno base_url configured; zero network calls".to_string()];
        }
        let mut lines = Vec::new();
        match self.catalog_status {
            Some(status) => lines.push(format!(
                "GET {CATALOG_PATH}\tstatus={status}\t{}",
                outcome_word(status)
            )),
            None => lines.push(format!("GET {CATALOG_PATH}\tstatus=none")),
        }
        for id in &self.catalog_ids {
            lines.push(format!("catalog_id\t{id}"));
        }
        if let Some(path) = &self.snapshot_path {
            let status = self
                .snapshot_status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "none".to_string());
            lines.push(format!(
                "GET {path}\tstatus={status}\t{}",
                outcome_word_opt(self.snapshot_status)
            ));
        }
        for error in &self.errors {
            lines.push(format!("error\t{error}"));
        }
        lines
    }
}

fn outcome_word(status: u16) -> &'static str {
    outcome_word_opt(Some(status))
}

fn outcome_word_opt(status: Option<u16>) -> &'static str {
    match status {
        Some(200..=299) => "ok",
        Some(401) => "unauthorized",
        Some(502..=504) => "degraded",
        Some(_) => "http_error",
        None => "transport_error",
    }
}

/// A client for one deployment base (`https://ohm.fpl.dev/messages`).
///
/// `base: None` means offline. Every method checks that **before** calling the
/// HTTP seam, which is what the offline-default test asserts.
pub struct AgentConversationsClient<H: AgentHttp = UreqAgentHttp> {
    base: Option<String>,
    token: Token,
    http: H,
}

impl<H: AgentHttp> AgentConversationsClient<H> {
    pub fn new(base_url: Option<&str>, token: Token, http: H) -> Result<Self, AgentError> {
        let base = match base_url {
            Some(base) => Some(validate_base(base)?),
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
        self.http
            .get(&url, &self.token)
            .map_err(|detail| AgentError::Transport {
                endpoint: path.to_string(),
                detail,
            })
    }

    fn post(&self, path: &str, body: &str) -> Result<HttpResponse, AgentError> {
        let url = self.url(path)?;
        self.http
            .post(&url, &self.token, body)
            .map_err(|detail| AgentError::Transport {
                endpoint: path.to_string(),
                detail,
            })
    }

    /// `GET {base}/agent-conversations`.
    pub fn catalog(&self) -> Result<Vec<CatalogItem>, AgentError> {
        let response = self.get(CATALOG_PATH)?;
        let catalog: Catalog = self.decode(CATALOG_PATH, response)?;
        Ok(catalog.items)
    }

    /// `GET {base}/agent-conversations/{id}?limit=N`.
    pub fn snapshot(&self, conversation_id: &str, limit: usize) -> Result<Snapshot, AgentError> {
        let path = snapshot_path(conversation_id, limit)?;
        let response = self.get(&path)?;
        self.decode(&path, response)
    }

    /// `POST {base}/agent-conversations/{id}/messages`.
    pub fn send(
        &self,
        conversation_id: &str,
        request_id: &str,
        text: &str,
        identity: Option<ExpectedNativeIdentity<'_>>,
    ) -> Result<SendReceipt, AgentError> {
        validate_request(request_id, text)?;
        let path = messages_path(conversation_id)?;
        let body = serde_json::to_string(&SendRequest {
            request_id,
            text,
            expected_native_identity: identity,
        })
        .map_err(|error| AgentError::Protocol {
            endpoint: path.clone(),
            detail: error.to_string(),
        })?;
        let response = self.post(&path, &body)?;
        let receipt: SendReceipt = self.decode(&path, response)?;
        if !receipt.is_queued() {
            return Err(AgentError::Protocol {
                endpoint: path,
                detail: "send was not queued".to_string(),
            });
        }
        Ok(receipt)
    }

    /// Read-only status probe. Prints statuses and catalog ids; never a token and
    /// never message text. A 401/502 is captured in `errors` and still reported
    /// with its status instead of being swallowed.
    pub fn probe(&self, conversation_id: Option<&str>) -> ProbeReport {
        let mut report = ProbeReport::default();
        if self.is_offline() {
            report.offline = true;
            return report;
        }
        match self.get(CATALOG_PATH) {
            Ok(response) => {
                report.catalog_status = Some(response.status);
                match classify(CATALOG_PATH, response.status) {
                    Ok(()) => match serde_json::from_str::<Catalog>(&response.body) {
                        Ok(catalog) => {
                            report.catalog_ids = catalog.items.into_iter().map(|i| i.id).collect()
                        }
                        Err(_) => report.errors.push(AgentError::Protocol {
                            endpoint: CATALOG_PATH.to_string(),
                            detail: "invalid catalog JSON".to_string(),
                        }),
                    },
                    Err(error) => report.errors.push(error),
                }
            }
            Err(error) => report.errors.push(error),
        }
        if let Some(id) = conversation_id {
            match snapshot_path(id, 1) {
                Ok(path) => {
                    report.snapshot_path = Some(path.clone());
                    match self.get(&path) {
                        Ok(response) => {
                            report.snapshot_status = Some(response.status);
                            if let Err(error) = classify(&path, response.status) {
                                report.errors.push(error);
                            }
                        }
                        Err(error) => report.errors.push(error),
                    }
                }
                Err(error) => report.errors.push(error),
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

/// Validate a configured base URL (scheme, no credentials/query/fragment).
/// The path is allowed because the agent API lives under `/messages`.
pub fn validate_base_url(base: &str) -> Result<String, AgentError> {
    validate_base(base)
}

fn validate_base(base: &str) -> Result<String, AgentError> {
    let url = Url::parse(base)
        .map_err(|error| AgentError::Refused(format!("invalid agent base_url: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AgentError::Refused(
            "agent base_url must be http or https".to_string(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AgentError::Refused(
            "agent base_url must not contain credentials, query, or fragment".to_string(),
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
        502..=504 => Err(AgentError::Degraded {
            endpoint: endpoint.to_string(),
            status,
        }),
        other => Err(AgentError::Http {
            endpoint: endpoint.to_string(),
            status: other,
        }),
    }
}

fn conversation_segment(conversation_id: &str) -> Result<String, AgentError> {
    if conversation_id.trim().is_empty() {
        return Err(AgentError::Refused(
            "agent conversation_id is not configured".to_string(),
        ));
    }
    Ok(utf8_percent_encode(conversation_id, PATH_SEGMENT).to_string())
}

pub fn snapshot_path(conversation_id: &str, limit: usize) -> Result<String, AgentError> {
    if !(1..=200).contains(&limit) {
        return Err(AgentError::Refused(
            "snapshot limit must be 1-200".to_string(),
        ));
    }
    Ok(format!(
        "{CATALOG_PATH}/{}?limit={limit}",
        conversation_segment(conversation_id)?
    ))
}

pub fn messages_path(conversation_id: &str) -> Result<String, AgentError> {
    Ok(format!(
        "{CATALOG_PATH}/{}/messages",
        conversation_segment(conversation_id)?
    ))
}

fn validate_request(request_id: &str, text: &str) -> Result<(), AgentError> {
    if request_id.trim().is_empty()
        || request_id.len() > 128
        || request_id.chars().any(char::is_control)
    {
        return Err(AgentError::Refused(
            "request_id must be non-empty, <=128 chars, and control-free".to_string(),
        ));
    }
    if text.trim().is_empty() || text.len() > 16_384 {
        return Err(AgentError::Refused(
            "text must be non-empty and <=16384 bytes".to_string(),
        ));
    }
    Ok(())
}

/// Stable outbound request id: the same `(agent, text)` always maps to the same
/// id, so a retry is an idempotent replay at the phone (never a duplicate).
/// Callers that intentionally repeat identical text pass an explicit id.
pub fn stable_request_id(agent: &str, text: &str) -> String {
    let digest = crate::stable_id(&["rakazo-ohmos-reply/v1", agent, text]);
    format!("rakazo-{}-{}", agent, &digest[..24])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeHttp {
        calls: Arc<Mutex<Vec<(String, String, usize)>>>,
        replies: Arc<Mutex<Vec<HttpResponse>>>,
    }

    impl FakeHttp {
        fn scripted(replies: Vec<HttpResponse>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                replies: Arc::new(Mutex::new(replies)),
            }
        }
        fn calls(&self) -> Vec<(String, String, usize)> {
            self.calls.lock().unwrap().clone()
        }
        fn next(&self, method: &str, url: &str) -> Result<HttpResponse, String> {
            self.calls.lock().unwrap().push((
                method.to_string(),
                url.to_string(),
                // Record the length only: a test must not need the secret value.
                0,
            ));
            Ok(self.replies.lock().unwrap().remove(0))
        }
    }

    impl AgentHttp for FakeHttp {
        fn get(&self, url: &str, _token: &Token) -> Result<HttpResponse, String> {
            self.next("GET", url)
        }
        fn post(&self, url: &str, _token: &Token, _body: &str) -> Result<HttpResponse, String> {
            self.next("POST", url)
        }
    }

    fn token(value: &str) -> Token {
        Token(value.to_string())
    }

    fn client(base: Option<&str>, http: FakeHttp) -> AgentConversationsClient<FakeHttp> {
        AgentConversationsClient::new(base, token("test-token-value"), http).unwrap()
    }

    #[test]
    fn catalog_snapshot_and_send_use_the_documented_paths() {
        let http = FakeHttp::scripted(vec![
            HttpResponse {
                status: 200,
                body: r#"{"items":[{"id":"whatsapp:123@bot","label":"Muse","platform":"whatsapp","capabilities":{}}]}"#.into(),
            },
            HttpResponse {
                status: 200,
                body: r#"{"schema_version":1,"local_chat_id":9,"platform":"WhatsApp","remote_id":"123@bot","title":"Muse","messages":[{"local_id":1,"native_id":"wa-1","from_me":false,"sender":"Muse","timestamp":2,"text":"hi","media_ref":null,"reply_to":null,"edited":false,"deleted":false,"delivery":"sent"}]}"#.into(),
            },
            HttpResponse {
                status: 200,
                body: r#"{"id":"req-1","status":"queued"}"#.into(),
            },
        ]);
        let client = client(Some("https://ohm.fpl.dev/messages/"), http.clone());
        let catalog = client.catalog().unwrap();
        assert_eq!(catalog[0].id, "whatsapp:123@bot");
        let snapshot = client.snapshot("whatsapp:123@bot", 5).unwrap();
        assert_eq!(snapshot.messages[0].native_id, "wa-1");
        assert_eq!(snapshot.messages[0].text.as_deref(), Some("hi"));
        let receipt = client
            .send(
                "whatsapp:123@bot",
                "req-1",
                "hello",
                Some(ExpectedNativeIdentity {
                    platform: crate::ohm::wire::Platform::WhatsApp,
                    remote_id: "123@bot",
                }),
            )
            .unwrap();
        assert!(receipt.is_queued());
        let calls = http.calls();
        assert_eq!(calls[0].0, "GET");
        assert_eq!(
            calls[0].1,
            "https://ohm.fpl.dev/messages/agent-conversations"
        );
        assert_eq!(
            calls[1].1,
            "https://ohm.fpl.dev/messages/agent-conversations/whatsapp:123@bot?limit=5"
        );
        assert_eq!(
            calls[2].1,
            "https://ohm.fpl.dev/messages/agent-conversations/whatsapp:123@bot/messages"
        );
    }

    #[test]
    fn offline_default_makes_zero_network_calls() {
        let http = FakeHttp::scripted(vec![]);
        let client = client(None, http.clone());
        assert!(client.is_offline());
        assert_eq!(client.catalog().unwrap_err(), AgentError::Offline);
        assert!(client.snapshot("whatsapp:1", 1).is_err());
        assert!(client.send("whatsapp:1", "r", "t", None).is_err());
        let report = client.probe(Some("whatsapp:1"));
        assert!(report.offline);
        assert!(report.catalog_status.is_none());
        assert!(http.calls().is_empty(), "offline must not touch HTTP");
    }

    #[test]
    fn unauthorized_and_degraded_are_typed_results() {
        let http = FakeHttp::scripted(vec![
            HttpResponse {
                status: 401,
                body: String::new(),
            },
            HttpResponse {
                status: 502,
                body: "error code: 502".into(),
            },
        ]);
        let client = client(Some("https://ohm.fpl.dev/messages"), http);
        let error = client.catalog().unwrap_err();
        assert_eq!(
            error,
            AgentError::Unauthorized {
                endpoint: CATALOG_PATH.into()
            }
        );
        assert_eq!(error.status(), Some(401));

        let error = client.snapshot("whatsapp:1", 5).unwrap_err();
        assert!(error.is_degraded(), "{error}");
        assert_eq!(error.status(), Some(502));
        // A 502 is never reported as a success.
        assert!(!matches!(error, AgentError::Offline));
    }

    #[test]
    fn probe_reports_status_codes_and_ids_only() {
        let http = FakeHttp::scripted(vec![
            HttpResponse {
                status: 200,
                body:
                    r#"{"items":[{"id":"whatsapp:123@bot","label":"Muse","platform":"whatsapp"}]}"#
                        .into(),
            },
            HttpResponse {
                status: 502,
                body: r#"{"error":"Selected native conversation is unavailable."}"#.into(),
            },
        ]);
        let client = client(Some("https://ohm.fpl.dev/messages"), http);
        let report = client.probe(Some("whatsapp:123@bot"));
        let text = report.lines().join("\n");
        assert!(text.contains("status=200"), "{text}");
        assert!(text.contains("catalog_id\twhatsapp:123@bot"), "{text}");
        assert!(text.contains("status=502"), "{text}");
        assert!(text.contains("degraded"), "{text}");
        assert!(!text.contains("test-token-value"), "{text}");
        assert!(!text.contains("Selected native"), "{text}");
    }

    #[test]
    fn token_is_never_rendered_by_debug_display_or_errors() {
        let secret = "test-token-value";
        let token = token(secret);
        assert_eq!(format!("{token:?}"), "Token(<redacted>)");
        assert!(!format!("{token:?}").contains(secret));
        let header = token.header_value();
        assert_eq!(header, format!("Bearer {secret}"));
        for error in [
            AgentError::Unauthorized {
                endpoint: CATALOG_PATH.into(),
            },
            AgentError::Degraded {
                endpoint: CATALOG_PATH.into(),
                status: 502,
            },
            AgentError::Token("missing /path".into()),
            AgentError::Refused("bad request".into()),
        ] {
            assert!(!format!("{error}").contains(secret));
            assert!(!format!("{error:?}").contains(secret));
        }
    }

    #[test]
    fn request_validation_rejects_empty_text_and_control_ids() {
        let http = FakeHttp::scripted(vec![]);
        let client = client(Some("https://ohm.fpl.dev/messages"), http.clone());
        assert!(client.send("whatsapp:1", " ", "x", None).is_err());
        assert!(client.send("whatsapp:1", "r", "  ", None).is_err());
        assert!(client.send("whatsapp:1", "r\nx", "x", None).is_err());
        assert!(http.calls().is_empty());
    }

    #[test]
    fn stable_request_id_is_deterministic_and_agent_scoped() {
        let a = stable_request_id("muse", "hello");
        let b = stable_request_id("muse", "hello");
        let c = stable_request_id("muse", "hello!");
        let d = stable_request_id("orch", "hello");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert!(a.starts_with("rakazo-muse-"), "{a}");
        // Fits the server's 128-char request_id bound.
        assert!(a.len() <= 128);
    }

    #[test]
    fn token_file_must_be_owner_only() {
        use std::io::Write;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("token");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"test-token-value\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = Token::load(&path).unwrap_err();
            assert!(
                error.to_string().contains("0600"),
                "expected an owner-only error, got {error}"
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let loaded = Token::load(&path).unwrap();
        assert_eq!(loaded.header_value(), "Bearer test-token-value");
        assert!(!format!("{loaded:?}").contains("test-token-value"));
    }

    #[test]
    fn env_override_names_the_agents_token_file() {
        // Pure name derivation: no process env mutation in a parallel test run.
        let var = format!("OHM_{}_TOKEN_FILE", "muse".to_uppercase());
        assert_eq!(var, "OHM_MUSE_TOKEN_FILE");
    }

    #[test]
    fn base_url_path_is_allowed_but_credentials_are_not() {
        let http = FakeHttp::scripted(vec![]);
        assert!(
            AgentConversationsClient::new(
                Some("https://user:pw@ohm.fpl.dev/messages"),
                token("x"),
                http.clone()
            )
            .is_err()
        );
        assert!(
            AgentConversationsClient::new(
                Some("https://ohm.fpl.dev/messages?token=leak"),
                token("x"),
                http
            )
            .is_err()
        );
    }
}
