//! Bridge configuration, per-agent service tokens, provisioning and replies.
//!
//! This is the multi-agent replacement for the one-allowlist watch. Nothing in
//! this module guesses a fleet shape: the visible agents come from
//! [`super::agentsmith`], and a roster command failure is fatal (there is no
//! "just orch" fallback).
//!
//! ## Credentials
//!
//! Every visible Rakazo bot gets its **own** bot-bound service token, written
//! owner-only (`0600`) to `service_token_dir/<safe-key>.token`. The owner
//! session is used once, to issue tokens; it never sends a bot reply. A reply
//! reads exactly the token for the requested agent and calls
//! `threads.appendAssistant` with it, so one PM cannot send as another bot.
//!
//! Raw tokens are never printed, logged, or placed in argv: provisioning prints
//! only the bot id and the token file path.
//!
//! ## Config (example committed as `config/agentsmith.example.toml`)
//!
//! ```toml
//! [agentsmith]
//! roster_command = ["pi-agentsmith", "once", "--json", "--no-remote", "--no-caps"]
//!
//! [bridge]
//! state_dir = "/home/ajmwagar/.local/share/rakazo/bridge"
//! service_token_dir = "/home/ajmwagar/.rakazo/tokens"
//! notify_kind = "steer"
//! notify_urgent = true
//! hide_workers = true
//! ```

use super::agentsmith::{Agent, AgentRole, Roster, resolve_bot};
use crate::rakazo::{AppendReceipt, Bot, Rakazo};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    future::Future,
    path::{Path, PathBuf},
};
use url::Url;

/// Committed example path, and the default for `bridge --bridge-config`.
pub const DEFAULT_CONFIG: &str = "config/agentsmith.toml";

fn default_state_dir() -> PathBuf {
    match std::env::var_os("RAKAZO_BRIDGE_STATE_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".local/share/rakazo/bridge"),
            None => PathBuf::from(".rakazo/bridge"),
        },
    }
}

fn default_service_token_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".rakazo/tokens"),
        None => PathBuf::from(".rakazo/tokens"),
    }
}

