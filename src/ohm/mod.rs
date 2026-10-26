//! ohmOS messaging client seam for rakazo-beeper.
//!
//! This module consumes the *existing* ohmOS contract: `GET /health`,
//! `GET /inbox`, `GET /thread/:id`, `POST /send`, `GET /events` (SSE). It does
//! not define a second protocol and it never talks to the Iroh/ohmphone path.
//!
//! Two providers, one selection, no fallback:
//! - `transport = "msgd"` — the legacy daemon surface above (top-level
//!   `base_url`/`[[routes]]`).
//! - `transport = "ohmos"` — the self-hosted provisioning
//!   `agent-conversations` API used for Muse (`[[agent]]`).
//!
//! Offline by default: there is no production base URL and `send` is refused
//! unless a config explicitly sets `allow_send = true` (msgd) or an `[[agent]]`
//! base URL is present (ohmos). See `docs/ohm-config.example.toml` and
//! `docs/ohm-client.md`.

pub mod agent_conversations;
pub mod bridge;
pub mod msgd;
pub mod ohmos;
pub mod transport;
pub mod wire;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub use agent_conversations::{
    AgentConversationsClient, AgentError, AgentHttp, CatalogItem, ExpectedNativeIdentity,
    NativeMessage, ProbeReport, SendReceipt, Snapshot, Token, UreqAgentHttp, stable_request_id,
};
pub use bridge::{
    Bridge, BridgeState, CommandRunner, FakeNotifier, FakePaneIo, OrchestratorNotifier, PaneIo,
    PiNotifyNotifier, PollReport, ProcessRunner, ReplyReport, TmuxPaneIo, inbound,
};
pub use msgd::{HttpExchange, MsgdClient, UreqHttp};
pub use ohmos::{
    AgentBridge, AgentDelivery, AgentInbound, AgentOutbound, AgentPollReport, AgentReplyReport,
    AgentSendOutcome, AgentState, AgentTransport, FakeAgentTransport, FakeDelivery, OhmosTransport,
    PiNotifyDelivery, default_state_file, identity,
};
pub use transport::{FakeTransport, HttpTransport, Inbound, Outbound, SendOutcome, Transport};
pub use wire::{Chat, Msg, Platform, SendRequest, SendResponse};

/// One conversation → one roster pane. `roster_key` is checked against
/// [`crate::orch::roster`] so a typo can never invent an agent.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// ohmOS conversation id (`Chat::id`).
    pub chat_id: i64,
    /// A key from `orch::roster()`.
    pub roster_key: String,
    /// Explicit pane target. Required unless the CLI supplies `--target`.
    pub pane: Option<String>,
    /// Platform for `POST /send`.
    pub platform: Platform,
    /// Platform-native chat handle for `POST /send` (`SendRequest.chat_remote_id`).
    pub remote_id: String,
    /// Platform-native recipient for `POST /send`; defaults to `remote_id`.
    pub recipient: Option<String>,
}

/// Client configuration, read from a TOML file (never from production env).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OhmConfig {
    /// No default: absent means the client cannot make any network call.
    pub base_url: Option<String>,
    /// `[[agent]]` providers (Slice B: the ohmOS agent-conversation one).
    /// Selecting `transport = "ohmos"` is explicit; there is no implicit
    /// fallback to the msgd provider or vice versa.
    #[serde(rename = "agent", default)]
    pub agents: Vec<AgentConfig>,
    /// Explicitly opt in to `POST /send`. Default is OFF.
    #[serde(default)]
    pub allow_send: bool,
    /// Small dedupe/ledger file. Absent means process-lifetime memory only.
    pub state_file: Option<PathBuf>,
    /// Panes that must never receive a paste. Defaults to the orchestrator pane.
    #[serde(default = "orchestrator_targets_default")]
    pub orchestrator_targets: Vec<String>,
    #[serde(default)]
    pub routes: Vec<Route>,
}

/// Which provider an `[[agent]]` entry selects. Explicit, never inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTransportKind {
    /// The self-hosted ohmOS agent-conversation API (this slice).
    Ohmos,
    /// The legacy ohm-msgd daemon. Configured through the top-level
    /// `base_url`/`[[routes]]`, not through an `[[agent]]` entry.
    Msgd,
}

/// One external agent on a native conversation.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Local agent key, e.g. `muse`. Also the default `pi-notify` worker target.
    pub key: String,
    /// Only `external` in this slice; no worker bots are created here.
    #[serde(default = "agent_kind_default")]
    pub kind: String,
    /// Explicit provider selection. Required: a missing field is a config error.
    pub transport: AgentTransportKind,
    /// `https://ohm.fpl.dev/messages` (pris/tailnet URLs are allowed). Absent
    /// means offline: zero network calls.
    pub base_url: Option<String>,
    /// Owner-only bearer file. `OHM_<KEY>_TOKEN_FILE` overrides it.
    pub token_file: Option<PathBuf>,
    /// The id returned by `GET /agent-conversations`, e.g.
    /// `whatsapp:1807055946647697@bot`.
    pub conversation_id: Option<String>,
    /// Native identity sent on outbound as a precondition.
    pub platform: Option<Platform>,
    pub remote_id: Option<String>,
    pub label: Option<String>,
    #[serde(default = "poll_seconds_default")]
    pub poll_seconds: u64,
    /// Roster `worker` delivery target; defaults to `key` until Slice A's
    /// roster contract exists.
    pub worker: Option<String>,
    /// Durable dedupe/idempotency ledger. Defaults to
    /// `$RAKAZO_BRIDGE_STATE_DIR` (or `~/.local/state/rakazo/bridge`)
    /// `<key>-ohmos.json`.
    pub state_file: Option<PathBuf>,
    #[serde(default = "snapshot_limit_default")]
    pub limit: usize,
}

