//! ohmOS messaging client seam for rakazo-beeper.
//!
//! This module consumes the *existing* ohmOS contract: `GET /health`,
//! `GET /inbox`, `GET /thread/:id`, `POST /send`, `GET /events` (SSE). It does
//! not define a second protocol and it never talks to the Iroh/ohmphone path.
//!
//! Offline by default: there is no production base URL and `send` is refused
//! unless a config explicitly sets `allow_send = true`. See
//! `docs/ohm-config.example.toml` and `docs/ohm-client.md`.

pub mod bridge;
pub mod msgd;
pub mod transport;
pub mod wire;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub use bridge::{
    Bridge, BridgeState, CommandRunner, FakeNotifier, FakePaneIo, OrchestratorNotifier, PaneIo,
    PiNotifyNotifier, PollReport, ProcessRunner, ReplyReport, TmuxPaneIo, inbound,
};
pub use msgd::{HttpExchange, MsgdClient, UreqHttp};
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
