//! Multi-agent supervisor: one Rakazo conversation per visible fleet agent.
//!
//! Replaces the single-allowlist watch. Every cycle (the caller's interval):
//!
//! 1. load the roster and derive the visible agents ([`super::agentsmith`]);
//! 2. keep only agents whose roster `host` equals this bridge's explicit host;
//!    foreign-host rows are reported and never delivered to by guess;
//! 3. reconcile the tracked set — added agents load their own `WatchState`,
//!    removed agents are dropped (their state file stays, so a reappearing
//!    identity does not re-deliver old messages);
//! 4. resolve each agent to its live Rakazo bot. The root is created through
//!    the idempotent `bots.create` path; PM children are **reported missing**
//!    because only the orchestrator's `spawn_bot` can nest them; relay-only
//!    worker bots are created/archived with the roster.
//! 5. poll each agent's thread and deliver new **user-role** rows once through
//!    the [`Deliverer`] seam. Bot/system rows and our own echoes are skipped. A
//!    delivery failure or a `Deferred` result does not advance that agent's
//!    watermark.
//!
//! The only local deliverer is `pi-notify`. There is no SSH path and no pane
//! paste; a future `swarmd` transport implements [`Deliverer`] behind the same
//! seam without rewriting routing.
//!
//! One failure does not hide the others: a bot-list failure is fatal for the
//! cycle, but a per-agent poll or delivery error is recorded in that agent's
//! state and the remaining agents still run.

use super::agentsmith::{Agent, AgentRole, ORCH_KEY, WORKER_PREFIX, resolve_bot, safe_key};
use super::watch::{
    NewAgentBot, PiNotify, RakazoSource, Report, WatchState, now_rfc3339, summarize, write_state,
};
use crate::rakazo::Bot;
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Delivery outcome. `Accepted` advances the watermark; `Deferred` does not, so
/// the same row is retried next cycle. Today only the local `pi-notify`
/// deliverer exists and it returns `Accepted` or an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    Deferred,
}

/// The one delivery seam. No implementation may paste into a pane or SSH to
/// another box; a `swarmd` adapter plugs in here once its deliver/ack/query
/// surface is documented.
pub trait Deliverer {
    /// `agent_key`/`host` identify the owner; `worker` is the local wake target.
    fn deliver(
        &self,
        agent_key: &str,
        host: &str,
        worker: &str,
        target: Option<&str>,
        text: &str,
        reference: &str,
    ) -> Result<Delivery>;
}

/// Local delivery through the fleet's existing `pi-notify` primitive.
#[derive(Clone, Debug, Default)]
pub struct PiNotifyDeliverer {
    pub notifier: PiNotify,
}

impl PiNotifyDeliverer {
    pub fn configured(kind: &str, urgent: bool) -> Self {
        Self {
            notifier: PiNotify::configured(kind, urgent),
        }
    }
}

impl Deliverer for PiNotifyDeliverer {
    fn deliver(
        &self,
        _agent_key: &str,
        host: &str,
        worker: &str,
        target: Option<&str>,
        text: &str,
        reference: &str,
    ) -> Result<Delivery> {
        // A host mismatch reaching this point is a routing bug, never a reason
        // to wake a local pane for a foreign agent.
        ensure!(
            !host.is_empty(),
            "refusing to deliver {agent_key} without an owner host",
            agent_key = _agent_key
        );
        let args = self
            .notifier
            .command_args_for(worker, target, reference, text);
        let output = std::process::Command::new("pi-notify")
            .args(&args)
            .output()
            .map_err(|error| anyhow::anyhow!("run pi-notify send: {error}"))?;
        ensure!(
            output.status.success(),
            "pi-notify send failed for {worker}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(Delivery::Accepted)
    }
}

/// One agent's supervisor state: the binding plus the durable dedupe ledger.
#[derive(Clone, Debug)]
pub struct Tracked {
    pub agent: Agent,
    pub state: WatchState,
    pub state_file: PathBuf,
    pub bot_id: Option<String>,
    /// True when the roster lists the agent but no live bot could be resolved
    /// and the bridge is not allowed to create one (a PM child).
    pub missing: bool,
}

/// One cycle's outcome. `errors` are per-agent and never stop the other agents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CycleReport {
    pub agents: usize,
    pub owned: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub foreign: Vec<String>,
    pub created: Vec<String>,
    pub archived: Vec<String>,
    pub missing: Vec<String>,
    pub skipped_offline: Vec<String>,
    pub delivered: usize,
    pub deferred: usize,
    pub duplicates: usize,
    pub ignored: usize,
    /// Seen tapbacks posted after a first delivery.
    pub seen: usize,
    /// Seen tapbacks that failed; delivery is unaffected.
    pub seen_failed: usize,
    pub errors: Vec<String>,
}

impl CycleReport {
    fn absorb(&mut self, report: Report) {
        self.delivered += report.delivered;
        self.duplicates += report.duplicates;
        self.ignored += report.ignored;
        self.deferred += report.deferred;
        self.seen += report.seen;
        self.seen_failed += report.seen_failed;
    }
}

pub struct Supervisor<S: RakazoSource, D: Deliverer> {
    source: S,
    deliverer: D,
    state_dir: PathBuf,
    host: String,
    tracked: BTreeMap<String, Tracked>,
    /// Tapback posted once per first delivery. Empty means disabled.
    seen_reaction: String,
    /// `--bridge-config` path appended to the delivered reply hint.
    reply_hint_config: Option<PathBuf>,
}

impl<S: RakazoSource, D: Deliverer> Supervisor<S, D> {
    /// `state_dir/<host>/<safe-key>.json` holds one ledger per agent owned by
    /// this box. `host` is explicit (config or `PI_AGENT_HOST`); it is never
    /// guessed from the machine name.
    pub fn new(
        source: S,
        deliverer: D,
        state_dir: impl Into<PathBuf>,
        host: impl Into<String>,
    ) -> Result<Self> {
        let host = host.into();
        ensure!(
            !host.trim().is_empty(),
            "bridge host must be explicit (set [bridge] host or PI_AGENT_HOST)"
        );
        Ok(Self {
            source,
            deliverer,
            state_dir: state_dir.into(),
            host: host.trim().to_string(),
            tracked: BTreeMap::new(),
            seen_reaction: String::new(),
            reply_hint_config: None,
        })
    }

