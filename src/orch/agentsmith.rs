//! The AgentSmith bridge roster: the live fleet this bridge may expose in Rakazo.
//!
//! The roster is read from the command in `[agentsmith] roster_command` (not
//! scraped from tmux and not derived from `verticals.toml`). One JSON document
//! is parsed per bridge interval; the shape consumed today is:
//!
//! ```text
//! { "workers": [ { "name", "session", "pane", "state", "status",
//!                  "cwd", "branch", ... } ], "bridge": { "hide_workers": bool } }
//! ```
//!
//! Rows are **not** `.rows[]` (that key is null in the live payload). Only
//! `.workers[]` is read.
//!
//! Role is derived from `session`, behind [`role`], because the payload has no
//! explicit `role` field yet: `pm-<vertical>` is a PM and `fleet` is a worker.
//! That is deliberately *not* name-prefix parsing — the worker
//! `ivr-instructor-materials-courseware-mcp-tool` shares a prefix with the
//! `ivr-instructor-materials` vertical, and a name-prefix guess would
//! mis-attribute it.
//!
//! Offline means offline: `state == "gone"` drops the pane even when the roster
//! still reports a stale id (today a gone `pm-jarvis` reports `pane: "%65"`).
//! A missing pane is never invented.
//!
//! Neither the roster JSON nor the derived entries carry a secret.

use crate::orch::roster;
use crate::rakazo::Bot;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::process::Command;

/// The orchestrator is never a worker row (the control pane is excluded); the
/// bridge supplies it itself, exactly as `roster.rs` does.
pub const ORCH_KEY: &str = roster::ORCH_KEY;
/// Slice B owns the Muse transport; the roster lists it but Slice A never
/// creates or watches a Rakazo bot for it.
pub const MUSE_KEY: &str = "muse";
/// Every worker bot this bridge creates is prefixed with this and suffixed `-v1`.
pub const WORKER_PREFIX: &str = "fleet-worker-";
pub const VERSION_SUFFIX: &str = "-v1";

/// Which piece of the fleet a roster row describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Pm,
    Worker,
}

/// One row of `.workers[]` (or `.rows[]` on newer AgentSmith builds), reduced
/// to the fields the bridge consumes.
///
/// Unknown fields are ignored on purpose: the live payload carries ~28 fields
/// (`index`, `badge`, `tokens_up`, `context_percent`, …) that this slice does
/// not use.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Row {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub session: String,
    /// The box this agent runs on. Required for PM/worker rows; there is no
    /// hostname guessing and no default to the local machine.
    #[serde(default)]
    pub host: String,
    /// Explicit role when the roster publishes one (`pm`, `worker`). Older
    /// payloads omit it and the role is derived from `session`.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

/// `.bridge` is optional in the payload. When AgentSmith publishes
/// `hide_workers`, the bridge honours it; otherwise the local config default
/// applies. One knob, two possible sources (see `bridge.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
struct RemoteBridge {
    #[serde(default)]
    hide_workers: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct Envelope {
    /// Some AgentSmith builds expose `.rows[]`; the binary on the reference
    /// host exposes `.workers[]` and leaves `.rows` null. Read whichever is a
    /// real array, preferring `rows` when both are present.
    #[serde(default)]
    rows: Option<Vec<Row>>,
    #[serde(default)]
    workers: Option<Vec<Row>>,
    #[serde(default)]
    bridge: Option<RemoteBridge>,
}

/// Which key in the payload supplied the rows. Reported by `bridge status` so a
/// shape change is visible instead of silent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RosterKey {
    #[default]
    Workers,
    Rows,
}

impl RosterKey {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Workers => "workers",
            Self::Rows => "rows",
        }
    }
}

/// One parsed roster: rows plus the optional upstream `hide_workers` hint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roster {
    pub rows: Vec<Row>,
    pub hide_workers: Option<bool>,
    pub key: RosterKey,
}

/// One agent the bridge will expose. Stable keys are derived, never configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// Stable local key; also the Rakazo `spawnKey` for bots we create.
    pub key: String,
    /// Rakazo bot name. PM children are named by the orchestrator, so this must
    /// stay in sync with `roster::title` (identity contract).
    pub name: String,
    pub role: AgentRole,
    /// The box this agent runs on. A supervisor owns only agents whose host
    /// equals its own explicit `[bridge] host`.
    pub host: String,
    /// tmux pane, `None` when the agent is offline. Never used for a paste:
    /// `pi-notify` is the only local delivery path.
    pub pane: Option<String>,
    /// `pi-notify --worker` target.
    pub delivery: String,
    /// A gone agent is listed (so `status` shows it) but is never delivered to.
    pub offline: bool,
}

