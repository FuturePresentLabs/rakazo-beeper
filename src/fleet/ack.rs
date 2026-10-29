//! Acknowledgement discipline: an ack is a promise, and a promise is a record.
//!
//! Tapping back every message is noise, and it trains the human to ignore
//! acknowledgements. The rule the bridge follows instead:
//!
//! * An acknowledgement is a **promise** — "I saw this and I will follow up" —
//!   never a receipt.
//! * A promise may only be made **mid-thread**: the agent is already working
//!   on a prior task when the new message arrives. An idle agent answers; it
//!   does not acknowledge.
//! * A promise creates an **obligation** in a durable ledger, and a hook
//!   enforces it: one nudge, then one visible admission when the promise is
//!   broken.
//!
//! The obligation identity is the `ack_id`, derived from the agent, the thread
//! and the acked message. It is stable: a retry reuses it, so a sweep can
//! never mint a second row for the same promise, and the admission notice is a
//! pure function of the obligation so it is byte-identical on every attempt.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};

/// The tapback used for an acknowledgement. It is one of Rakazo's six
/// server-validated reactions; the allowlist itself is never widened here.
pub const ACK_REACTION: &str = "👍";

/// When the bridge may acknowledge an inbound human message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckPolicy {
    /// Never acknowledge. The reply is the only response.
    Never,
    /// Acknowledge only while the agent is mid-thread on a prior task.
    #[default]
    MidThreadOnly,
    /// Acknowledge every inbound message.
    Always,
}

impl AckPolicy {
    /// The stable configuration spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::MidThreadOnly => "mid_thread_only",
            Self::Always => "always",
        }
    }

    /// Parse an explicit policy name. An unknown name is an error: the bridge
    /// must never guess between `never` and `always`.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim() {
            "never" => Ok(Self::Never),
            "mid_thread_only" => Ok(Self::MidThreadOnly),
            "always" => Ok(Self::Always),
            other => {
                bail!("unknown ack_policy {other:?}: expected never, mid_thread_only or always")
            }
        }
    }
}

impl std::str::FromStr for AckPolicy {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl std::fmt::Display for AckPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The life of one acknowledgement promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObligationStatus {
    /// The promise is outstanding.
    Open,
    /// The agent replied in the thread; the promise was kept.
    Fulfilled,
    /// The promise was broken and admitted to the human.
    Expired,
}

impl ObligationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Fulfilled => "fulfilled",
            Self::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "open" => Ok(Self::Open),
            "fulfilled" => Ok(Self::Fulfilled),
            "expired" => Ok(Self::Expired),
            other => bail!("unknown obligation status {other:?}"),
        }
    }
}

/// One acknowledged message and the follow-up it owes.
///
/// `ack_id` is the identity of the promise; every other field describes it.
/// `seq` is the acked message's position in the agent's thread and orders the
/// promise against the agent's replies: a reply only fulfils an obligation
/// that was opened before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obligation {
    /// Stable identity of this promise.
    pub ack_id: String,
    /// The acknowledged inbound message.
    pub message_id: String,
    /// The thread the human reads (the room the agent answers in).
    pub thread: String,
    /// The agent that owes the follow-up.
    pub agent: String,
    /// Unix seconds when the acknowledgement was sent.
    pub created_at: i64,
    /// Unix seconds when the follow-up is due.
    pub due_by: i64,
    /// The acked message's position in the agent's thread.
    pub seq: i64,
    /// Open, fulfilled or expired.
    pub status: ObligationStatus,
    /// The reply that kept the promise (a message id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulfilled_by: Option<String>,
    /// The run that kept the promise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Unix seconds of the single nudge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudged_at: Option<i64>,
    /// Unix seconds the admission notice was delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noticed_at: Option<i64>,
    /// How many times the admission notice was attempted.
    #[serde(default)]
    pub notice_attempts: u32,
}

/// The stable identity of one acknowledgement.
///
/// Derived from the agent, the thread and the acked message: a retry of the
/// same message reuses the id, so the promise is recorded once and the
/// admission notice keeps one identity for its whole life.
pub fn ack_id(agent: &str, thread: &str, message_id: &str) -> String {
    let digest = crate::stable_id(&["ack-v1", agent, thread, message_id]);
    format!("ack-{}", &digest[..24])
}

/// The visible admission for a broken promise.
///
/// The text is a pure function of the obligation: no clock, no counter, no
/// per-tick wording. A notice that renamed itself every tick would look like a
/// new admission each time and would mint a row per minute.
pub fn admission_notice(obligation: &Obligation) -> String {
    format!(
        "I acknowledged your message and then did not follow up ({}). \
         The acknowledgement was a promise, and I did not keep it.",
        obligation.ack_id
    )
}

/// Durable storage for acknowledgement promises.
///
/// The ledger, not the transcript, is authoritative: a restart, a duplicate
/// delivery or a second sweep must never invent a promise.
pub trait ObligationLedger {
    /// Record a promise. Recording the same `ack_id` twice is a replay, not a
    /// second promise.
    fn record(&mut self, obligation: &Obligation) -> Result<()>;