fn default_roster_command() -> Vec<String> {
    [
        "pi-agentsmith",
        "once",
        "--json",
        "--no-remote",
        "--no-caps",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn notify_kind_default() -> String {
    "steer".to_string()
}

fn notify_urgent_default() -> bool {
    true
}

fn hide_workers_default() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsmithSection {
    /// The command whose stdout is the roster JSON. Failing is fatal.
    #[serde(default = "default_roster_command")]
    pub roster_command: Vec<String>,
}

impl Default for AgentsmithSection {
    fn default() -> Self {
        Self {
            roster_command: default_roster_command(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeSection {
    /// Explicit local identity. A supervisor owns only roster rows whose `host`
    /// equals this value. There is no hostname guessing and no default.
    #[serde(default)]
    pub host: Option<String>,
    /// One `WatchState` per agent lives here, under a host subdirectory.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// One owner-only service token per bot lives here.
    #[serde(default = "default_service_token_dir")]
    pub service_token_dir: PathBuf,
    /// `pi-notify --kind`.
    #[serde(default = "notify_kind_default")]
    pub notify_kind: String,
    /// `pi-notify --urgent`.
    #[serde(default = "notify_urgent_default")]
    pub notify_urgent: bool,
    /// Local default. If the roster payload carries `bridge.hide_workers`, that
    /// value wins (one knob, documented in `docs/agent-bridge.md`).
    #[serde(default = "hide_workers_default")]
    pub hide_workers: bool,
    /// Optional ohm/beeper `[[agent]]` config. When set, `bridge status` reports
    /// each matching agent's real transport and a live Beeper probe instead of
    /// assuming Rakazo-only polling. No URL, no token value: a path only.
    #[serde(default)]
    pub agent_config: Option<PathBuf>,
}

impl Default for BridgeSection {
    fn default() -> Self {
        Self {
            host: None,
            state_dir: default_state_dir(),
            service_token_dir: default_service_token_dir(),
            notify_kind: notify_kind_default(),
            notify_urgent: notify_urgent_default(),
            hide_workers: hide_workers_default(),
            agent_config: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    #[serde(default)]
    pub agentsmith: AgentsmithSection,
    #[serde(default)]
    pub bridge: BridgeSection,
}

impl BridgeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read bridge config {}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("invalid bridge config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.agentsmith.roster_command.is_empty(),
            "agentsmith.roster_command must not be empty"
        );
        ensure!(
            !self.agentsmith.roster_command[0].trim().is_empty(),
            "agentsmith.roster_command program must not be empty"
        );
        ensure!(
            !self.bridge.notify_kind.trim().is_empty(),
            "bridge.notify_kind must not be empty"
        );
        ensure!(
            !self.bridge.state_dir.as_os_str().is_empty(),
            "bridge.state_dir must not be empty"
        );
        ensure!(
            !self.bridge.service_token_dir.as_os_str().is_empty(),
            "bridge.service_token_dir must not be empty"
        );
        ensure!(
            self.bridge
                .agent_config
                .as_deref()
                .is_none_or(|path| !path.as_os_str().is_empty()),
            "bridge.agent_config must not be empty when set"
        );
        Ok(())
    }

    /// The local host identity. `[bridge] host` wins; `PI_AGENT_HOST` is the
    /// only alternative. A missing value is an explicit error — never the
    /// machine's hostname and never an empty wildcard.
    pub fn host(&self) -> Result<String> {
        let host = self
            .bridge
            .host
            .as_deref()
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var("PI_AGENT_HOST")
                    .ok()
                    .map(|host| host.trim().to_string())
                    .filter(|host| !host.is_empty())
            });
        host.context(
            "bridge host is not set: add `host = \"<agentsmith host>\"` under [bridge] or set PI_AGENT_HOST (never guessed from the machine name)",
        )
    }

    /// The upstream hint wins when present; the local default applies otherwise.
    pub fn effective_hide_workers(&self, roster: &Roster) -> bool {
        roster.hide_workers.unwrap_or(self.bridge.hide_workers)
    }
}

/// Owner-only, per-agent token files.
#[derive(Clone, Debug)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, key: &str) -> Result<PathBuf> {
        Ok(self
            .dir
            .join(format!("{}.token", super::agentsmith::safe_key(key)?)))
    }

    /// Read one agent's token. A missing file, an empty file, or a file with
    /// group/other permissions is an error: this never falls back to another
    /// agent's token or to the owner session.
    pub fn read(&self, key: &str) -> Result<String> {
        let path = self.path(key)?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read service token for {key} at {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
                "service token for {key} at {} must be owner-only (0600)",
                path.display()
            );
        }
        let token = text.trim().to_string();
        ensure!(
            !token.is_empty(),
            "service token for {key} at {} is empty",
            path.display()
        );
        Ok(token)
    }

    pub fn exists(&self, key: &str) -> bool {
        self.path(key).map(|path| path.exists()).unwrap_or(false)
    }

    /// Write one agent's token atomically, owner-only from creation.
    pub fn write(&self, key: &str, token: &str) -> Result<PathBuf> {
        ensure!(!token.trim().is_empty(), "refusing to write an empty token");
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create token dir {}", parent.display()))?;
        }
        let tmp = path.with_extension("tmp");
        write_secret(&tmp, token)?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("replace service token {}", path.display()))?;
        set_owner_only(&path)?;
        Ok(path)
    }
}

fn write_secret(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("write secret {}", path.display()))?;
    file.write_all(text.trim().as_bytes())
        .with_context(|| format!("write secret {}", path.display()))?;
    file.write_all(b"\n")?;
    Ok(())
}

fn set_owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

/// Issues a bot-bound service token from the owner session.
///
/// Production is [`SessionIssuer`]; tests record the requested binding. The raw
/// token is returned once and immediately written owner-only by the caller.
pub trait TokenIssuer {
    fn issue(&self, agent_key: &str, bot_id: &str) -> impl Future<Output = Result<String>> + Send;
}

/// Owner-session issuer: `POST /rpc/serviceTokens/issue` with the two scopes the
/// bridge needs (`threads:read`, `threads:write:assistant`).
#[derive(Clone)]
pub struct SessionIssuer {
    pub rakazo: Rakazo,
}