impl Agent {
    /// The vertical for a PM entry, derived from its stable key.
    pub fn vertical(&self) -> Option<&str> {
        (self.role == AgentRole::Pm).then(|| {
            self.key
                .strip_prefix("fleet-pm-")
                .and_then(|rest| rest.strip_suffix(VERSION_SUFFIX))
                .unwrap_or(&self.key)
        })
    }

    /// Accept the key, the pi-notify target, the `pm:<vertical>` shorthand, or
    /// the Rakazo bot name. Used by `bridge reply/provision --agent`.
    pub fn matches(&self, needle: &str) -> bool {
        if self.key == needle || self.delivery == needle || self.name == needle {
            return true;
        }
        self.vertical()
            .is_some_and(|vertical| needle == format!("pm:{vertical}"))
    }
}

/// Who an entry is: the root, a PM child, a relay-only worker, or Muse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRole {
    Orch,
    Pm,
    Worker,
    Muse,
}

impl AgentRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Orch => "orch",
            Self::Pm => "pm",
            Self::Worker => "worker",
            Self::Muse => "muse",
        }
    }
}

/// Role sniffing lives here and nowhere else.
///
/// An explicit row `role` wins when AgentSmith publishes one; otherwise the
/// role comes from `session` (PMs are `pm-<vertical>`, fleet workers are
/// `fleet`). The session rule is deliberately not name-prefix parsing: the
/// worker `ivr-instructor-materials-courseware-mcp-tool` shares a prefix with
/// the `ivr-instructor-materials` vertical.
pub fn role(session: &str) -> Option<Role> {
    let session = session.trim();
    if session.starts_with("pm-") {
        Some(Role::Pm)
    } else if session == "fleet" {
        Some(Role::Worker)
    } else {
        None
    }
}

/// Explicit `role` first, then `session`. Unknown explicit roles are ignored so
/// a future role (`muse`, `orch`) is not mis-read as a worker.
pub fn row_role(row: &Row) -> Option<Role> {
    match row.role.as_deref().map(str::trim) {
        Some(role) if role.eq_ignore_ascii_case("pm") => Some(Role::Pm),
        Some(role) if role.eq_ignore_ascii_case("worker") => Some(Role::Worker),
        _ => role(&row.session),
    }
}

/// A gone/live decision from the row's `state`. `gone` is offline; an empty
/// state is treated as live only when a pane was reported.
pub fn is_offline(row: &Row) -> bool {
    row.state.eq_ignore_ascii_case("gone") || row.state.eq_ignore_ascii_case("dead")
}

/// The pane a row may be delivered to, or `None`. A stale pane on a gone row is
/// dropped rather than trusted.
pub fn live_pane(row: &Row) -> Option<String> {
    if is_offline(row) {
        return None;
    }
    row.pane
        .as_deref()
        .map(str::trim)
        .filter(|pane| !pane.is_empty() && *pane != "-")
        .map(str::to_string)
}

/// Parse the roster JSON document. Reads `.rows[]` when it is a real array
/// (newer AgentSmith) and otherwise `.workers[]` (the reference binary, where
/// `.rows` is null). Malformed JSON is an error: the bridge fails closed rather
/// than inventing agents.
pub fn parse(json: &str) -> Result<Roster> {
    let envelope: Envelope =
        serde_json::from_str(json).context("invalid AgentSmith roster JSON")?;
    let (rows, key) = match (envelope.rows, envelope.workers) {
        (Some(rows), _) if !rows.is_empty() => (rows, RosterKey::Rows),
        (_, Some(workers)) => (workers, RosterKey::Workers),
        (Some(rows), None) => (rows, RosterKey::Rows),
        (None, None) => (Vec::new(), RosterKey::Workers),
    };
    Ok(Roster {
        rows,
        hide_workers: envelope.bridge.and_then(|bridge| bridge.hide_workers),
        key,
    })
}

/// A stable, filesystem-safe token file stem. Worker names are already slugs;
/// this keeps a hand-edited roster from escaping `service_token_dir`.
pub fn safe_key(key: &str) -> Result<String> {
    let mapped: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse separator runs and trim them, so `../` cannot become a path
    // segment and `..` cannot survive anywhere.
    let mut safe = String::new();
    for c in mapped.chars() {
        if c == '-' && safe.ends_with('-') {
            continue;
        }
        safe.push(c);
    }
    let safe = safe.trim_matches('-').to_string();
    ensure!(!safe.is_empty(), "agent key {key:?} has no safe characters");
    Ok(safe)
}