    /// One obligation by identity.
    fn obligation(&self, ack_id: &str) -> Result<Option<Obligation>>;

    /// Every open promise for one agent in one thread.
    fn open_in_thread(&self, agent: &str, thread: &str) -> Result<Vec<Obligation>>;

    /// Every open promise whose due time has passed.
    fn due(&self, now: i64) -> Result<Vec<Obligation>>;

    /// Record the single nudge.
    fn mark_nudged(&mut self, ack_id: &str, at: i64) -> Result<()>;

    /// Close a promise that was kept.
    fn mark_fulfilled(&mut self, ack_id: &str, reply: &str, run_id: Option<&str>) -> Result<()>;

    /// Close a promise that was broken. Returns `false` when it was already
    /// closed, so the admission is raised at most once.
    fn mark_expired(&mut self, ack_id: &str) -> Result<bool>;

    /// Record that the admission notice was delivered.
    fn mark_noticed(&mut self, ack_id: &str, at: i64) -> Result<()>;

    /// Record a failed admission attempt.
    fn mark_notice_attempt(&mut self, ack_id: &str) -> Result<()>;

    /// Drop a promise that was never made (the acknowledgement itself failed).
    fn remove(&mut self, ack_id: &str) -> Result<()>;
}

/// An in-memory ledger. The hermetic tests use it; production uses the
/// SQLite ledger in the bridge store.
#[derive(Clone, Debug, Default)]
pub struct MemoryLedger {
    rows: BTreeMap<String, Obligation>,
}

impl MemoryLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every obligation, open or closed.
    pub fn all(&self) -> Vec<Obligation> {
        self.rows.values().cloned().collect()
    }

    /// The open obligations.
    pub fn open(&self) -> Vec<Obligation> {
        self.rows
            .values()
            .filter(|obligation| obligation.status == ObligationStatus::Open)
            .cloned()
            .collect()
    }
}

impl ObligationLedger for MemoryLedger {
    fn record(&mut self, obligation: &Obligation) -> Result<()> {
        self.rows
            .entry(obligation.ack_id.clone())
            .or_insert_with(|| obligation.clone());
        Ok(())
    }

    fn obligation(&self, ack_id: &str) -> Result<Option<Obligation>> {
        Ok(self.rows.get(ack_id).cloned())
    }

    fn open_in_thread(&self, agent: &str, thread: &str) -> Result<Vec<Obligation>> {
        Ok(self
            .rows
            .values()
            .filter(|obligation| {
                obligation.status == ObligationStatus::Open
                    && obligation.agent == agent
                    && obligation.thread == thread
            })
            .cloned()
            .collect())
    }

    fn due(&self, now: i64) -> Result<Vec<Obligation>> {
        Ok(self
            .rows
            .values()
            .filter(|obligation| {
                obligation.status == ObligationStatus::Open && obligation.due_by <= now
            })
            .cloned()
            .collect())
    }

    fn mark_nudged(&mut self, ack_id: &str, at: i64) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id) {
            obligation.nudged_at = Some(at);
        }
        Ok(())
    }

    fn mark_fulfilled(&mut self, ack_id: &str, reply: &str, run_id: Option<&str>) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id)
            && obligation.status == ObligationStatus::Open
        {
            obligation.status = ObligationStatus::Fulfilled;
            obligation.fulfilled_by = Some(reply.to_string());
            obligation.run_id = run_id.map(str::to_string);
        }
        Ok(())
    }

    fn mark_expired(&mut self, ack_id: &str) -> Result<bool> {
        let Some(obligation) = self.rows.get_mut(ack_id) else {
            return Ok(false);
        };
        if obligation.status != ObligationStatus::Open {
            return Ok(false);
        }
        obligation.status = ObligationStatus::Expired;
        Ok(true)
    }

    fn mark_noticed(&mut self, ack_id: &str, at: i64) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id) {
            obligation.noticed_at = Some(at);
        }
        Ok(())
    }

    fn mark_notice_attempt(&mut self, ack_id: &str) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id) {
            obligation.notice_attempts += 1;
        }
        Ok(())
    }

    fn remove(&mut self, ack_id: &str) -> Result<()> {
        self.rows.remove(ack_id);
        Ok(())
    }
}

/// A durable ledger in one JSON file.
///
/// This is the agent-bridge ledger: the promises of one agent live beside that
/// agent's watch state, so they survive a restart with it. The file is written
/// atomically and one row per `ack_id`, so a sweep can never mint a row per
/// tick.
#[derive(Clone, Debug, Default)]
pub struct FileLedger {
    path: PathBuf,
    rows: BTreeMap<String, Obligation>,
}