impl TokenIssuer for SessionIssuer {
    fn issue(&self, _agent_key: &str, bot_id: &str) -> impl Future<Output = Result<String>> + Send {
        let rakazo = self.rakazo.clone();
        let bot_id = bot_id.to_string();
        async move { Ok(rakazo.issue_service_token(&bot_id).await?.token) }
    }
}

/// Appends one assistant message with a bot-bound service token.
pub trait ServiceAppender {
    fn append(
        &self,
        token: &str,
        bot: &str,
        text: &str,
        nonce: &str,
    ) -> impl Future<Output = Result<AppendReceipt>> + Send;
}

/// Production appender: builds a one-shot service principal per call so no
/// token is kept in memory after the reply.
#[derive(Clone, Debug)]
pub struct RakazoAppender {
    pub base: Url,
    pub space: String,
}

impl ServiceAppender for RakazoAppender {
    fn append(
        &self,
        token: &str,
        bot: &str,
        text: &str,
        nonce: &str,
    ) -> impl Future<Output = Result<AppendReceipt>> + Send {
        let base = self.base.clone();
        let space = self.space.clone();
        let bot = bot.to_string();
        let text = text.to_string();
        let nonce = nonce.to_string();
        async move {
            let client = Rakazo::from_service_parts(base, token, &space)?;
            client.append_assistant(&bot, &text, &nonce, None).await
        }
    }
}

/// One provisioning decision. Never carries token material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provisioned {
    pub agent_key: String,
    pub bot_id: String,
    pub token_path: PathBuf,
    /// `false` means the token already existed and was left untouched.
    pub issued: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProvisionReport {
    pub provisioned: Vec<Provisioned>,
    pub skipped: Vec<(String, String)>,
}

/// Issue (or report) one token per visible bot.
///
/// * Muse is transport-backed (Slice B): no Rakazo bot, no token.
/// * PM children that are missing stay missing; this never fabricates a bot or
///   a token for one.
/// * When `hide_workers` filtered workers out of `agents`, no worker bot and no
///   worker token exists.
/// * An existing token file is never overwritten, so a token cannot silently
///   move to a new identity.
pub async fn provision<I: TokenIssuer>(
    agents: &[Agent],
    bots: &[Bot],
    store: &TokenStore,
    issuer: &I,
    apply: bool,
    only: Option<&str>,
) -> Result<ProvisionReport> {
    let mut report = ProvisionReport::default();
    for agent in agents {
        if let Some(only) = only
            && !agent.matches(only)
        {
            continue;
        }
        match agent.role {
            AgentRole::Muse => {
                report
                    .skipped
                    .push((agent.key.clone(), "transport=ohmos (Slice B)".into()));
                continue;
            }
            AgentRole::Pm | AgentRole::Worker | AgentRole::Orch => {}
        }
        let Some(bot) = resolve_bot(bots, agents, agent) else {
            let reason = match agent.role {
                AgentRole::Pm => "missing (orchestrator-spawned child; never faked)",
                _ => "missing (bot not created yet)",
            };
            report.skipped.push((agent.key.clone(), reason.into()));
            continue;
        };
        let path = store.path(&agent.key)?;
        if path.exists() {
            report.provisioned.push(Provisioned {
                agent_key: agent.key.clone(),
                bot_id: bot.id.clone(),
                token_path: path,
                issued: false,
            });
            continue;
        }
        if !apply {
            report.provisioned.push(Provisioned {
                agent_key: agent.key.clone(),
                bot_id: bot.id.clone(),
                token_path: path,
                issued: false,
            });
            continue;
        }
        let token = issuer.issue(&agent.key, &bot.id).await?;
        let path = store.write(&agent.key, &token)?;
        report.provisioned.push(Provisioned {
            agent_key: agent.key.clone(),
            bot_id: bot.id.clone(),
            token_path: path,
            issued: true,
        });
    }
    Ok(report)
}