/// Derive the visible agents from parsed rows.
///
/// * `orch` is always first and always present; it is owned by `local_host`;
/// * `muse` is listed but transport-backed (Slice B), so no bot is created;
/// * every PM row is listed, online or not, carrying the row's `host`;
/// * worker rows appear only when `hide_workers` is false.
///
/// A PM/worker row without a `host` is an explicit error: a supervisor must
/// never assume a foreign row belongs to the box it happens to run on.
/// Duplicate keys are an error: two rows must never collapse into one bot.
pub fn agents(rows: &[Row], hide_workers: bool, local_host: &str) -> Result<Vec<Agent>> {
    ensure!(
        !local_host.trim().is_empty(),
        "bridge host must be explicit (set [bridge] host or PI_AGENT_HOST)"
    );
    let local_host = local_host.trim().to_string();
    let mut agents = vec![
        Agent {
            key: ORCH_KEY.to_string(),
            name: roster::ORCH_NAME.to_string(),
            role: AgentRole::Orch,
            host: local_host.clone(),
            pane: roster::orch_pane(),
            delivery: ORCH_KEY.to_string(),
            offline: false,
        },
        Agent {
            key: MUSE_KEY.to_string(),
            name: "Muse".to_string(),
            role: AgentRole::Muse,
            host: local_host,
            pane: None,
            delivery: MUSE_KEY.to_string(),
            offline: false,
        },
    ];
    for row in rows {
        let Some(role) = row_role(row) else {
            continue;
        };
        let name = row.name.trim();
        ensure!(
            !name.is_empty(),
            "roster row in session {:?} has no name",
            row.session
        );
        let host = row.host.trim();
        ensure!(
            !host.is_empty(),
            "roster row {name:?} in session {:?} has no host; refusing to guess",
            row.session
        );
        let offline = is_offline(row);
        let agent = match role {
            Role::Pm => {
                let vertical = row
                    .session
                    .trim()
                    .strip_prefix("pm-")
                    .filter(|vertical| !vertical.is_empty())
                    .context("PM session without a vertical")?;
                Agent {
                    key: format!("fleet-pm-{vertical}{VERSION_SUFFIX}"),
                    // Same display contract as `roster.rs`: the PM child is
                    // resolved by exact name under the live orchestrator.
                    name: format!("PM · {}", roster::title(vertical)),
                    role: AgentRole::Pm,
                    host: host.to_string(),
                    pane: live_pane(row),
                    delivery: row.session.trim().to_string(),
                    offline,
                }
            }
            Role::Worker => {
                if hide_workers {
                    continue;
                }
                Agent {
                    key: format!("{WORKER_PREFIX}{name}{VERSION_SUFFIX}"),
                    name: format!("{WORKER_PREFIX}{name}{VERSION_SUFFIX}"),
                    role: AgentRole::Worker,
                    host: host.to_string(),
                    pane: live_pane(row),
                    delivery: name.to_string(),
                    offline,
                }
            }
        };
        agents.push(agent);
    }
    let mut keys = std::collections::BTreeMap::new();
    for agent in &agents {
        if let Some(previous) = keys.insert(agent.key.clone(), agent.host.clone()) {
            anyhow::bail!(
                "duplicate agent key {} claimed by hosts {previous:?} and {:?}; failing closed",
                agent.key,
                agent.host
            );
        }
    }
    Ok(agents)
}

/// Match one derived agent to its live Rakazo bot.
///
/// * root: our own `spawnKey` (`fleet-orch-v1`);
/// * worker: our own `spawnKey` (`fleet-worker-<name>-v1`), never a name guess;
/// * PM: an exact-name child of the **live** orchestrator, because only the
///   orchestrator's `spawn_bot` can set `parentBotId` and children cannot carry
///   our `spawnKey`.
///
/// Muse is transport-backed and has no Rakazo bot.
pub fn resolve_bot<'a>(bots: &'a [Bot], _agents: &[Agent], agent: &Agent) -> Option<&'a Bot> {
    let live = || bots.iter().filter(|bot| bot.archived_at.is_none());
    match agent.role {
        AgentRole::Muse => None,
        AgentRole::Orch | AgentRole::Worker => {
            live().find(|bot| bot.spawn_key.as_deref() == Some(agent.key.as_str()))
        }
        AgentRole::Pm => {
            let root = live().find(|bot| bot.spawn_key.as_deref() == Some(ORCH_KEY))?;
            live().find(|bot| {
                bot.parent_bot_id.as_deref() == Some(root.id.as_str()) && bot.name == agent.name
            })
        }
    }
}