fn agent_kind_default() -> String {
    "external".to_string()
}

fn poll_seconds_default() -> u64 {
    5
}

fn snapshot_limit_default() -> usize {
    50
}

impl AgentConfig {
    /// The effective worker target. `muse` for the current single-agent path.
    pub fn worker(&self) -> &str {
        self.worker.as_deref().unwrap_or(&self.key)
    }

    pub fn poll_seconds(&self) -> u64 {
        self.poll_seconds.max(1)
    }

    pub fn token_path(&self) -> Result<PathBuf> {
        if let Some(path) = agent_conversations::token_file_from_env(&self.key) {
            return Ok(path);
        }
        self.token_file.clone().with_context(|| {
            format!(
                "agent {} has no token_file and OHM_{}_TOKEN_FILE is unset",
                self.key,
                self.key.to_uppercase().replace('-', "_")
            )
        })
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.key.trim().is_empty(), "agent key must not be empty");
        ensure!(
            self.kind == "external",
            "agent {} kind must be \"external\" (worker bots are Slice A)",
            self.key
        );
        match self.transport {
            AgentTransportKind::Ohmos => {}
            AgentTransportKind::Msgd => bail!(
                "agent {} selects transport = \"msgd\"; use the top-level base_url/[[routes]] \
                 msgd configuration instead (no implicit fallback)",
                self.key
            ),
        }
        if let Some(base) = &self.base_url {
            agent_conversations::validate_base_url(base)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        }
        ensure!(
            self.poll_seconds >= 1,
            "agent {} poll_seconds must be positive",
            self.key
        );
        ensure!(
            (1..=200).contains(&self.limit),
            "agent {} limit must be 1-200",
            self.key
        );
        ensure!(
            !self.worker().trim().is_empty(),
            "agent {} worker target must not be empty",
            self.key
        );
        Ok(())
    }
}

impl OhmConfig {
    /// Select one agent, or all when `key` is `None`. An unknown key is an
    /// error: a typo must never silently select nothing.
    pub fn agents<'a>(&'a self, key: Option<&str>) -> Result<Vec<&'a AgentConfig>> {
        match key {
            None => Ok(self.agents.iter().collect()),
            Some(key) => {
                let agent = self
                    .agents
                    .iter()
                    .find(|agent| agent.key == key)
                    .with_context(|| format!("no [[agent]] with key {key}"))?;
                Ok(vec![agent])
            }
        }
    }
}

fn orchestrator_targets_default() -> Vec<String> {
    vec!["%0".to_string()]
}

impl OhmConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read ohm config {}", path.display()))?;
        let config: Self = toml::from_str(&text).context("invalid ohm client configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(base) = &self.base_url {
            MsgdClient::<UreqHttp>::validate_base(base)?;
        }
        let mut agent_keys = HashSet::new();
        for agent in &self.agents {
            ensure!(
                agent_keys.insert(agent.key.as_str()),
                "duplicate [[agent]] key"
            );
            agent.validate()?;
        }
        ensure!(
            !self.orchestrator_targets.is_empty(),
            "ohm orchestrator_targets must not be empty (default is [\"%0\"])"
        );
        for target in &self.orchestrator_targets {
            ensure!(
                !target.trim().is_empty(),
                "ohm orchestrator target must not be empty"
            );
        }
        let mut ids = HashSet::new();
        // The orchestrator key is static; PM keys come from verticals.toml.
        // A missing file (CI, fresh host) must not reject an orchestrator route,
        // but it must still reject any PM key we cannot verify.
        let entries =
            crate::orch::roster().unwrap_or_else(|_| vec![crate::orch::orchestrator_entry()]);
        let roster: HashSet<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        for route in &self.routes {
            ensure!(ids.insert(route.chat_id), "duplicate ohm route chat_id");
            ensure!(
                roster.contains(route.roster_key.as_str()),
                "ohm route roster_key is not in orch::roster()"
            );
            ensure!(
                route
                    .pane
                    .as_deref()
                    .is_none_or(|pane| !pane.trim().is_empty()),
                "ohm route pane must not be empty"
            );
            ensure!(
                route
                    .recipient
                    .as_deref()
                    .is_none_or(|recipient| !recipient.trim().is_empty()),
                "ohm route recipient must not be empty"
            );
        }
        Ok(())
    }
}