impl FileLedger {
    /// Open (or start) the ledger at `path`.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let rows = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let rows: Vec<Obligation> = serde_json::from_str(&text).with_context(|| {
                    format!("invalid acknowledgement ledger {}", path.display())
                })?;
                rows.into_iter()
                    .map(|row| (row.ack_id.clone(), row))
                    .collect()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read acknowledgement ledger {}", path.display()));
            }
        };
        Ok(Self { path, rows })
    }

    /// The ledger path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every obligation, open or closed.
    pub fn all(&self) -> Vec<Obligation> {
        self.rows.values().cloned().collect()
    }

    /// The open obligations.
    pub fn open(&self) -> Vec<Obligation> {
        self.rows
            .values()
            .filter(|obligation| obligation.status == ObligationStatus::Open)
            .cloned()
            .collect()
    }

    /// Write the ledger atomically. A ledger that cannot be written is an
    /// error: an unrecordable promise must not be made.
    fn save(&self) -> Result<()> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create acknowledgement ledger dir {}", parent.display())
            })?;
        }
        let rows: Vec<Obligation> = self.rows.values().cloned().collect();
        let text = serde_json::to_string_pretty(&rows)
            .with_context(|| format!("encode acknowledgement ledger {}", self.path.display()))?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text)
            .with_context(|| format!("write acknowledgement ledger {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replace acknowledgement ledger {}", self.path.display()))?;
        Ok(())
    }
}

impl ObligationLedger for FileLedger {
    fn record(&mut self, obligation: &Obligation) -> Result<()> {
        if self.rows.contains_key(&obligation.ack_id) {
            return Ok(());
        }
        self.rows
            .insert(obligation.ack_id.clone(), obligation.clone());
        self.save()
    }

    fn obligation(&self, ack_id: &str) -> Result<Option<Obligation>> {
        Ok(self.rows.get(ack_id).cloned())
    }

    fn open_in_thread(&self, agent: &str, thread: &str) -> Result<Vec<Obligation>> {
        Ok(self
            .rows
            .values()
            .filter(|obligation| {
                obligation.status == ObligationStatus::Open
                    && obligation.agent == agent
                    && obligation.thread == thread
            })
            .cloned()
            .collect())
    }

    fn due(&self, now: i64) -> Result<Vec<Obligation>> {
        Ok(self
            .rows
            .values()
            .filter(|obligation| {
                obligation.status == ObligationStatus::Open && obligation.due_by <= now
            })
            .cloned()
            .collect())
    }

    fn mark_nudged(&mut self, ack_id: &str, at: i64) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id) {
            obligation.nudged_at = Some(at);
            self.save()?;
        }
        Ok(())
    }

    fn mark_fulfilled(&mut self, ack_id: &str, reply: &str, run_id: Option<&str>) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id)
            && obligation.status == ObligationStatus::Open
        {
            obligation.status = ObligationStatus::Fulfilled;
            obligation.fulfilled_by = Some(reply.to_string());
            obligation.run_id = run_id.map(str::to_string);
            self.save()?;
        }
        Ok(())
    }

    fn mark_expired(&mut self, ack_id: &str) -> Result<bool> {
        let Some(obligation) = self.rows.get_mut(ack_id) else {
            return Ok(false);
        };
        if obligation.status != ObligationStatus::Open {
            return Ok(false);
        }
        obligation.status = ObligationStatus::Expired;
        self.save()?;
        Ok(true)
    }

    fn mark_noticed(&mut self, ack_id: &str, at: i64) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id) {
            obligation.noticed_at = Some(at);
            self.save()?;
        }
        Ok(())
    }

    fn mark_notice_attempt(&mut self, ack_id: &str) -> Result<()> {
        if let Some(obligation) = self.rows.get_mut(ack_id) {
            obligation.notice_attempts += 1;
            self.save()?;
        }
        Ok(())
    }

    fn remove(&mut self, ack_id: &str) -> Result<()> {
        if self.rows.remove(ack_id).is_some() {
            self.save()?;
        }
        Ok(())
    }
}

/// The side effects an acknowledgement needs.
/// `acknowledge` is the visible tapback, `nudge` is the single reminder and
/// `admit` is the visible admission that a promise was broken.
pub trait AckEffects {
    /// Deliver the tapback that says "I saw this and I will follow up".
    fn acknowledge(&self, obligation: &Obligation) -> impl Future<Output = Result<()>> + Send;

    /// Remind the agent once that it owes a follow-up.
    fn nudge(&self, obligation: &Obligation) -> impl Future<Output = Result<()>> + Send;

    /// Tell the human, in the thread, that the promise was broken.
    fn admit(
        &self,
        obligation: &Obligation,
        notice: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// One inbound message the bridge is considering acknowledging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AckRequest {
    /// The agent that received the message.
    pub agent: String,
    /// The thread the human reads.
    pub thread: String,
    /// The inbound message.
    pub message_id: String,
    /// The message's position in the agent's thread.
    pub seq: i64,
    /// True when the agent is already working on a prior task.
    pub busy: bool,
    /// False when the agent cannot answer in this thread. The bridge never
    /// acknowledges a channel it cannot follow up on.
    pub can_reply: bool,
    /// The run the inbound message started, when it started one.
    pub run_id: Option<String>,
}

/// Why an acknowledgement was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AckBlock {
    /// The policy forbids acknowledgements.
    Policy(AckPolicy),
    /// The agent is idle: the reply is the response.
    NotBusy,
    /// The agent cannot answer in this thread.
    NoReplyChannel,
    /// This message was already acknowledged.
    AlreadyAcked,
    /// The promise could not be recorded, so it was not made.
    LedgerUnavailable(String),
    /// The tapback itself failed, so no promise was made.
    AckFailed(String),
}