    /// Enable the seen tapback (`[bridge] seen_reaction`). Empty disables it and
    /// makes zero `threads.react` calls.
    pub fn with_seen_reaction(mut self, reaction: impl Into<String>) -> Self {
        self.seen_reaction = reaction.into();
        self
    }

    /// Enable the reply hint line in delivered text with this `--bridge-config`.
    pub fn with_reply_hint_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.reply_hint_config = Some(path.into());
        self
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn tracked(&self, key: &str) -> Option<&Tracked> {
        self.tracked.get(key)
    }

    pub fn tracked_map(&self) -> &BTreeMap<String, Tracked> {
        &self.tracked
    }

    pub fn state_file(&self, key: &str) -> Result<PathBuf> {
        state_file(&self.state_dir, &self.host, key)
    }

    /// Reconcile the tracked set with `agents`, then resolve, create/archive and
    /// poll. The roster itself is loaded by the caller (`serve`), so this is
    /// also directly testable.
    pub async fn cycle(&mut self, agents: &[Agent]) -> Result<CycleReport> {
        // Host ownership guard: this supervisor may own only rows whose host is
        // exactly its own. Foreign rows are reported, never polled or delivered.
        let owned: Vec<Agent> = agents
            .iter()
            .filter(|agent| agent.host == self.host)
            .cloned()
            .collect();
        let foreign: Vec<String> = agents
            .iter()
            .filter(|agent| agent.host != self.host)
            .map(|agent| format!("{}@{}", agent.key, agent.host))
            .collect();
        let mut report = CycleReport {
            agents: agents.len(),
            owned: owned.len(),
            foreign,
            ..CycleReport::default()
        };
        let previous_worker_keys: BTreeSet<String> = self
            .tracked
            .keys()
            .filter(|key| key.starts_with(WORKER_PREFIX))
            .cloned()
            .collect();
        self.reconcile(&owned, &mut report)?;
        let mut bots = self.source.bots().await?;

        // Resolve-or-create per owned agent. The root and relay-only workers
        // are ours to create idempotently; PM children are never fabricated.
        for agent in &owned {
            if agent.role == AgentRole::Muse {
                continue;
            }
            if agent.offline {
                report.skipped_offline.push(agent.key.clone());
                continue;
            }
            let Some(tracked) = self.tracked.get_mut(&agent.key) else {
                continue;
            };
            match resolve_bot(&bots, &owned, agent) {
                Some(bot) => {
                    tracked.bot_id = Some(bot.id.clone());
                    tracked.missing = false;
                }
                None => match agent.role {
                    AgentRole::Pm => {
                        // Only the orchestrator's own spawn_bot tool can nest a
                        // PM child; the bridge reports it missing instead of
                        // faking one.
                        tracked.missing = true;
                        tracked.bot_id = None;
                        report.missing.push(agent.key.clone());
                    }
                    AgentRole::Orch | AgentRole::Worker => {
                        let spec = bot_spec(agent);
                        match self.source.create_bot(spec).await {
                            Ok(bot) => {
                                report.created.push(agent.key.clone());
                                tracked.bot_id = Some(bot.id.clone());
                                tracked.missing = false;
                                bots.push(bot);
                            }
                            Err(error) => {
                                tracked.missing = true;
                                tracked.state.last_poll_ok = Some(false);
                                tracked.state.last_poll_at = Some(now_rfc3339());
                                tracked.state.last_error =
                                    Some(format!("create bot {}: {error:#}", agent.key));
                                save_tracked(tracked)?;
                                report
                                    .errors
                                    .push(format!("{}: create bot failed: {error:#}", agent.key));
                            }
                        }
                    }
                    AgentRole::Muse => unreachable!("muse is skipped above"),
                },
            }
        }

        // A worker that left the roster (or went away when hide_workers turned
        // on) must not keep a live bot. Only bots this host owns (rostered here
        // or previously tracked here) are candidates, so a Studio supervisor
        // never archives a neo worker.
        let wanted_workers: BTreeSet<&str> = owned
            .iter()
            .filter(|agent| agent.role == AgentRole::Worker && !agent.offline)
            .map(|agent| agent.key.as_str())
            .collect();
        let owned_worker_keys: BTreeSet<&str> = owned
            .iter()
            .filter(|agent| agent.role == AgentRole::Worker)
            .map(|agent| agent.key.as_str())
            .collect();
        let mut candidates: BTreeSet<String> = owned_worker_keys
            .iter()
            .map(|key| key.to_string())
            .collect();
        // Workers tracked before this cycle: a worker that just vanished from
        // the roster (or from `hide_workers=false`) is still ours to archive.
        candidates.extend(previous_worker_keys);
        for bot in bots.iter().filter(|bot| bot.archived_at.is_none()) {
            let Some(spawn) = bot.spawn_key.as_deref() else {
                continue;
            };
            if !candidates.contains(spawn) || wanted_workers.contains(spawn) {
                continue;
            }
            match self.source.archive_bot(&bot.id).await {
                Ok(()) => report.archived.push(spawn.to_string()),
                Err(error) => report
                    .errors
                    .push(format!("{spawn}: archive failed: {error:#}")),
            }
        }

        // Poll each live, resolvable owned agent once.
        for agent in &owned {
            if agent.role == AgentRole::Muse || agent.offline {
                continue;
            }
            let Some(tracked) = self.tracked.get_mut(&agent.key) else {
                continue;
            };
            let Some(bot_id) = tracked.bot_id.clone() else {
                continue;
            };
            let thread = bots
                .iter()
                .find(|bot| bot.id == bot_id)
                .map(|bot| bot.thread_id.clone().unwrap_or_else(|| bot.id.clone()))
                .or_else(|| tracked.state.thread_id.clone())
                .unwrap_or_else(|| bot_id.clone());
            tracked.state.bind_conversation(&bot_id, &thread);

            match poll_one(
                &self.source,
                &self.deliverer,
                tracked,
                &agent.delivery,
                &bot_id,
                &self.seen_reaction,
                self.reply_hint_config.as_deref(),
            )
            .await
            {
                Ok(report_one) => report.absorb(report_one),
                Err(error) => {
                    tracked.state.last_poll_ok = Some(false);
                    tracked.state.last_poll_at = Some(now_rfc3339());
                    tracked.state.last_error = Some(format!("{error:#}"));
                    save_tracked(tracked)?;
                    report.errors.push(format!("{}: {error:#}", agent.key));
                }
            }
        }
        Ok(report)
    }