/// Runs the configured roster command and parses its stdout. The only
/// production source; tests inject a fake [`RosterSource`].
#[derive(Clone, Debug)]
pub struct CommandRoster {
    program: String,
    args: Vec<String>,
}

impl CommandRoster {
    pub fn new(command: &[String]) -> Result<Self> {
        let (program, args) = command
            .split_first()
            .context("agentsmith.roster_command must not be empty")?;
        ensure!(!program.trim().is_empty(), "empty roster program");
        Ok(Self {
            program: program.clone(),
            args: args.to_vec(),
        })
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Roster source seam. Production is [`CommandRoster`]; tests script rows.
pub trait RosterSource {
    fn fetch(&self) -> Result<Roster>;
}

impl RosterSource for CommandRoster {
    fn fetch(&self) -> Result<Roster> {
        let output = Command::new(&self.program)
            .args(&self.args)
            .output()
            .with_context(|| format!("run roster command {}", self.program))?;
        ensure!(
            output.status.success(),
            "roster command {} {} exited with {}: {}",
            self.program,
            self.args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        parse(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Test/source seam that returns a scripted roster (or a scripted failure).
pub struct FixedRoster {
    pub roster: Result<Roster, String>,
}

impl FixedRoster {
    pub fn rows(rows: Vec<Row>) -> Self {
        Self {
            roster: Ok(Roster {
                rows,
                hide_workers: None,
                key: RosterKey::Workers,
            }),
        }
    }

    pub fn failing(message: &str) -> Self {
        Self {
            roster: Err(message.to_string()),
        }
    }
}

impl RosterSource for FixedRoster {
    fn fetch(&self) -> Result<Roster> {
        match &self.roster {
            Ok(roster) => Ok(roster.clone()),
            Err(message) => anyhow::bail!("{message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUDIO: &str = "averys-mac-studio";

    fn row(name: &str, session: &str, pane: Option<&str>, state: &str) -> Row {
        Row {
            name: name.into(),
            session: session.into(),
            host: STUDIO.into(),
            pane: pane.map(str::to_string),
            state: state.into(),
            status: state.into(),
            cwd: None,
            branch: None,
            ..Row::default()
        }
    }

    fn row_on(host: &str, name: &str, session: &str, pane: &str) -> Row {
        Row {
            host: host.into(),
            ..row(name, session, Some(pane), "working")
        }
    }

    #[test]
    fn live_payload_shape_is_workers_not_rows() {
        // A trimmed version of the verified payload: rows live in `.workers[]`
        // and `.rows` is null. Unknown fields must not break parsing.
        let roster = parse(
            r#"{"rows":null,"workers":[
                {"index":1,"name":"pm-fpl","session":"pm-fpl","pane":"%40","state":"idle","status":"idle","cwd":"/x","branch":"master","badge":null},
                {"index":2,"name":"fab-cicd","session":"fleet","pane":"%112","state":"working","status":"working"}
            ],"bridge":{"hide_workers":true}}"#,
        )
        .unwrap();
        assert_eq!(roster.rows.len(), 2);
        assert_eq!(roster.hide_workers, Some(true));
        assert_eq!(roster.key, RosterKey::Workers);
        assert_eq!(roster.rows[0].state, "idle");
        assert_eq!(roster.rows[1].session, "fleet");
    }

    #[test]
    fn newer_rows_shape_is_read_and_preferred() {
        let json = r#"{"rows":[{"name":"pm-fpl","session":"pm-fpl","host":"averys-mac-studio","role":"pm","pane":"%40","state":"working"}],"workers":[]}"#;
        let roster = parse(json).unwrap();
        assert_eq!(roster.key, RosterKey::Rows);
        assert_eq!(roster.rows.len(), 1);
        assert_eq!(roster.rows[0].host, STUDIO);
        assert_eq!(roster.rows[0].role.as_deref(), Some("pm"));
    }

    #[test]
    fn an_explicit_role_wins_over_the_session_heuristic() {
        let mut explicit = row("agent", "fleet", Some("%1"), "working");
        explicit.role = Some("pm".into());
        assert_eq!(row_role(&explicit), Some(Role::Pm));
        explicit.role = Some("worker".into());
        assert_eq!(row_role(&explicit), Some(Role::Worker));
        explicit.role = Some("something-new".into());
        assert_eq!(
            row_role(&explicit),
            Some(Role::Worker),
            "unknown explicit role falls back to session"
        );
    }

    #[test]
    fn pm_shorthand_matches_the_derived_key() {
        let rows = vec![row("pm-fpl", "pm-fpl", Some("%40"), "working")];
        let agents = agents(&rows, true, STUDIO).unwrap();
        let fpl = agents
            .iter()
            .find(|agent| agent.key == "fleet-pm-fpl-v1")
            .unwrap();
        assert_eq!(fpl.vertical(), Some("fpl"));
        assert!(fpl.matches("pm:fpl"));
        assert!(fpl.matches("pm-fpl"));
        assert!(fpl.matches("fleet-pm-fpl-v1"));
        assert!(!fpl.matches("pm:marbles"));
    }

    #[test]
    fn default_roster_is_orch_every_pm_and_muse_with_no_workers() {
        let rows = vec![
            row("pm-fpl", "pm-fpl", Some("%40"), "idle"),
            row("pm-marbles", "pm-marbles", Some("%31"), "working"),
            row("fab-cicd", "fleet", Some("%112"), "working"),
        ];
        let agents = agents(&rows, true, STUDIO).unwrap();
        let keys: Vec<_> = agents.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "fleet-orch-v1",
                "muse",
                "fleet-pm-fpl-v1",
                "fleet-pm-marbles-v1"
            ],
            "no worker bots when hide_workers is true"
        );
        assert!(
            !agents
                .iter()
                .any(|a| a.key.contains("worker") || a.key == "fab-cicd")
        );
        assert_eq!(agents[2].name, "PM · FPL");
        assert_eq!(agents[2].delivery, "pm-fpl");
        assert_eq!(agents[1].role, AgentRole::Muse);
    }

    #[test]
    fn workers_appear_when_visible_with_their_own_keys_and_targets() {
        let rows = vec![
            row("pm-fpl", "pm-fpl", Some("%40"), "idle"),
            // The ambiguous prefix: a worker whose name starts with a vertical.
            row(
                "ivr-instructor-materials-courseware-mcp-tool",
                "fleet",
                Some("%116"),
                "working",
            ),
        ];
        let agents = agents(&rows, false, STUDIO).unwrap();
        let worker = agents
            .iter()
            .find(|a| a.role == AgentRole::Worker)
            .expect("visible worker");
        assert_eq!(
            worker.key,
            "fleet-worker-ivr-instructor-materials-courseware-mcp-tool-v1"
        );
        assert_eq!(
            worker.delivery, "ivr-instructor-materials-courseware-mcp-tool",
            "the role came from session, so the name prefix did not confuse it"
        );
    }

    #[test]
    fn gone_agents_are_offline_and_their_stale_pane_is_dropped() {
        let rows = vec![row("pm-jarvis", "pm-jarvis", Some("%65"), "gone")];
        let agents = agents(&rows, true, STUDIO).unwrap();
        let jarvis = agents
            .iter()
            .find(|agent| agent.matches("pm:jarvis"))
            .unwrap();
        assert!(jarvis.offline);
        assert_eq!(jarvis.pane, None, "never deliver to a dead pane");
    }

    #[test]
    fn control_and_unknown_sessions_are_not_relay_targets() {
        let rows = vec![
            row("orch", "control", Some("%0"), "working"),
            row("somewhere", "", None, "idle"),
        ];
        let agents = agents(&rows, false, STUDIO).unwrap();
        assert_eq!(agents.len(), 2, "only orch + muse");
    }

    #[test]
    fn duplicate_keys_are_refused() {
        let rows = vec![
            row("pm-fpl", "pm-fpl", Some("%1"), "idle"),
            row("pm-fpl", "pm-fpl", Some("%2"), "idle"),
        ];
        assert!(agents(&rows, true, STUDIO).is_err());
    }

    #[test]
    fn a_row_without_a_host_is_an_error_not_a_local_default() {
        let mut missing = row("pm-fpl", "pm-fpl", Some("%40"), "working");
        missing.host = String::new();
        let error = agents(&[missing], true, STUDIO).unwrap_err();
        assert!(format!("{error:#}").contains("has no host"), "{error:#}");
    }

    #[test]
    fn a_duplicate_host_claim_fails_closed() {
        let rows = vec![
            row_on(STUDIO, "pm-fpl", "pm-fpl", "%40"),
            row_on("agora-one", "pm-fpl", "pm-fpl", "%40"),
        ];
        let error = agents(&rows, true, STUDIO).unwrap_err();
        assert!(format!("{error:#}").contains("duplicate"), "{error:#}");
    }

    #[test]
    fn safe_keys_cannot_escape_the_token_directory() {
        assert_eq!(safe_key("fleet-pm-fpl-v1").unwrap(), "fleet-pm-fpl-v1");
        assert_eq!(safe_key("../../etc/passwd").unwrap(), "etc-passwd");
        assert_eq!(safe_key("a b").unwrap(), "a-b");
        assert!(safe_key("...").is_err());
    }
}