impl AckBlock {
    /// A stable, human-readable reason for the log.
    pub fn reason(&self) -> String {
        match self {
            Self::Policy(policy) => format!("policy={policy} forbids an acknowledgement"),
            Self::NotBusy => {
                "agent is idle: the reply is the response, not an acknowledgement".to_string()
            }
            Self::NoReplyChannel => {
                "agent cannot reply in this thread: no acknowledgement".to_string()
            }
            Self::AlreadyAcked => "this message was already acknowledged".to_string(),
            Self::LedgerUnavailable(error) => {
                format!("ledger write failed, so no promise was made: {error}")
            }
            Self::AckFailed(error) => {
                format!("acknowledgement failed, so no promise was made: {error}")
            }
        }
    }
}

/// The outcome of considering one inbound message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AckDecision {
    /// The promise was made and recorded.
    Allowed,
    /// No promise was made.
    Blocked(AckBlock),
}

impl AckDecision {
    pub fn allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// The result of considering one inbound message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AckOutcome {
    /// Whether the promise was made.
    pub decision: AckDecision,
    /// The identity of the promise, when one was made.
    pub ack_id: Option<String>,
    /// Why the policy allowed or blocked the acknowledgement.
    pub reason: String,
}

/// What one sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Promises nudged once.
    pub nudged: usize,
    /// Promises expired after the grace period.
    pub expired: usize,
    /// Visible admissions raised.
    pub admitted: usize,
    /// Failures, each with its reason.
    pub errors: Vec<String>,
}

/// The acknowledgement keeper: the policy, the ledger and the hook.
pub struct Keeper<L: ObligationLedger, E: AckEffects + Sync> {
    ledger: L,
    effects: E,
    policy: AckPolicy,
    due_seconds: i64,
    grace_seconds: i64,
    /// How often a failed admission is retried before the promise is closed
    /// without a notice. The retry is bounded; it is never a loop.
    notice_attempts: u32,
}

impl<L: ObligationLedger, E: AckEffects + Sync> Keeper<L, E> {
    /// The keeper with the default schedule: a promise is due after
    /// `due_seconds` and is admitted after `grace_seconds` more.
    pub fn new(
        ledger: L,
        effects: E,
        policy: AckPolicy,
        due_seconds: i64,
        grace_seconds: i64,
    ) -> Self {
        Self {
            ledger,
            effects,
            policy,
            due_seconds: due_seconds.max(0),
            grace_seconds: grace_seconds.max(0),
            notice_attempts: 3,
        }
    }

    /// Bound the number of admission attempts.
    pub fn with_notice_attempts(mut self, attempts: u32) -> Self {
        self.notice_attempts = attempts.max(1);
        self
    }

    pub fn policy(&self) -> AckPolicy {
        self.policy
    }

    /// Consider one inbound message.
    ///
    /// The promise is recorded **before** the tapback is sent. A promise that
    /// cannot be recorded is never made: the ledger is what the hook enforces,
    /// and an unrecordable promise would be a promise nobody keeps.
    pub async fn consider(&mut self, request: &AckRequest, now: i64) -> AckOutcome {
        let blocked = |block: AckBlock| AckOutcome {
            decision: AckDecision::Blocked(block.clone()),
            ack_id: None,
            reason: block.reason(),
        };

        if self.policy == AckPolicy::Never {
            let outcome = blocked(AckBlock::Policy(AckPolicy::Never));
            tracing::debug!(
                agent = %request.agent,
                thread = %request.thread,
                reason = %outcome.reason,
                "no acknowledgement"
            );
            return outcome;
        }
        if !request.can_reply {
            let outcome = blocked(AckBlock::NoReplyChannel);
            tracing::debug!(
                agent = %request.agent,
                thread = %request.thread,
                reason = %outcome.reason,
                "no acknowledgement"
            );
            return outcome;
        }
        if self.policy == AckPolicy::MidThreadOnly && !request.busy {
            let outcome = blocked(AckBlock::NotBusy);
            tracing::debug!(
                agent = %request.agent,
                thread = %request.thread,
                reason = %outcome.reason,
                "no acknowledgement"
            );
            return outcome;
        }

        let ack_id = ack_id(&request.agent, &request.thread, &request.message_id);
        match self.ledger.obligation(&ack_id) {
            Ok(Some(_)) => {
                let outcome = blocked(AckBlock::AlreadyAcked);
                tracing::debug!(
                    ack_id = %ack_id,
                    reason = %outcome.reason,
                    "no acknowledgement"
                );
                return outcome;
            }
            Ok(None) => {}
            Err(error) => {
                let outcome = blocked(AckBlock::LedgerUnavailable(error.to_string()));
                tracing::warn!(
                    ack_id = %ack_id,
                    reason = %outcome.reason,
                    "no acknowledgement"
                );
                return outcome;
            }
        }

        let obligation = Obligation {
            ack_id: ack_id.clone(),
            message_id: request.message_id.clone(),
            thread: request.thread.clone(),
            agent: request.agent.clone(),
            created_at: now,
            due_by: now + self.due_seconds,
            seq: request.seq,
            status: ObligationStatus::Open,
            fulfilled_by: None,
            run_id: request.run_id.clone(),
            nudged_at: None,
            noticed_at: None,
            notice_attempts: 0,
        };
        if let Err(error) = self.ledger.record(&obligation) {
            let outcome = blocked(AckBlock::LedgerUnavailable(error.to_string()));
            tracing::warn!(
                ack_id = %ack_id,
                reason = %outcome.reason,
                "no acknowledgement"
            );
            return outcome;
        }
        if let Err(error) = self.effects.acknowledge(&obligation).await {
            // The tapback never landed, so no promise was made: drop the row
            // instead of owing a follow-up nobody was told about.
            let _ = self.ledger.remove(&ack_id);
            let outcome = blocked(AckBlock::AckFailed(error.to_string()));
            tracing::warn!(
                ack_id = %ack_id,
                reason = %outcome.reason,
                "no acknowledgement"
            );
            return outcome;
        }
        tracing::info!(
            ack_id = %ack_id,
            agent = %request.agent,
            thread = %request.thread,
            due_by = obligation.due_by,
            "acknowledged: the tapback is a promise to follow up"
        );
        AckOutcome {
            decision: AckDecision::Allowed,
            ack_id: Some(ack_id),
            reason: format!(
                "policy={} and the agent is mid-thread: acknowledged as a promise",
                self.policy
            ),
        }
    }