    /// Added agents load their per-agent state file; removed agents are dropped
    /// without deleting the file (same identity, same ledger).
    fn reconcile(&mut self, agents: &[Agent], report: &mut CycleReport) -> Result<()> {
        let wanted: BTreeMap<&str, &Agent> = agents
            .iter()
            .map(|agent| (agent.key.as_str(), agent))
            .collect();
        let existing: Vec<String> = self.tracked.keys().cloned().collect();
        for key in existing {
            if !wanted.contains_key(key.as_str()) {
                self.tracked.remove(&key);
                report.removed.push(key);
            }
        }
        for (key, agent) in wanted {
            if let Some(tracked) = self.tracked.get_mut(key) {
                tracked.agent = (*agent).clone();
                continue;
            }
            let state_file = self.state_file(key)?;
            migrate_legacy_state(&self.state_dir, &self.host, key)?;
            let state = WatchState::load(Some(&state_file))?;
            ensure!(
                state.bot_id.is_empty() || state.thread_id.is_some(),
                "watch state for {key} is bound to a bot without a conversation"
            );
            self.tracked.insert(
                key.to_string(),
                Tracked {
                    agent: (*agent).clone(),
                    state,
                    state_file,
                    bot_id: None,
                    missing: false,
                },
            );
            report.added.push(key.to_string());
        }
        Ok(())
    }
}

/// Per-agent state file path, scoped by host so moving an agent to another box
/// cannot inherit this box's watermark. Shared by the supervisor and
/// `bridge status` so both read the same ledger.
pub fn state_file(state_dir: &Path, host: &str, key: &str) -> Result<PathBuf> {
    let host = safe_key(host)?;
    Ok(state_dir
        .join(host)
        .join(format!("{}.json", safe_key(key)?)))
}

/// Migrate a pre-host state file (`state_dir/<key>.json`) into the host-scoped
/// location when the host-scoped file does not exist yet. This is what keeps
/// the old `orch-inbound-state.json` (or an old `fleet-orch-v1.json`) from
/// re-delivering after the host directory is introduced.
pub fn migrate_legacy_state(state_dir: &Path, host: &str, key: &str) -> Result<bool> {
    let target = state_file(state_dir, host, key)?;
    if target.exists() {
        return Ok(false);
    }
    let legacy = state_dir.join(format!("{}.json", safe_key(key)?));
    if !legacy.exists() {
        return Ok(false);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create state dir {}", parent.display()))?;
    }
    std::fs::copy(&legacy, &target).with_context(|| {
        format!(
            "migrate legacy watch state {} to {}",
            legacy.display(),
            target.display()
        )
    })?;
    Ok(true)
}

/// The create spec for a bot the bridge owns. Every fleet bot is relay-only:
/// it never runs a model, it only appends assistant messages.
pub fn bot_spec(agent: &Agent) -> NewAgentBot {
    NewAgentBot {
        name: agent.name.clone(),
        title: match agent.role {
            AgentRole::Orch => "Relay-only orchestrator bridge".to_string(),
            AgentRole::Worker => "Relay-only fleet worker bridge".to_string(),
            AgentRole::Pm => "Relay-only PM bridge".to_string(),
            AgentRole::Muse => "Relay-only muse bridge".to_string(),
        },
        description: "Managed by rakazo-beeper `bridge serve`; relay-only, no model run."
            .to_string(),
        spawn_key: agent.key.clone(),
        relay_only: true,
    }
}

/// Where `pi-notify` should address this row. The orchestrator keeps the
/// untargeted orchestrator lane; every other agent is addressed at its live
/// pane (falling back to its session name), so a row cannot be consumed by
/// another session with a similar name.
pub fn delivery_target(agent: &Agent) -> Option<String> {
    match agent.role {
        AgentRole::Orch => None,
        AgentRole::Pm | AgentRole::Worker | AgentRole::Muse => {
            agent.pane.clone().or_else(|| Some(agent.delivery.clone()))
        }
    }
}