/// Send one assistant reply as exactly one agent, with that agent's own token.
///
/// The bot must be the one the roster resolves for this agent; a caller cannot
/// pass another agent's bot id and have the reply silently sent as them.
// The binding is explicit rather than bundled in a struct: every argument is a
// separate safety boundary (store, appender, roster, bots, agent, bot, text,
// nonce) and callers are all internal.
#[allow(clippy::too_many_arguments)]
pub async fn reply<A: ServiceAppender>(
    store: &TokenStore,
    appender: &A,
    agents: &[Agent],
    bots: &[Bot],
    agent_key: &str,
    bot_id: &str,
    text: &str,
    nonce: &str,
) -> Result<AppendReceipt> {
    ensure!(!text.trim().is_empty(), "refusing to send an empty reply");
    let agent = agents
        .iter()
        .find(|agent| agent.matches(agent_key))
        .with_context(|| format!("no roster agent matches {agent_key}"))?;
    ensure!(
        agent.role != AgentRole::Muse,
        "muse is transport-backed (Slice B); `bridge reply` does not send as muse"
    );
    let bound = resolve_bot(bots, agents, agent)
        .with_context(|| format!("{} has no live Rakazo bot", agent.key))?;
    ensure!(
        bound.id == bot_id,
        "{} is bound to bot {}; refusing to reply as {}",
        agent.key,
        bound.id,
        bot_id
    );
    let token = store.read(&agent.key)?;
    appender.append(&token, bot_id, text, nonce).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orch::agentsmith::{Row, agents, parse};
    use crate::rakazo::Bot;
    use std::sync::Mutex;
    use tempfile::TempDir;

    const STUDIO: &str = "averys-mac-studio";

    fn row(name: &str, session: &str, pane: &str) -> Row {
        Row {
            name: name.into(),
            session: session.into(),
            host: STUDIO.into(),
            pane: Some(pane.into()),
            state: "working".into(),
            status: "working".into(),
            ..Row::default()
        }
    }

    fn bot(id: &str, name: &str, spawn: &str, parent: Option<&str>) -> Bot {
        Bot {
            id: id.into(),
            name: name.into(),
            color: String::new(),
            title: String::new(),
            description: String::new(),
            space_id: "space".into(),
            thread_id: Some(format!("thread-{id}")),
            parent_bot_id: parent.map(str::to_string),
            spawn_key: Some(spawn.into()),
            archived_at: None,
            capabilities: None,
            relay_only: None,
        }
    }

    #[test]
    fn config_is_strict_and_hide_workers_defaults_true() {
        let config: BridgeConfig = toml::from_str(
            r#"
            [agentsmith]
            roster_command = ["env", "true"]
            "#,
        )
        .unwrap();
        assert!(config.bridge.hide_workers);
        assert_eq!(config.bridge.notify_kind, "steer");
        assert!(config.bridge.notify_urgent);
        assert!(
            toml::from_str::<BridgeConfig>("[bridge]\nhide_workers = false\n[unknown]\nx = 1")
                .is_err(),
            "unknown sections are refused"
        );
        let empty: BridgeConfig = toml::from_str("[agentsmith]\nroster_command = []\n").unwrap();
        assert!(
            empty.validate().is_err(),
            "empty roster command must be refused"
        );
        assert!(toml::from_str::<BridgeConfig>("[agentsmith]\nroster_command = 7\n").is_err());
    }

    #[test]
    fn host_is_explicit_and_never_defaulted() {
        let config: BridgeConfig = toml::from_str(
            "[agentsmith]\nroster_command=[\"x\"]\n[bridge]\nhost = \"averys-mac-studio\"\n",
        )
        .unwrap();
        assert_eq!(config.host().unwrap(), STUDIO);
        assert_eq!(BridgeSection::default().host, None);
        if std::env::var_os("PI_AGENT_HOST").is_none() {
            let error = BridgeConfig::default().host().unwrap_err();
            assert!(
                format!("{error:#}").contains("host is not set"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn upstream_hide_workers_wins_over_the_local_default() {
        let config: BridgeConfig =
            toml::from_str("[agentsmith]\nroster_command=[\"x\"]\n").unwrap();
        let roster = parse(r#"{"workers":[],"bridge":{"hide_workers":false}}"#).unwrap();
        assert!(!config.effective_hide_workers(&roster));
        let roster = parse(r#"{"workers":[]}"#).unwrap();
        assert!(config.effective_hide_workers(&roster));
    }

    #[test]
    fn a_failing_roster_command_is_fatal_and_never_falls_back_to_orch() {
        use crate::orch::agentsmith::{FixedRoster, RosterSource};
        let source = FixedRoster::failing("roster command exited with 1");
        assert!(source.fetch().is_err());
        assert_eq!(
            agents(&[], true, STUDIO).unwrap().len(),
            2,
            "orch + muse only"
        );
    }

    #[test]
    fn token_files_are_owner_only_and_per_agent() {
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        let a = store.write("fleet-pm-fpl-v1", "token-a").unwrap();
        let b = store.write("fleet-pm-marbles-v1", "token-b").unwrap();
        assert_ne!(a, b);
        assert_eq!(store.read("fleet-pm-fpl-v1").unwrap(), "token-a");
        assert_eq!(store.read("fleet-pm-marbles-v1").unwrap(), "token-b");
        assert!(store.read("fleet-pm-absent-v1").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&a).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_group_readable_token_is_refused_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        let path = store.write("fleet-orch-v1", "token").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(store.read("fleet-orch-v1").is_err());
    }

    #[derive(Default)]
    struct RecordingIssuer {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl TokenIssuer for RecordingIssuer {
        fn issue(
            &self,
            agent_key: &str,
            bot_id: &str,
        ) -> impl Future<Output = Result<String>> + Send {
            self.calls
                .lock()
                .unwrap()
                .push((agent_key.to_string(), bot_id.to_string()));
            let token = format!("token-for-{agent_key}");
            async move { Ok(token) }
        }
    }

    #[derive(Default)]
    struct RecordingAppender {
        calls: Mutex<Vec<(String, String, String, String)>>,
    }

    impl ServiceAppender for RecordingAppender {
        fn append(
            &self,
            token: &str,
            bot: &str,
            text: &str,
            nonce: &str,
        ) -> impl Future<Output = Result<AppendReceipt>> + Send {
            self.calls.lock().unwrap().push((
                token.to_string(),
                bot.to_string(),
                text.to_string(),
                nonce.to_string(),
            ));
            let receipt = AppendReceipt {
                message_id: "m1".into(),
                seq: 1,
            };
            async move { Ok(receipt) }
        }
    }

    fn fixture() -> (Vec<Agent>, Vec<Bot>) {
        let rows = vec![
            row("pm-fpl", "pm-fpl", "%40"),
            row("pm-marbles", "pm-marbles", "%31"),
            row("fab-cicd", "fleet", "%112"),
        ];
        let orch = bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None);
        let mut bots = vec![orch.clone()];
        for (id, key) in [
            ("fpl-bot", "fleet-pm-fpl-v1"),
            ("marbles-bot", "fleet-pm-marbles-v1"),
        ] {
            let name = agents(&rows, true, STUDIO)
                .unwrap()
                .into_iter()
                .find(|agent| agent.key == key)
                .unwrap()
                .name;
            bots.push(bot(id, &name, "run:spawn_bot:hash", Some("orch-bot")));
        }
        let agents = agents(&rows, true, STUDIO).unwrap();
        (agents, bots)
    }

    #[tokio::test]
    async fn provisioning_issues_one_token_per_visible_bot_and_never_overwrites() {
        let (agents, bots) = fixture();
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        let issuer = RecordingIssuer::default();
        let report = provision(&agents, &bots, &store, &issuer, true, None)
            .await
            .unwrap();
        let keys: Vec<_> = report
            .provisioned
            .iter()
            .filter(|decision| decision.issued)
            .map(|decision| decision.agent_key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["fleet-orch-v1", "fleet-pm-fpl-v1", "fleet-pm-marbles-v1"]
        );
        // no worker token when workers are hidden, no muse token ever
        assert!(!store.exists("muse"));
        assert!(!store.exists("fleet-worker-fab-cicd-v1"));
        assert_eq!(
            store.read("fleet-pm-fpl-v1").unwrap(),
            "token-for-fleet-pm-fpl-v1"
        );

        // Second run issues nothing new and never overwrites an existing token.
        let report = provision(&agents, &bots, &store, &issuer, true, None)
            .await
            .unwrap();
        assert!(report.provisioned.iter().all(|decision| !decision.issued));
        assert_eq!(issuer.calls.lock().unwrap().len(), 3);
        assert_eq!(
            store.read("fleet-pm-fpl-v1").unwrap(),
            "token-for-fleet-pm-fpl-v1"
        );
    }

    #[tokio::test]
    async fn provisioning_can_be_limited_to_one_agent() {
        let (agents, bots) = fixture();
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        let issuer = RecordingIssuer::default();
        let report = provision(&agents, &bots, &store, &issuer, true, Some("pm:fpl"))
            .await
            .unwrap();
        assert_eq!(report.provisioned.len(), 1);
        assert_eq!(report.provisioned[0].agent_key, "fleet-pm-fpl-v1");
        assert!(store.exists("fleet-pm-fpl-v1"));
        assert!(!store.exists("fleet-pm-marbles-v1"));
        assert_eq!(issuer.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn provisioning_reports_missing_children_and_skips_muse() {
        let (agents, _) = fixture();
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        let issuer = RecordingIssuer::default();
        // No bots at all: orch is missing too, PMs are missing, muse is skipped.
        let report = provision(&agents, &[], &store, &issuer, true, None)
            .await
            .unwrap();
        assert!(report.provisioned.is_empty());
        assert!(issuer.calls.lock().unwrap().is_empty());
        assert!(
            report
                .skipped
                .iter()
                .any(|(key, reason)| key == "muse" && reason.contains("ohmos"))
        );
        assert!(
            report
                .skipped
                .iter()
                .any(|(key, reason)| key == "fleet-pm-fpl-v1" && reason.contains("missing"))
        );
    }

    #[tokio::test]
    async fn hide_workers_true_creates_no_worker_tokens() {
        let rows = vec![row("fab-cicd", "fleet", "%112")];
        let hidden = agents(&rows, true, STUDIO).unwrap();
        let shown = agents(&rows, false, STUDIO).unwrap();
        let orch = bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None);
        let worker = bot(
            "worker-bot",
            "fleet-worker-fab-cicd-v1",
            "fleet-worker-fab-cicd-v1",
            None,
        );
        let bots = vec![orch, worker];
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        let issuer = RecordingIssuer::default();
        provision(&hidden, &bots, &store, &issuer, true, None)
            .await
            .unwrap();
        assert!(!store.exists("fleet-worker-fab-cicd-v1"));
        provision(&shown, &bots, &store, &issuer, true, None)
            .await
            .unwrap();
        assert!(store.exists("fleet-worker-fab-cicd-v1"));
    }

    #[tokio::test]
    async fn reply_uses_only_the_agents_own_token_and_never_crosses_bots() {
        let (agents, bots) = fixture();
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        store.write("fleet-pm-fpl-v1", "token-fpl").unwrap();
        store.write("fleet-pm-marbles-v1", "token-marbles").unwrap();
        let appender = RecordingAppender::default();

        let receipt = reply(
            &store,
            &appender,
            &agents,
            &bots,
            "fleet-pm-fpl-v1",
            "fpl-bot",
            "hello",
            "nonce-1",
        )
        .await
        .unwrap();
        assert_eq!(receipt.seq, 1);
        let calls = appender.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "token-fpl", "used the FPL token only");
        assert_eq!(calls[0].1, "fpl-bot");

        // Asking for the FPL agent but naming the marbles bot is refused with no
        // call at all: one agent can never send as another bot.
        let error = reply(
            &store,
            &appender,
            &agents,
            &bots,
            "fleet-pm-fpl-v1",
            "marbles-bot",
            "wrong bot",
            "nonce-2",
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("refusing to reply as"),
            "{error:#}"
        );
        assert_eq!(appender.calls.lock().unwrap().len(), 1);

        // No token file for an agent means an error, not the owner session.
        let error = reply(
            &store,
            &appender,
            &agents,
            &bots,
            "fleet-orch-v1",
            "orch-bot",
            "hi",
            "nonce-3",
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("read service token"),
            "{error:#}"
        );
        assert_eq!(appender.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_agent_and_muse_have_no_reply_path() {
        let (agents, bots) = fixture();
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path());
        let appender = RecordingAppender::default();
        assert!(
            reply(&store, &appender, &agents, &bots, "pm-nope", "x", "t", "n")
                .await
                .is_err()
        );
        let error = reply(&store, &appender, &agents, &bots, "muse", "m", "t", "n")
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("transport-backed"),
            "{error:#}"
        );
        assert!(appender.calls.lock().unwrap().is_empty());
    }
}