    /// Close the promises an agent's reply kept.
    ///
    /// The thread and the ordering are enough: the reply must come after the
    /// acknowledged message. It does not have to quote it.
    pub async fn fulfil(
        &mut self,
        agent: &str,
        thread: &str,
        reply: &str,
        run_id: Option<&str>,
        reply_seq: i64,
    ) -> Result<usize> {
        let mut fulfilled = 0;
        for obligation in self.ledger.open_in_thread(agent, thread)? {
            if obligation.seq >= reply_seq {
                continue;
            }
            self.ledger
                .mark_fulfilled(&obligation.ack_id, reply, run_id)?;
            fulfilled += 1;
            tracing::info!(
                ack_id = %obligation.ack_id,
                agent = %agent,
                thread = %thread,
                reply = %reply,
                "acknowledgement kept"
            );
        }
        Ok(fulfilled)
    }

    /// The hook: enforce every promise that is due.
    ///
    /// One nudge per promise, never a loop. After the grace period the
    /// promise is expired and the human is told, once, in the thread.
    pub async fn sweep(&mut self, now: i64) -> Result<SweepReport> {
        let mut report = SweepReport::default();
        for obligation in self.ledger.due(now)? {
            if obligation.noticed_at.is_some() {
                // The admission was delivered; only the closing update is
                // left. Never admit it twice.
                self.ledger.mark_expired(&obligation.ack_id)?;
                continue;
            }
            let admit_at = obligation.due_by + self.grace_seconds;
            if now < admit_at {
                if obligation.nudged_at.is_none() {
                    match self.effects.nudge(&obligation).await {
                        Ok(()) => {
                            self.ledger.mark_nudged(&obligation.ack_id, now)?;
                            report.nudged += 1;
                            tracing::info!(
                                ack_id = %obligation.ack_id,
                                agent = %obligation.agent,
                                thread = %obligation.thread,
                                "acknowledged message is due: nudged once"
                            );
                        }
                        Err(error) => {
                            report
                                .errors
                                .push(format!("{}: nudge failed: {error:#}", obligation.ack_id));
                        }
                    }
                }
                continue;
            }
            // The promise is broken: admit it once, in the thread.
            let notice = admission_notice(&obligation);
            match self.effects.admit(&obligation, &notice).await {
                Ok(()) => {
                    self.ledger.mark_noticed(&obligation.ack_id, now)?;
                    if self.ledger.mark_expired(&obligation.ack_id)? {
                        report.expired += 1;
                        report.admitted += 1;
                    }
                    tracing::warn!(
                        ack_id = %obligation.ack_id,
                        agent = %obligation.agent,
                        thread = %obligation.thread,
                        "acknowledged message was never followed up: admitted"
                    );
                }
                Err(error) => {
                    self.ledger.mark_notice_attempt(&obligation.ack_id)?;
                    let attempts = self
                        .ledger
                        .obligation(&obligation.ack_id)?
                        .map(|row| row.notice_attempts)
                        .unwrap_or_default();
                    if attempts >= self.notice_attempts {
                        // Bounded: the promise is closed without a notice
                        // rather than retried forever.
                        self.ledger.mark_expired(&obligation.ack_id)?;
                        report.errors.push(format!(
                            "{}: admission failed {attempts} times, closing without a notice: {error:#}",
                            obligation.ack_id
                        ));
                    } else {
                        report.errors.push(format!(
                            "{}: admission failed, will retry once: {error:#}",
                            obligation.ack_id
                        ));
                    }
                }
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeEffects {
        acknowledged: Mutex<Vec<String>>,
        nudged: Mutex<Vec<String>>,
        admitted: Mutex<Vec<String>>,
        notices: Mutex<Vec<String>>,
        fail_ack: bool,
        fail_nudge: bool,
        fail_admit: bool,
    }

    impl FakeEffects {
        fn acknowledged(&self) -> Vec<String> {
            self.acknowledged.lock().unwrap().clone()
        }
        fn nudged(&self) -> Vec<String> {
            self.nudged.lock().unwrap().clone()
        }
        fn admitted(&self) -> Vec<String> {
            self.admitted.lock().unwrap().clone()
        }
        fn notices(&self) -> Vec<String> {
            self.notices.lock().unwrap().clone()
        }
    }

    impl AckEffects for FakeEffects {
        async fn acknowledge(&self, obligation: &Obligation) -> Result<()> {
            if self.fail_ack {
                bail!("tapback refused");
            }
            self.acknowledged
                .lock()
                .unwrap()
                .push(obligation.ack_id.clone());
            Ok(())
        }
        async fn nudge(&self, obligation: &Obligation) -> Result<()> {
            if self.fail_nudge {
                bail!("nudge refused");
            }
            self.nudged.lock().unwrap().push(obligation.ack_id.clone());
            Ok(())
        }
        async fn admit(&self, obligation: &Obligation, notice: &str) -> Result<()> {
            if self.fail_admit {
                bail!("admission refused");
            }
            self.admitted
                .lock()
                .unwrap()
                .push(obligation.ack_id.clone());
            self.notices.lock().unwrap().push(notice.to_string());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailingLedger;

    impl ObligationLedger for FailingLedger {
        fn record(&mut self, _obligation: &Obligation) -> Result<()> {
            bail!("ledger is read-only")
        }
        fn obligation(&self, _ack_id: &str) -> Result<Option<Obligation>> {
            Ok(None)
        }
        fn open_in_thread(&self, _agent: &str, _thread: &str) -> Result<Vec<Obligation>> {
            Ok(Vec::new())
        }
        fn due(&self, _now: i64) -> Result<Vec<Obligation>> {
            Ok(Vec::new())
        }
        fn mark_nudged(&mut self, _ack_id: &str, _at: i64) -> Result<()> {
            Ok(())
        }
        fn mark_fulfilled(
            &mut self,
            _ack_id: &str,
            _reply: &str,
            _run_id: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        fn mark_expired(&mut self, _ack_id: &str) -> Result<bool> {
            Ok(false)
        }
        fn mark_noticed(&mut self, _ack_id: &str, _at: i64) -> Result<()> {
            Ok(())
        }
        fn mark_notice_attempt(&mut self, _ack_id: &str) -> Result<()> {
            Ok(())
        }
        fn remove(&mut self, _ack_id: &str) -> Result<()> {
            Ok(())
        }
    }

    fn request(busy: bool) -> AckRequest {
        AckRequest {
            agent: "agent-a".into(),
            thread: "!room:beeper.local".into(),
            message_id: "$steer-1".into(),
            seq: 12,
            busy,
            can_reply: true,
            run_id: Some("run-1".into()),
        }
    }

    fn keeper() -> Keeper<MemoryLedger, FakeEffects> {
        Keeper::new(
            MemoryLedger::new(),
            FakeEffects::default(),
            AckPolicy::MidThreadOnly,
            600,
            300,
        )
    }

    #[test]
    fn the_default_policy_is_mid_thread_only() {
        assert_eq!(AckPolicy::default(), AckPolicy::MidThreadOnly);
        assert_eq!(AckPolicy::default().as_str(), "mid_thread_only");
    }

    #[test]
    fn an_unknown_policy_name_is_an_error_not_a_guess() {
        assert!(AckPolicy::parse("sometimes").is_err());
        assert_eq!(AckPolicy::parse("never").unwrap(), AckPolicy::Never);
        assert_eq!(AckPolicy::parse("always").unwrap(), AckPolicy::Always);
        assert_eq!(
            AckPolicy::parse("mid_thread_only").unwrap(),
            AckPolicy::MidThreadOnly
        );
    }

    #[test]
    fn the_ack_identity_is_stable_and_specific() {
        let first = ack_id("agent-a", "!room:beeper.local", "$m-1");
        assert_eq!(first, ack_id("agent-a", "!room:beeper.local", "$m-1"));
        assert_ne!(first, ack_id("agent-a", "!room:beeper.local", "$m-2"));
        assert_ne!(first, ack_id("agent-b", "!room:beeper.local", "$m-1"));
    }

    #[tokio::test]
    async fn an_idle_agent_is_not_acknowledged_the_reply_is_the_response() {
        let mut keeper = keeper();
        let outcome = keeper.consider(&request(false), 1_000).await;
        assert_eq!(outcome.decision, AckDecision::Blocked(AckBlock::NotBusy));
        assert!(outcome.reason.contains("idle"));
        assert!(outcome.ack_id.is_none());
    }

    #[tokio::test]
    async fn a_busy_agent_acknowledges_once_and_owes_one_obligation() {
        let mut keeper = keeper();
        let outcome = keeper.consider(&request(true), 1_000).await;
        assert_eq!(outcome.decision, AckDecision::Allowed);
        let ack_id = outcome
            .ack_id
            .clone()
            .expect("an acknowledged message has an id");
        assert_eq!(keeper.effects.acknowledged().len(), 1);
        let open = keeper.ledger.open();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].ack_id, ack_id);
        assert_eq!(open[0].message_id, "$steer-1");
        assert_eq!(open[0].thread, "!room:beeper.local");
        assert_eq!(open[0].agent, "agent-a");
        assert_eq!(open[0].due_by, 1_600);
        assert_eq!(open[0].status, ObligationStatus::Open);

        // The same message again is a replay, not a second promise.
        let replay = keeper.consider(&request(true), 1_001).await;
        assert_eq!(
            replay.decision,
            AckDecision::Blocked(AckBlock::AlreadyAcked)
        );
        assert_eq!(keeper.effects.acknowledged().len(), 1);
        assert_eq!(keeper.ledger.open().len(), 1);
    }

    #[tokio::test]
    async fn a_reply_in_the_thread_keeps_the_promise() {
        let mut keeper = keeper();
        keeper.consider(&request(true), 1_000).await;
        let fulfilled = keeper
            .fulfil(
                "agent-a",
                "!room:beeper.local",
                "message-13",
                Some("run-2"),
                13,
            )
            .await
            .unwrap();
        assert_eq!(fulfilled, 1);
        let rows = keeper.ledger.all();
        assert_eq!(rows[0].status, ObligationStatus::Fulfilled);
        assert_eq!(rows[0].fulfilled_by.as_deref(), Some("message-13"));
        assert_eq!(rows[0].run_id.as_deref(), Some("run-2"));
        assert!(keeper.ledger.open().is_empty());
    }

    #[tokio::test]
    async fn a_reply_that_precedes_the_acknowledgement_keeps_nothing() {
        let mut keeper = keeper();
        keeper.consider(&request(true), 1_000).await;
        let fulfilled = keeper
            .fulfil("agent-a", "!room:beeper.local", "message-11", None, 11)
            .await
            .unwrap();
        assert_eq!(fulfilled, 0);
        assert_eq!(keeper.ledger.open().len(), 1);
    }

    #[tokio::test]
    async fn a_broken_promise_is_nudged_once_then_admitted_once() {
        let mut keeper = keeper();
        keeper.consider(&request(true), 1_000).await;
        let ack_id = keeper.ledger.open()[0].ack_id.clone();

        // Due, but inside the grace period: one nudge, no admission.
        let first = keeper.sweep(1_700).await.unwrap();
        assert_eq!(first.nudged, 1);
        assert_eq!(first.admitted, 0);
        assert_eq!(keeper.effects.nudged(), vec![ack_id.clone()]);

        // Still inside the grace period: no second nudge.
        let second = keeper.sweep(1_800).await.unwrap();
        assert_eq!(second.nudged, 0);
        assert_eq!(second.admitted, 0);
        assert_eq!(keeper.effects.nudged(), vec![ack_id.clone()]);

        // Past the grace period: one visible admission.
        let third = keeper.sweep(1_950).await.unwrap();
        assert_eq!(third.admitted, 1);
        assert_eq!(third.expired, 1);
        assert_eq!(keeper.effects.admitted(), vec![ack_id.clone()]);

        // A later sweep admits nothing: the notice is raised once.
        let fourth = keeper.sweep(2_500).await.unwrap();
        assert_eq!(fourth.admitted, 0);
        assert_eq!(keeper.effects.admitted().len(), 1);
        assert_eq!(keeper.effects.notices().len(), 1);
        assert_eq!(keeper.ledger.open().len(), 0);
    }

    #[tokio::test]
    async fn the_admission_notice_is_byte_identical_across_sweeps() {
        let mut keeper = keeper();
        keeper.consider(&request(true), 1_000).await;
        let obligation = keeper.ledger.open()[0].clone();
        let first = admission_notice(&obligation);
        let second = admission_notice(&obligation);
        assert_eq!(first.as_bytes(), second.as_bytes());
        assert!(first.contains(&obligation.ack_id));

        keeper.sweep(1_700).await.unwrap();
        keeper.sweep(1_950).await.unwrap();
        let notices = keeper.effects.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].as_bytes(), first.as_bytes());
        assert_eq!(keeper.ledger.all().len(), 1);
    }

    #[tokio::test]
    async fn a_ledger_that_cannot_record_suppresses_the_acknowledgement() {
        let mut keeper = Keeper::new(
            FailingLedger,
            FakeEffects::default(),
            AckPolicy::Always,
            600,
            300,
        );
        let outcome = keeper.consider(&request(true), 1_000).await;
        match &outcome.decision {
            AckDecision::Blocked(AckBlock::LedgerUnavailable(reason)) => {
                assert!(reason.contains("read-only"), "{reason}");
            }
            other => panic!("unexpected decision {other:?}"),
        }
        assert!(outcome.reason.contains("ledger write failed"));
        assert!(outcome.ack_id.is_none());
    }

    #[tokio::test]
    async fn a_failed_tapback_leaves_no_promise_behind() {
        let effects = FakeEffects {
            fail_ack: true,
            ..FakeEffects::default()
        };
        let mut keeper = Keeper::new(MemoryLedger::new(), effects, AckPolicy::Always, 600, 300);
        let outcome = keeper.consider(&request(true), 1_000).await;
        assert!(matches!(
            outcome.decision,
            AckDecision::Blocked(AckBlock::AckFailed(_))
        ));
        assert!(keeper.ledger.all().is_empty());
    }

    #[tokio::test]
    async fn a_channel_the_agent_cannot_answer_is_never_acknowledged() {
        let mut keeper = keeper();
        let mut request = request(true);
        request.can_reply = false;
        let outcome = keeper.consider(&request, 1_000).await;
        assert_eq!(
            outcome.decision,
            AckDecision::Blocked(AckBlock::NoReplyChannel)
        );
        assert!(keeper.ledger.all().is_empty());
    }

    #[tokio::test]
    async fn the_never_policy_never_acknowledges() {
        let mut keeper = Keeper::new(
            MemoryLedger::new(),
            FakeEffects::default(),
            AckPolicy::Never,
            600,
            300,
        );
        let outcome = keeper.consider(&request(true), 1_000).await;
        assert_eq!(
            outcome.decision,
            AckDecision::Blocked(AckBlock::Policy(AckPolicy::Never))
        );
        assert!(keeper.ledger.all().is_empty());
    }

    #[tokio::test]
    async fn the_always_policy_acknowledges_an_idle_agent() {
        let mut keeper = Keeper::new(
            MemoryLedger::new(),
            FakeEffects::default(),
            AckPolicy::Always,
            600,
            300,
        );
        let outcome = keeper.consider(&request(false), 1_000).await;
        assert_eq!(outcome.decision, AckDecision::Allowed);
        assert_eq!(keeper.ledger.open().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_admission_is_bounded_and_never_loops() {
        let effects = FakeEffects {
            fail_admit: true,
            ..FakeEffects::default()
        };
        let mut keeper = Keeper::new(MemoryLedger::new(), effects, AckPolicy::Always, 0, 0);
        keeper.consider(&request(true), 1_000).await;
        for _ in 0..5 {
            keeper.sweep(2_000).await.unwrap();
        }
        let rows = keeper.ledger.all();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ObligationStatus::Expired);
        assert_eq!(rows[0].notice_attempts, 3);
        assert!(keeper.effects.notices().is_empty());
    }

    #[test]
    fn the_file_ledger_keeps_an_open_promise_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orch.acks.json");
        let obligation = Obligation {
            ack_id: "ack-0123456789abcdef01234567".into(),
            message_id: "m-1".into(),
            thread: "thread-a".into(),
            agent: "fleet-orch-v1".into(),
            created_at: 1_000,
            due_by: 1_900,
            seq: 12,
            status: ObligationStatus::Open,
            fulfilled_by: None,
            run_id: None,
            nudged_at: None,
            noticed_at: None,
            notice_attempts: 0,
        };
        {
            let mut ledger = FileLedger::load(&path).unwrap();
            ledger.record(&obligation).unwrap();
            ledger.record(&obligation).unwrap();
        }
        // Restart: the same file, a new ledger.
        let mut ledger = FileLedger::load(&path).unwrap();
        assert_eq!(ledger.all().len(), 1, "one row per promise");
        assert_eq!(ledger.open().len(), 1);
        assert_eq!(ledger.due(1_900).unwrap().len(), 1);
        assert_eq!(ledger.due(1_899).unwrap().len(), 0);
        ledger
            .mark_fulfilled(&obligation.ack_id, "m-2", None)
            .unwrap();
        let reopened = FileLedger::load(&path).unwrap();
        assert_eq!(reopened.open().len(), 0);
        assert_eq!(
            reopened.all()[0].status,
            ObligationStatus::Fulfilled,
            "the closing update is durable too"
        );
    }

    #[tokio::test]
    async fn a_promise_whose_notice_landed_is_closed_without_a_second_notice() {
        let mut keeper = keeper();
        keeper.consider(&request(true), 1_000).await;
        let ack_id = keeper.ledger.open()[0].ack_id.clone();
        // The notice was delivered but the closing update did not land.
        keeper.ledger.mark_noticed(&ack_id, 1_950).unwrap();
        let report = keeper.sweep(2_000).await.unwrap();
        assert_eq!(report.admitted, 0);
        assert!(keeper.effects.notices().is_empty());
        assert_eq!(keeper.ledger.all()[0].status, ObligationStatus::Expired);
    }
}