/// Drain one agent's thread. Each user message is notified at most once, even
/// across polls and restarts. A failure leaves the watermark untouched.
async fn poll_one<S: RakazoSource, D: Deliverer>(
    source: &S,
    deliverer: &D,
    tracked: &mut Tracked,
    delivery: &str,
    bot_id: &str,
    seen_reaction: &str,
    reply_hint_config: Option<&Path>,
) -> Result<Report> {
    tracked.state.last_poll_at = Some(now_rfc3339());
    let mut messages = source.fetch_after(bot_id, tracked.state.last_seq).await?;
    messages.sort_by_key(|message| message.seq);
    let mut report = Report::default();
    let mut changed = false;
    let conversation = tracked
        .state
        .thread_id
        .clone()
        .filter(|thread| !thread.is_empty())
        .context("agent is not bound to a conversation")?;

    for message in messages {
        if message.seq <= tracked.state.last_seq {
            report.duplicates += 1;
            tracked.state.duplicates_total += 1;
            continue;
        }
        if tracked.state.delivered(&message.id) {
            report.duplicates += 1;
            tracked.state.duplicates_total += 1;
            tracked.state.last_seq = message.seq;
            changed = true;
            continue;
        }
        let ignored = message.role != "user"
            || tracked.state.is_echo(&message)
            || message.text().trim().is_empty();
        if ignored {
            report.ignored += 1;
            tracked.state.last_seq = message.seq;
            changed = true;
            continue;
        }
        let reference = format!("rakazo:{conversation}:{}", message.id);
        let mut text = format!("[{reference}] {}", summarize(&tracked.agent.name, &message));
        if let Some(config) = reply_hint_config {
            text.push('\n');
            text.push_str(&reply_hint(config, &tracked.agent.key, &message.id));
        }
        // Persist earlier progress before the side effect, then advance past
        // this message only once the deliverer accepted it.
        if changed {
            save_tracked(tracked)?;
            changed = false;
        }
        let outcome = deliverer
            .deliver(
                &tracked.agent.key,
                &tracked.agent.host,
                delivery,
                delivery_target(&tracked.agent).as_deref(),
                &text,
                &reference,
            )
            .with_context(|| format!("deliver {reference}"))?;
        if outcome == Delivery::Deferred {
            // Not accepted by the remote transport yet: keep the watermark and
            // retry next cycle.
            report.deferred += 1;
            continue;
        }
        // "Received, working": only the first delivery ever adds it, and the
        // deterministic nonce makes a retry a replay rather than a second add.
        if !seen_reaction.is_empty() {
            let nonce = format!("seen:{}", message.id);
            match source
                .react(bot_id, &message.id, seen_reaction, &nonce)
                .await
            {
                Ok(()) => report.seen += 1,
                Err(error) => {
                    // Best-effort: log and keep the delivery and the watermark.
                    report.seen_failed += 1;
                    tracing::warn!(
                        agent = %tracked.agent.key,
                        message = %message.id,
                        "seen tapback failed: {error:#}"
                    );
                }
            }
        }
        tracked.state.record_delivered(&message.id);
        tracked.state.last_delivered_id = Some(message.id.clone());
        tracked.state.delivered_total += 1;
        tracked.state.last_seq = message.seq;
        tracked.state.last_poll_ok = Some(true);
        tracked.state.last_error = None;
        save_tracked(tracked)?;
        report.delivered += 1;
    }

    if changed {
        save_tracked(tracked)?;
    }
    tracked.state.last_poll_ok = Some(true);
    tracked.state.last_error = None;
    save_tracked(tracked)?;
    Ok(report)
}

/// The one command that lets an agent answer without being told twice. Kept
/// terse, never carries a token, and preserves the `[rakazo:<thread>:<id>]`
/// reference prefix already in the delivered text.
fn reply_hint(config: &Path, agent_key: &str, message_id: &str) -> String {
    format!(
        "[reply: rakazo-beeper bridge --bridge-config {} reply --agent {} --reply-to {} --text \"<answer>\"]",
        shell_arg(&config.to_string_lossy()),
        agent_key,
        message_id
    )
}

/// Quote one argument only when it would otherwise split.
fn shell_arg(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn save_tracked(tracked: &Tracked) -> Result<()> {
    write_state(&tracked.state_file, &tracked.state)
}

/// Seed one agent's state file to the conversation head **without delivering**.
/// Enabling a supervisor on an existing fleet must not replay history into
/// panes; `bridge seed` calls this for every owned agent before `serve`.
pub async fn seed_watermark<S: RakazoSource>(
    source: &S,
    state_dir: &Path,
    host: &str,
    agent: &Agent,
    bot: &Bot,
) -> Result<i64> {
    let path = state_file(state_dir, host, &agent.key)?;
    migrate_legacy_state(state_dir, host, &agent.key)?;
    let mut state = WatchState::load(Some(&path))?;
    let thread = bot.thread_id.clone().unwrap_or_else(|| bot.id.clone());
    state.bind_conversation(&bot.id, &thread);
    let latest = source.latest_seq(&bot.id).await?;
    if latest > state.last_seq {
        state.last_seq = latest;
    }
    state.last_poll_ok = Some(true);
    state.last_poll_at = Some(now_rfc3339());
    write_state(&path, &state)?;
    Ok(state.last_seq)
}

/// Match a folded roster (or a single bot) by key. Used by `bridge status`.
pub fn bot_for<'a>(agents: &[Agent], bots: &'a [Bot], key: &str) -> Option<&'a Bot> {
    let agent = agents.iter().find(|agent| agent.key == key)?;
    resolve_bot(bots, agents, agent)
}

/// A one-line, secret-free status row per agent.
pub fn status_rows(
    agents: &[Agent],
    bots: &[Bot],
    tracked: &BTreeMap<String, Tracked>,
    local_host: &str,
    backend: &str,
    token_present: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    agents
        .iter()
        .map(|agent| {
            let state = tracked.get(&agent.key);
            let bot = resolve_bot(bots, agents, agent);
            let binding = match state {
                Some(tracked) if tracked.missing => "missing",
                Some(tracked) => tracked.state.last_poll_ok.map_or("unstarted", |ok| {
                    if ok { "ok" } else { "error" }
                }),
                None => "untracked",
            };
            let relay = match bot.and_then(|bot| bot.relay_only) {
                Some(true) => "true",
                Some(false) => "false",
                None => "unknown",
            };
            let server = bot.and_then(|bot| (!bot.space_id.is_empty()).then_some(bot.space_id.as_str()));
            format!(
                "{}\t{}\thost={}\towned={}\tworker={}\tpane={}\tbot={}\tthread={}\twatermark={}\tstate={}\tlast_poll_ok={}\tlast_poll_at={}\tlast_error={}\trelay_only={}\ttoken_present={}\tdelivery_backend={}\tspace={}",
                agent.key,
                agent.role.as_str(),
                agent.host,
                agent.host == local_host,
                agent.delivery,
                agent.pane.as_deref().unwrap_or("-"),
                bot.map(|bot| bot.id.as_str()).unwrap_or("-"),
                state
                    .and_then(|tracked| tracked.state.thread_id.as_deref())
                    .or_else(|| state.and_then(|tracked| tracked.bot_id.as_deref()))
                    .unwrap_or("-"),
                state.map(|tracked| tracked.state.last_seq).unwrap_or(0),
                binding,
                state
                    .and_then(|tracked| tracked.state.last_poll_ok)
                    .map_or("-".to_string(), |ok| ok.to_string()),
                state
                    .and_then(|tracked| tracked.state.last_poll_at.as_deref())
                    .unwrap_or("-"),
                state
                    .and_then(|tracked| tracked.state.last_error.as_deref())
                    .unwrap_or("-"),
                relay,
                token_present(&agent.key),
                backend,
                server.unwrap_or("-"),
            )
        })
        .collect()
}

/// The root key, re-exported so callers do not duplicate the identity contract.
pub const ROOT_KEY: &str = ORCH_KEY;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orch::agentsmith::{Row, agents};
    use crate::rakazo::Message;
    use serde_json::json;
    use std::{collections::HashMap, future::Future, sync::Mutex};
    use tempfile::TempDir;

    const STUDIO: &str = "averys-mac-studio";
    const NEO: &str = "agora-one";

    fn row(name: &str, session: &str, pane: &str, state: &str) -> Row {
        Row {
            name: name.into(),
            session: session.into(),
            host: STUDIO.into(),
            pane: Some(pane.into()),
            state: state.into(),
            status: state.into(),
            ..Row::default()
        }
    }

    fn row_on(host: &str, name: &str, session: &str, pane: &str) -> Row {
        Row {
            host: host.into(),
            ..row(name, session, pane, "working")
        }
    }

    fn message(id: &str, seq: i64, role: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            seq,
            role: role.into(),
            blocks: vec![json!({"kind": "text", "text": text})],
            created_at: None,
            client_nonce: None,
            reply_to_message_id: None,
            reply_quote: None,
            run_id: None,
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
            relay_only: Some(true),
        }
    }

    #[derive(Default)]
    struct FakeSource {
        bots: Mutex<Vec<Bot>>,
        pages: Mutex<HashMap<String, Vec<Message>>>,
        created: Mutex<Vec<NewAgentBot>>,
        archived: Mutex<Vec<String>>,
        reacts: Mutex<Vec<(String, String, String, String)>>,
        react_fail: bool,
        bots_fail: bool,
    }

    impl FakeSource {
        fn with(bots: Vec<Bot>, pages: HashMap<String, Vec<Message>>) -> Self {
            Self {
                bots: Mutex::new(bots),
                pages: Mutex::new(pages),
                ..Self::default()
            }
        }

        fn created(&self) -> Vec<NewAgentBot> {
            self.created.lock().unwrap().clone()
        }

        fn archived(&self) -> Vec<String> {
            self.archived.lock().unwrap().clone()
        }

        fn reacts(&self) -> Vec<(String, String, String, String)> {
            self.reacts.lock().unwrap().clone()
        }
    }

    impl RakazoSource for FakeSource {
        fn bots(&self) -> impl Future<Output = Result<Vec<Bot>>> + Send {
            let fail = self.bots_fail;
            let bots = self.bots.lock().unwrap().clone();
            async move {
                if fail {
                    anyhow::bail!("scripted roster bots failure");
                }
                Ok(bots)
            }
        }

        fn fetch_after(
            &self,
            bot: &str,
            _seq: i64,
        ) -> impl Future<Output = Result<Vec<Message>>> + Send {
            // Replay the scripted rows verbatim so id/seq dedupe is tested.
            let page = self
                .pages
                .lock()
                .unwrap()
                .get(bot)
                .cloned()
                .unwrap_or_default();
            async move { Ok(page) }
        }

        fn create_bot(&self, spec: NewAgentBot) -> impl Future<Output = Result<Bot>> + Send {
            let id = format!("created-{}", spec.spawn_key);
            let bot = bot(&id, &spec.name, &spec.spawn_key, None);
            self.bots.lock().unwrap().push(bot.clone());
            self.created.lock().unwrap().push(spec);
            async move { Ok(bot) }
        }

        fn react(
            &self,
            bot: &str,
            message: &str,
            reaction: &str,
            nonce: &str,
        ) -> impl Future<Output = Result<()>> + Send {
            let fail = self.react_fail;
            if !fail {
                self.reacts.lock().unwrap().push((
                    bot.to_string(),
                    message.to_string(),
                    reaction.to_string(),
                    nonce.to_string(),
                ));
            }
            async move {
                if fail {
                    anyhow::bail!("scripted reaction failure");
                }
                Ok(())
            }
        }

        fn archive_bot(&self, id: &str) -> impl Future<Output = Result<()>> + Send {
            self.archived.lock().unwrap().push(id.to_string());
            let mut bots = self.bots.lock().unwrap();
            if let Some(bot) = bots.iter_mut().find(|bot| bot.id == id) {
                bot.archived_at = Some("2026-10-26T00:00:00Z".into());
            }
            async move { Ok(()) }
        }
    }

    /// Records deliveries; can fail or defer on demand.
    type DeliverCalls = Vec<(String, String, String, Option<String>, String, String)>;

    #[derive(Default)]
    struct FakeDeliverer {
        calls: Mutex<DeliverCalls>,
        fail: bool,
        defer: bool,
    }

    impl FakeDeliverer {
        fn calls(&self) -> DeliverCalls {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Deliverer for FakeDeliverer {
        fn deliver(
            &self,
            agent_key: &str,
            host: &str,
            worker: &str,
            target: Option<&str>,
            text: &str,
            reference: &str,
        ) -> Result<Delivery> {
            if self.fail {
                anyhow::bail!("scripted delivery failure");
            }
            if self.defer {
                return Ok(Delivery::Deferred);
            }
            self.calls.lock().unwrap().push((
                agent_key.to_string(),
                host.to_string(),
                worker.to_string(),
                target.map(str::to_string),
                text.to_string(),
                reference.to_string(),
            ));
            Ok(Delivery::Accepted)
        }
    }

    fn supervisor(
        dir: &TempDir,
        source: FakeSource,
        deliverer: FakeDeliverer,
    ) -> Supervisor<FakeSource, FakeDeliverer> {
        Supervisor::new(source, deliverer, dir.path(), STUDIO).unwrap()
    }

    fn fixture() -> (Vec<Agent>, Vec<Bot>) {
        let rows = vec![
            row("pm-fpl", "pm-fpl", "%40", "working"),
            row("pm-marbles", "pm-marbles", "%31", "working"),
            row("fab-cicd", "fleet", "%112", "working"),
        ];
        let all = agents(&rows, true, STUDIO).unwrap();
        let fpl = all
            .iter()
            .find(|agent| agent.key == "fleet-pm-fpl-v1")
            .unwrap()
            .name
            .clone();
        let marbles = all
            .iter()
            .find(|agent| agent.key == "fleet-pm-marbles-v1")
            .unwrap()
            .name
            .clone();
        let bots = vec![
            bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None),
            bot("fpl-bot", &fpl, "run:spawn_bot:hash", Some("orch-bot")),
            bot(
                "marbles-bot",
                &marbles,
                "run:spawn_bot:hash",
                Some("orch-bot"),
            ),
        ];
        (all, bots)
    }

    #[tokio::test]
    async fn one_message_wakes_only_the_agent_it_was_sent_to() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert(
            "orch-bot".to_string(),
            vec![message("m1", 7, "user", "phone -> orch")],
        );
        pages.insert(
            "fpl-bot".to_string(),
            vec![
                message("m2", 8, "user", "phone -> fpl PM"),
                message("m3", 9, "bot", "model ack that must be skipped"),
            ],
        );
        let dir = TempDir::new().unwrap();
        let mut supervisor = supervisor(
            &dir,
            FakeSource::with(bots, pages),
            FakeDeliverer::default(),
        );
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 2);
        assert_eq!(report.ignored, 1);
        assert_eq!(report.foreign.len(), 0);
        let calls = supervisor.deliverer.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "fleet-orch-v1");
        assert_eq!(calls[0].1, STUDIO);
        assert_eq!(calls[0].2, "fleet-orch-v1");
        assert_eq!(calls[0].3, None, "orch keeps the orchestrator lane");
        assert_eq!(calls[0].5, "rakazo:thread-orch-bot:m1");
        assert_eq!(calls[1].0, "fleet-pm-fpl-v1");
        assert_eq!(calls[1].2, "pm-fpl");
        assert_eq!(
            calls[1].3.as_deref(),
            Some("%40"),
            "a PM row is addressed at its own live pane"
        );
        assert_eq!(calls[1].5, "rakazo:thread-fpl-bot:m2");
        assert!(
            !calls.iter().any(|call| call.2 == "pm-marbles"),
            "the other PM pane was not woken"
        );

        // A second cycle delivers nothing: per-agent dedupe is durable.
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 0);
        assert_eq!(supervisor.deliverer.calls().len(), 2);
    }

    #[tokio::test]
    async fn a_studio_supervisor_never_delivers_a_foreign_host_row() {
        let rows = vec![
            row_on(STUDIO, "pm-fpl", "pm-fpl", "%40"),
            row_on(NEO, "pm-marbles", "pm-marbles", "%31"),
        ];
        let agents = agents(&rows, true, STUDIO).unwrap();
        let global = agents
            .iter()
            .find(|agent| agent.key == "fleet-pm-marbles-v1")
            .unwrap()
            .name
            .clone();
        let bots = vec![
            bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None),
            bot(
                "fpl-bot",
                &agents
                    .iter()
                    .find(|agent| agent.key == "fleet-pm-fpl-v1")
                    .unwrap()
                    .name,
                "run:spawn_bot:hash",
                Some("orch-bot"),
            ),
            bot("neo-bot", &global, "run:spawn_bot:hash", Some("orch-bot")),
        ];
        let mut pages = HashMap::new();
        pages.insert(
            "fpl-bot".to_string(),
            vec![message("m1", 1, "user", "studio")],
        );
        pages.insert("neo-bot".to_string(), vec![message("m2", 1, "user", "neo")]);
        let dir = TempDir::new().unwrap();
        let mut supervisor = supervisor(
            &dir,
            FakeSource::with(bots, pages),
            FakeDeliverer::default(),
        );
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 1, "only the Studio PM");
        assert_eq!(report.foreign.len(), 1);
        assert!(report.foreign[0].starts_with("fleet-pm-marbles-v1@agora-one"));
        assert_eq!(supervisor.deliverer.calls().len(), 1);
        assert_eq!(supervisor.deliverer.calls()[0].2, "pm-fpl");
        assert_eq!(supervisor.deliverer.calls()[0].3.as_deref(), Some("%40"));
        assert!(supervisor.tracked("fleet-pm-marbles-v1").is_none());
    }

    #[tokio::test]
    async fn per_agent_state_files_are_independent_and_host_scoped() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert("orch-bot".to_string(), vec![message("m1", 1, "user", "a")]);
        pages.insert("fpl-bot".to_string(), vec![message("m1", 1, "user", "a")]);
        let dir = TempDir::new().unwrap();
        let mut supervisor = supervisor(
            &dir,
            FakeSource::with(bots, pages),
            FakeDeliverer::default(),
        );
        supervisor.cycle(&agents).await.unwrap();
        for key in ["fleet-orch-v1", "fleet-pm-fpl-v1", "fleet-pm-marbles-v1"] {
            assert!(
                dir.path().join(STUDIO).join(format!("{key}.json")).exists(),
                "host-scoped state file for {key}"
            );
        }
        assert_eq!(
            supervisor.tracked("fleet-orch-v1").unwrap().state.last_seq,
            1
        );
        assert_eq!(
            supervisor
                .tracked("fleet-pm-marbles-v1")
                .unwrap()
                .state
                .last_seq,
            0
        );
    }

    #[test]
    fn a_legacy_state_file_is_migrated_not_replayed() {
        let dir = TempDir::new().unwrap();
        let legacy = dir.path().join("fleet-orch-v1.json");
        std::fs::write(&legacy, r#"{"bot_id":"b1","thread_id":"t1","last_seq":42}"#).unwrap();
        assert!(migrate_legacy_state(dir.path(), STUDIO, "fleet-orch-v1").unwrap());
        let scoped = state_file(dir.path(), STUDIO, "fleet-orch-v1").unwrap();
        assert!(scoped.exists());
        assert!(!migrate_legacy_state(dir.path(), STUDIO, "fleet-orch-v1").unwrap());
        let state = crate::orch::watch::WatchState::load(Some(&scoped)).unwrap();
        assert_eq!(state.last_seq, 42, "migration keeps the watermark");
    }

    #[tokio::test]
    async fn a_missing_pm_is_reported_and_never_faked() {
        let rows = vec![row("pm-fpl", "pm-fpl", "%40", "working")];
        let agents = agents(&rows, true, STUDIO).unwrap();
        let dir = TempDir::new().unwrap();
        let source = FakeSource::with(
            vec![bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None)],
            HashMap::new(),
        );
        let mut supervisor = supervisor(&dir, source, FakeDeliverer::default());
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.missing, vec!["fleet-pm-fpl-v1"]);
        assert!(report.created.iter().all(|key| key != "fleet-pm-fpl-v1"));
        assert!(
            supervisor
                .source
                .created()
                .iter()
                .all(|spec| !spec.spawn_key.starts_with("fleet-pm-"))
        );
    }

    #[tokio::test]
    async fn the_root_is_created_once_through_the_idempotent_path() {
        let dir = TempDir::new().unwrap();
        let agents = agents(&[], true, STUDIO).unwrap();
        let mut supervisor = supervisor(
            &dir,
            FakeSource::with(Vec::new(), HashMap::new()),
            FakeDeliverer::default(),
        );
        let first = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(first.created, vec!["fleet-orch-v1"]);
        let created = supervisor.source.created();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].spawn_key, "fleet-orch-v1");
        assert!(
            created[0].relay_only,
            "every bot this slice creates is relay-only"
        );

        let second = supervisor.cycle(&agents).await.unwrap();
        assert!(second.created.is_empty(), "idempotent: no second root");
        assert_eq!(supervisor.source.created().len(), 1);
    }

    #[tokio::test]
    async fn workers_appear_and_disappear_without_carrying_a_token() {
        let dir = TempDir::new().unwrap();
        let rows_with_worker = vec![row("fab-cicd", "fleet", "%112", "working")];
        let shown = agents(&rows_with_worker, false, STUDIO).unwrap();
        let hidden = agents(&rows_with_worker, true, STUDIO).unwrap();
        let source = FakeSource::with(
            vec![bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None)],
            HashMap::new(),
        );
        let mut supervisor = supervisor(&dir, source, FakeDeliverer::default());

        let report = supervisor.cycle(&shown).await.unwrap();
        assert!(
            report
                .created
                .contains(&"fleet-worker-fab-cicd-v1".to_string())
        );
        let created = supervisor.source.created();
        let worker = created
            .iter()
            .find(|spec| spec.spawn_key == "fleet-worker-fab-cicd-v1")
            .expect("worker bot created");
        assert!(worker.relay_only);

        // hide_workers=true: the worker leaves the derived roster, so its bot is
        // archived. The worker key is name-derived, so a future worker with a
        // different name cannot inherit this identity or its token file.
        let report = supervisor.cycle(&hidden).await.unwrap();
        assert_eq!(
            report.archived,
            vec!["fleet-worker-fab-cicd-v1".to_string()]
        );
        assert_eq!(
            supervisor.source.archived(),
            vec!["created-fleet-worker-fab-cicd-v1".to_string()]
        );
        assert!(supervisor.tracked("fleet-worker-fab-cicd-v1").is_none());
    }

    #[tokio::test]
    async fn a_foreign_host_worker_bot_is_never_archived_by_studio() {
        let dir = TempDir::new().unwrap();
        let agents = agents(&[], true, STUDIO).unwrap();
        let source = FakeSource::with(
            vec![
                bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None),
                bot(
                    "neo-worker",
                    "fleet-worker-neo-job-v1",
                    "fleet-worker-neo-job-v1",
                    None,
                ),
            ],
            HashMap::new(),
        );
        let mut supervisor = supervisor(&dir, source, FakeDeliverer::default());
        let report = supervisor.cycle(&agents).await.unwrap();
        assert!(report.archived.is_empty(), "not ours to archive");
        assert!(supervisor.source.archived().is_empty());
    }

    #[tokio::test]
    async fn a_gone_worker_is_archived_not_delivered_to() {
        let dir = TempDir::new().unwrap();
        let rows = vec![row("fab-cicd", "fleet", "%112", "gone")];
        let agents = agents(&rows, false, STUDIO).unwrap();
        let source = FakeSource::with(
            vec![
                bot("orch-bot", "Fleet Orchestrator", "fleet-orch-v1", None),
                bot(
                    "worker-bot",
                    "fleet-worker-fab-cicd-v1",
                    "fleet-worker-fab-cicd-v1",
                    None,
                ),
            ],
            HashMap::new(),
        );
        let mut supervisor = supervisor(&dir, source, FakeDeliverer::default());
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(
            report.archived,
            vec!["fleet-worker-fab-cicd-v1".to_string()]
        );
        assert_eq!(report.delivered, 0);
    }

    #[test]
    fn the_default_roster_is_orch_pms_and_muse_only() {
        let rows = vec![
            row("pm-fpl", "pm-fpl", "%40", "working"),
            row("pm-marbles", "pm-marbles", "%31", "working"),
            row("fab-cicd", "fleet", "%112", "working"),
        ];
        let agents = agents(&rows, true, STUDIO).unwrap();
        let keys: Vec<_> = agents.iter().map(|agent| agent.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "fleet-orch-v1",
                "muse",
                "fleet-pm-fpl-v1",
                "fleet-pm-marbles-v1"
            ]
        );
    }

    #[tokio::test]
    async fn a_delivery_failure_does_not_advance_that_agents_watermark() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert("orch-bot".to_string(), vec![message("m1", 7, "user", "x")]);
        let dir = TempDir::new().unwrap();
        let mut supervisor = supervisor(
            &dir,
            FakeSource::with(bots, pages),
            FakeDeliverer {
                calls: Mutex::new(Vec::new()),
                fail: true,
                defer: false,
            },
        );
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 0);
        assert!(report.errors.iter().any(|e| e.contains("fleet-orch-v1")));
        let tracked = supervisor.tracked("fleet-orch-v1").unwrap();
        assert_eq!(tracked.state.last_seq, 0, "watermark stays put");
        assert_eq!(tracked.state.last_poll_ok, Some(false));
        let saved: WatchState =
            serde_json::from_str(&std::fs::read_to_string(&tracked.state_file).unwrap()).unwrap();
        assert_eq!(saved.last_seq, 0);
    }

    #[tokio::test]
    async fn a_deferred_delivery_retains_the_watermark_for_a_retry() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert("orch-bot".to_string(), vec![message("m1", 7, "user", "x")]);
        let dir = TempDir::new().unwrap();
        let mut supervisor = supervisor(
            &dir,
            FakeSource::with(bots, pages),
            FakeDeliverer {
                calls: Mutex::new(Vec::new()),
                fail: false,
                defer: true,
            },
        );
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.deferred, 1);
        assert_eq!(report.delivered, 0);
        let tracked = supervisor.tracked("fleet-orch-v1").unwrap();
        assert_eq!(tracked.state.last_seq, 0);
        assert!(!tracked.state.delivered("m1"));
    }

    fn seen_supervisor(
        dir: &TempDir,
        source: FakeSource,
        reaction: &str,
    ) -> Supervisor<FakeSource, FakeDeliverer> {
        supervisor(dir, source, FakeDeliverer::default())
            .with_seen_reaction(reaction)
            .with_reply_hint_config("/etc/rakazo/agentsmith.toml")
    }

    #[tokio::test]
    async fn a_first_delivery_posts_exactly_one_seen_tapback() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert("orch-bot".to_string(), vec![message("m1", 7, "user", "x")]);
        let dir = TempDir::new().unwrap();
        let mut supervisor = seen_supervisor(&dir, FakeSource::with(bots, pages), "👍");
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(report.seen, 1);
        assert_eq!(report.seen_failed, 0);
        let reacts = supervisor.source.reacts();
        assert_eq!(reacts.len(), 1, "one add per message lifetime");
        assert_eq!(reacts[0].0, "orch-bot");
        assert_eq!(reacts[0].1, "m1");
        assert_eq!(reacts[0].2, "👍");
        assert_eq!(
            reacts[0].3, "seen:m1",
            "idempotent nonce from the message id"
        );

        // A replay of the same row is a duplicate and must not react again.
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.duplicates, 1);
        assert_eq!(supervisor.source.reacts().len(), 1);
    }

    #[tokio::test]
    async fn no_seen_tapback_on_duplicates_echoes_or_bot_rows() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert(
            "orch-bot".to_string(),
            vec![
                message("m1", 1, "user", "first"),
                message("m2", 2, "bot", "not ours"),
                message("m1", 3, "user", "first"),
            ],
        );
        let dir = TempDir::new().unwrap();
        let mut supervisor = seen_supervisor(&dir, FakeSource::with(bots, pages), "👍");
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(report.duplicates, 1);
        assert_eq!(report.ignored, 1);
        assert_eq!(supervisor.source.reacts().len(), 1);
        assert_eq!(supervisor.source.reacts()[0].1, "m1");
    }

    #[tokio::test]
    async fn a_reaction_failure_does_not_change_counters_or_the_watermark() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert("orch-bot".to_string(), vec![message("m1", 7, "user", "x")]);
        let dir = TempDir::new().unwrap();
        let source = FakeSource {
            react_fail: true,
            ..FakeSource::with(bots, pages)
        };
        let mut supervisor = seen_supervisor(&dir, source, "👍");
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 1, "delivery still counts");
        assert_eq!(report.duplicates, 0);
        assert_eq!(report.seen, 0);
        assert_eq!(report.seen_failed, 1);
        assert_eq!(
            supervisor.tracked("fleet-orch-v1").unwrap().state.last_seq,
            7,
            "a failed tapback never rewinds the watermark"
        );
        assert_eq!(supervisor.deliverer.calls().len(), 1);
    }

    #[tokio::test]
    async fn an_empty_seen_reaction_makes_zero_react_calls() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert("orch-bot".to_string(), vec![message("m1", 7, "user", "x")]);
        let dir = TempDir::new().unwrap();
        let mut supervisor = seen_supervisor(&dir, FakeSource::with(bots, pages), "");
        let report = supervisor.cycle(&agents).await.unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(report.seen, 0);
        assert_eq!(report.seen_failed, 0);
        assert!(supervisor.source.reacts().is_empty());
    }

    #[tokio::test]
    async fn delivered_text_carries_the_exact_reply_command() {
        let (agents, bots) = fixture();
        let mut pages = HashMap::new();
        pages.insert("orch-bot".to_string(), vec![message("m1", 7, "user", "x")]);
        let dir = TempDir::new().unwrap();
        let mut supervisor = seen_supervisor(&dir, FakeSource::with(bots, pages), "");
        supervisor.cycle(&agents).await.unwrap();
        let text = &supervisor.deliverer.calls()[0].4;
        assert!(
            text.contains("[rakazo:thread-orch-bot:m1]"),
            "reference prefix kept: {text}"
        );
        assert!(
            text.contains(
                "[reply: rakazo-beeper bridge --bridge-config /etc/rakazo/agentsmith.toml reply --agent fleet-orch-v1 --reply-to m1 --text \"<answer>\"]"
            ),
            "reply hint missing: {text}"
        );
    }

    #[tokio::test]
    async fn a_failing_bot_list_is_fatal_for_the_cycle() {
        let (agents, _) = fixture();
        let dir = TempDir::new().unwrap();
        let source = FakeSource {
            bots_fail: true,
            ..FakeSource::default()
        };
        let mut supervisor = supervisor(&dir, source, FakeDeliverer::default());
        assert!(supervisor.cycle(&agents).await.is_err());
    }
}
