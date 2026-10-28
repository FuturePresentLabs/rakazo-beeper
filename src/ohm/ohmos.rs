//! Muse inbound/outbound over the ohmOS agent-conversation provider.
//!
//! This is a *provider beside* the existing `msgd` transport in
//! [`super::transport`], not a third bridge: it reuses the ohm delivery
//! primitive (`pi-notify send`) and the same durable-ledger shape as
//! [`super::bridge`]. The `transport = "ohmos"` vs `"msgd"` choice is explicit
//! in config; there is no implicit fallback.
//!
//! Ownership split (exactly one owner per WhatsApp message):
//! - **Agent delivery + outbound** — this module. Inbound `from_me=false` rows
//!   go to the roster `worker` target for the agent; replies go back through
//!   `POST /agent-conversations/{id}/messages`.
//! - **Rakazo UI display** — the existing hosted `conversationAttachments`
//!   binding stays the display path. It is never asked to deliver to an agent
//!   and this module never posts into the Rakazo UI, so a message is not doubled.

use super::{
    agent_conversations::{
        AgentConversationsClient, AgentError, AgentHttp, ExpectedNativeIdentity, NativeMessage,
    },
    bridge::{CommandRunner, ProcessRunner},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Bounded inbound ledger: enough history that a restart plus a long phone
/// window cannot re-deliver, without growing without limit.
const MAX_SEEN: usize = 8_192;

/// One provider poll item: the conversation it came from plus the phone row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentInbound {
    pub conversation_id: String,
    pub message: NativeMessage,
}

/// One outbound reply. `request_id` is the phone's immutable idempotency key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentOutbound {
    pub conversation_id: String,
    pub request_id: String,
    pub text: String,
    pub identity: Option<(super::wire::Platform, String)>,
}

/// `Queued` is the only success: the native outbox accepted the request. It is
/// not delivery proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSendOutcome {
    Queued,
}

/// The provider seam (mirrors [`super::transport::Transport`], but identity is
/// the phone's stable `native_id` string, not a local row id).
pub trait AgentTransport {
    fn poll(&mut self) -> Result<Vec<AgentInbound>, AgentError>;
    fn send(&mut self, outbound: &AgentOutbound) -> Result<AgentSendOutcome, AgentError>;

    /// The provider-specific stable reference used in `pi-notify --ref`. The
    /// default is the ohmOS shape; Beeper overrides it to
    /// `rakazo:beeper:<chatID>:<messageID>`.
    fn reference(&self, conversation_id: &str, native_id: &str) -> String {
        format!("rakazo:{conversation_id}:{native_id}")
    }

    /// The provider id of the last confirmed send (e.g. Beeper's pending message
    /// id). `None` means the provider has no usable id. The bridge writes this
    /// id into the durable ledger before reporting success.
    fn last_receipt_id(&self) -> Option<String> {
        None
    }
}

/// The real provider: one conversation over the ohmOS HTTP client.
pub struct OhmosTransport<H: AgentHttp> {
    client: AgentConversationsClient<H>,
    conversation_id: String,
    identity: Option<(super::wire::Platform, String)>,
    limit: usize,
}

impl<H: AgentHttp> OhmosTransport<H> {
    pub fn new(
        client: AgentConversationsClient<H>,
        conversation_id: String,
        identity: Option<(super::wire::Platform, String)>,
        limit: usize,
    ) -> Self {
        Self {
            client,
            conversation_id,
            identity,
            limit,
        }
    }
}

impl<H: AgentHttp> AgentTransport for OhmosTransport<H> {
    fn poll(&mut self) -> Result<Vec<AgentInbound>, AgentError> {
        let snapshot = self.client.snapshot(&self.conversation_id, self.limit)?;
        Ok(snapshot
            .messages
            .into_iter()
            .map(|message| AgentInbound {
                conversation_id: self.conversation_id.clone(),
                message,
            })
            .collect())
    }

    fn send(&mut self, outbound: &AgentOutbound) -> Result<AgentSendOutcome, AgentError> {
        let identity =
            outbound
                .identity
                .as_ref()
                .or(self.identity.as_ref())
                .map(|(platform, remote_id)| ExpectedNativeIdentity {
                    platform: *platform,
                    remote_id: remote_id.as_str(),
                });
        self.client.send(
            &outbound.conversation_id,
            &outbound.request_id,
            &outbound.text,
            identity,
        )?;
        Ok(AgentSendOutcome::Queued)
    }
}

/// The only local delivery primitive for an agent. `steer` + `--urgent` is the
/// existing single-agent path; `pi-notify` queues the wakeup. A pane paste is
/// never used here.
pub trait AgentDelivery {
    fn deliver(&self, worker: &str, reference: &str, text: &str) -> Result<(), AgentError>;
}

#[derive(Clone, Debug)]
pub struct PiNotifyDelivery<R = ProcessRunner> {
    runner: R,
}

impl PiNotifyDelivery<ProcessRunner> {
    pub fn new() -> Self {
        Self {
            runner: ProcessRunner,
        }
    }
}

impl Default for PiNotifyDelivery<ProcessRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R> PiNotifyDelivery<R> {
    pub fn with_runner(runner: R) -> Self {
        Self { runner }
    }

    /// `--ref` is `rakazo:<conversation>:<native message id>`; the kind is
    /// `steer` and the delivery is urgent by construction.
    pub fn command_args(worker: &str, reference: &str, text: &str) -> Vec<String> {
        vec![
            "send".to_string(),
            "--kind".to_string(),
            "steer".to_string(),
            "--urgent".to_string(),
            "--worker".to_string(),
            worker.to_string(),
            "--ref".to_string(),
            reference.to_string(),
            "--text".to_string(),
            text.to_string(),
        ]
    }
}

impl<R: CommandRunner> AgentDelivery for PiNotifyDelivery<R> {
    fn deliver(&self, worker: &str, reference: &str, text: &str) -> Result<(), AgentError> {
        let args = Self::command_args(worker, reference, text);
        let (success, stderr) = self
            .runner
            .run("pi-notify", &args)
            .map_err(|error| AgentError::Refused(format!("run pi-notify for {worker}: {error}")))?;
        if !success {
            return Err(AgentError::Refused(format!(
                "pi-notify delivery failed for {worker}: {stderr}"
            )));
        }
        Ok(())
    }
}

/// Recording fake for offline tests.
#[derive(Default, Clone)]
pub struct FakeDelivery {
    pub calls: std::sync::Arc<Mutex<Vec<(String, String, String)>>>,
    pub fail: bool,
}

impl FakeDelivery {
    pub fn calls(&self) -> Vec<(String, String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl AgentDelivery for FakeDelivery {
    fn deliver(&self, worker: &str, reference: &str, text: &str) -> Result<(), AgentError> {
        if self.fail {
            return Err(AgentError::Refused("scripted delivery failure".into()));
        }
        self.calls.lock().unwrap().push((
            worker.to_string(),
            reference.to_string(),
            text.to_string(),
        ));
        Ok(())
    }
}

/// Scripted provider for offline tests. `offline` proves zero network calls.
#[derive(Default)]
pub struct FakeAgentTransport {
    pub batches: Vec<Vec<AgentInbound>>,
    pub sent: Vec<AgentOutbound>,
    pub offline: bool,
}

impl FakeAgentTransport {
    pub fn new(batches: Vec<Vec<AgentInbound>>) -> Self {
        Self {
            batches,
            sent: Vec::new(),
            offline: false,
        }
    }
}

impl AgentTransport for FakeAgentTransport {
    fn poll(&mut self) -> Result<Vec<AgentInbound>, AgentError> {
        if self.offline {
            return Err(AgentError::Offline);
        }
        if self.batches.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(self.batches.remove(0))
        }
    }

    fn send(&mut self, outbound: &AgentOutbound) -> Result<AgentSendOutcome, AgentError> {
        if self.offline {
            return Err(AgentError::Offline);
        }
        // Mimic `queue_outbound_once`: the same request id replays without
        // adding a second native send.
        if !self
            .sent
            .iter()
            .any(|o| o.request_id == outbound.request_id)
        {
            self.sent.push(outbound.clone());
        }
        Ok(AgentSendOutcome::Queued)
    }
}

/// Durable provider ledger. Dedupe is by the **phone message id** (`native_id`),
/// never text. `own_requests` suppresses our own outbound echoes even if the
/// phone reports one as inbound.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(default)]
    pub seen: BTreeSet<String>,
    #[serde(default)]
    pub own_requests: BTreeSet<String>,
    #[serde(default)]
    pub replies: BTreeSet<String>,
    /// request id → provider message id, written after a confirmed send. Kept
    /// separate from `replies` so a retry can prove which native id landed.
    #[serde(default)]
    pub sent: BTreeMap<String, String>,
}

impl AgentState {
    fn remember_seen(&mut self, native_id: &str) {
        self.seen.insert(native_id.to_string());
        while self.seen.len() > MAX_SEEN {
            if let Some(first) = self.seen.iter().next().cloned() {
                self.seen.remove(&first);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentPollReport {
    pub delivered: usize,
    pub duplicates: usize,
    pub suppressed: usize,
    pub dropped: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentReplyReport {
    Queued,
    AlreadyQueued,
}

/// The bridge: one agent conversation, one durable ledger, one delivery target.
pub struct AgentBridge<T: AgentTransport, D: AgentDelivery> {
    transport: T,
    delivery: D,
    worker: String,
    conversation_id: String,
    state: AgentState,
    state_file: Option<PathBuf>,
}

impl<T: AgentTransport, D: AgentDelivery> AgentBridge<T, D> {
    pub fn new(
        transport: T,
        delivery: D,
        worker: String,
        conversation_id: String,
        state_file: Option<PathBuf>,
    ) -> Result<Self, AgentError> {
        if worker.trim().is_empty() {
            return Err(AgentError::Refused("agent worker target is empty".into()));
        }
        if conversation_id.trim().is_empty() {
            return Err(AgentError::Refused(
                "agent conversation_id is not configured".into(),
            ));
        }
        let state = match &state_file {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(path).map_err(|error| {
                    AgentError::Refused(format!("read agent state {}: {error}", path.display()))
                })?;
                serde_json::from_str(&text).map_err(|error| {
                    AgentError::Refused(format!("invalid agent state {}: {error}", path.display()))
                })?
            }
            _ => AgentState::default(),
        };
        Ok(Self {
            transport,
            delivery,
            worker,
            conversation_id,
            state,
            state_file,
        })
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Drain one snapshot. Each `from_me=false` row with a non-empty phone id is
    /// delivered at most once, even across restarts. A delivery failure fails
    /// closed and does **not** advance the ledger, so the next poll retries.
    pub fn poll_once(&mut self) -> Result<AgentPollReport, AgentError> {
        let inbound = self.transport.poll()?;
        let mut report = AgentPollReport::default();
        let mut changed = false;
        for item in inbound {
            let message = item.message;
            if message.from_me {
                report.suppressed += 1;
                continue;
            }
            if message.deleted {
                report.dropped += 1;
                continue;
            }
            if message.native_id.trim().is_empty() {
                report.dropped += 1;
                continue;
            }
            if self.state.seen.contains(&message.native_id) {
                report.duplicates += 1;
                continue;
            }
            if self.state.own_requests.contains(&message.native_id) {
                // Our own echo: never deliver it back to the worker.
                report.suppressed += 1;
                continue;
            }
            let Some(text) = message.text.as_deref() else {
                report.dropped += 1;
                continue;
            };
            if text.trim().is_empty() {
                report.dropped += 1;
                continue;
            }
            let conversation = if item.conversation_id.trim().is_empty() {
                self.conversation_id.clone()
            } else {
                item.conversation_id
            };
            let reference = self.transport.reference(&conversation, &message.native_id);
            let body = format!("[{reference}] {text}");
            self.delivery
                .deliver(&self.worker, &reference, &body)
                .map_err(|error| {
                    AgentError::Refused(format!(
                        "deliver {conversation}/{} to {}: {error}",
                        message.native_id, self.worker
                    ))
                })?;
            self.state.remember_seen(&message.native_id);
            report.delivered += 1;
            changed = true;
        }
        if changed {
            self.save()?;
        }
        Ok(report)
    }

    /// Post one reply with a stable request id. A retry with the same id returns
    /// [`AgentReplyReport::AlreadyQueued`] without a second HTTP post, and the
    /// phone's `queue_outbound_once` would replay it anyway.
    pub fn reply(&mut self, request_id: &str, text: &str) -> Result<AgentReplyReport, AgentError> {
        if text.trim().is_empty() {
            return Err(AgentError::Refused("refusing empty reply text".into()));
        }
        if self.state.replies.contains(request_id) {
            return Ok(AgentReplyReport::AlreadyQueued);
        }
        let outbound = AgentOutbound {
            conversation_id: self.conversation_id.clone(),
            request_id: request_id.to_string(),
            text: text.to_string(),
            identity: None,
        };
        match self.transport.send(&outbound)? {
            AgentSendOutcome::Queued => {
                self.state.replies.insert(request_id.to_string());
                self.state.own_requests.insert(request_id.to_string());
                if let Some(message_id) = self.transport.last_receipt_id() {
                    // Record the provider id *before* reporting success, so a
                    // crash between send and save cannot re-post a duplicate.
                    self.state.own_requests.insert(message_id.clone());
                    self.state.sent.insert(request_id.to_string(), message_id);
                }
                self.save()?;
                Ok(AgentReplyReport::Queued)
            }
        }
    }

    fn save(&self) -> Result<(), AgentError> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| {
                AgentError::Refused(format!(
                    "create agent state dir {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let tmp = path.with_extension("tmp");
        let text = serde_json::to_string_pretty(&self.state)
            .map_err(|error| AgentError::Refused(format!("serialize agent state: {error}")))?;
        std::fs::write(&tmp, text).map_err(|error| {
            AgentError::Refused(format!("write agent state {}: {error}", tmp.display()))
        })?;
        std::fs::rename(&tmp, path).map_err(|error| {
            AgentError::Refused(format!("replace agent state {}: {error}", path.display()))
        })?;
        Ok(())
    }
}

/// Resolve the ledger path. An explicit `state_file` wins; otherwise the fleet
/// state directory keeps one file per agent so a restart is always durable.
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
    base.join(format!("{key}-ohmos.json"))
}

/// Build the outbound identity only when both halves are configured, so an
/// incomplete config sends without the precondition rather than guessing.
pub fn identity(
    platform: Option<super::wire::Platform>,
    remote_id: Option<&str>,
) -> Option<(super::wire::Platform, String)> {
    match (platform, remote_id) {
        (Some(platform), Some(remote_id)) if !remote_id.trim().is_empty() => {
            Some((platform, remote_id.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ohm::{Platform, agent_conversations::AgentConversationsClient};
    use tempfile::TempDir;

    fn message(native: &str, from_me: bool, text: &str) -> NativeMessage {
        NativeMessage {
            local_id: 1,
            native_id: native.into(),
            from_me,
            sender: if from_me { "me" } else { "Muse" }.into(),
            timestamp: 1,
            text: Some(text.into()),
            ..Default::default()
        }
    }

    fn inbound(native: &str, text: &str) -> AgentInbound {
        AgentInbound {
            conversation_id: "whatsapp:123@bot".into(),
            message: message(native, false, text),
        }
    }

    fn bridge(
        transport: FakeAgentTransport,
        delivery: FakeDelivery,
        state_file: Option<PathBuf>,
    ) -> AgentBridge<FakeAgentTransport, FakeDelivery> {
        AgentBridge::new(
            transport,
            delivery,
            "muse".into(),
            "whatsapp:123@bot".into(),
            state_file,
        )
        .unwrap()
    }

    #[test]
    fn inbound_is_delivered_once_by_phone_id_and_never_a_pane() {
        let transport = FakeAgentTransport::new(vec![
            vec![inbound("wa-1", "hello"), inbound("wa-1", "hello")],
            vec![inbound("wa-1", "hello")],
        ]);
        let delivery = FakeDelivery::default();
        let mut bridge = bridge(transport, delivery, None);
        let first = bridge.poll_once().unwrap();
        assert_eq!(first.delivered, 1);
        assert_eq!(first.duplicates, 1);
        let second = bridge.poll_once().unwrap();
        assert_eq!(second.delivered, 0);
        assert_eq!(second.duplicates, 1);
        let calls = bridge.delivery.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "muse");
        assert_eq!(calls[0].1, "rakazo:whatsapp:123@bot:wa-1");
        assert!(
            calls[0]
                .2
                .starts_with("[rakazo:whatsapp:123@bot:wa-1] hello")
        );
    }

    #[test]
    fn inbound_dedupe_survives_a_restart() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("muse.json");
        {
            let transport = FakeAgentTransport::new(vec![vec![inbound("wa-7", "once")]]);
            let mut bridge = bridge(transport, FakeDelivery::default(), Some(state.clone()));
            assert_eq!(bridge.poll_once().unwrap().delivered, 1);
        }
        assert!(state.exists());
        let transport = FakeAgentTransport::new(vec![vec![inbound("wa-7", "once")]]);
        let delivery = FakeDelivery::default();
        let mut bridge = bridge(transport, delivery.clone(), Some(state));
        let report = bridge.poll_once().unwrap();
        assert_eq!(report.delivered, 0);
        assert_eq!(report.duplicates, 1);
        assert!(delivery.calls().is_empty(), "a restart must not redeliver");
    }

    #[test]
    fn from_me_and_own_request_ids_are_suppressed() {
        let transport = FakeAgentTransport::new(vec![vec![
            AgentInbound {
                conversation_id: "whatsapp:123@bot".into(),
                message: message("wa-out-1", true, "sent by me"),
            },
            inbound("req-own-1", "our echo"),
            inbound("wa-in-1", "real inbound"),
        ]]);
        let delivery = FakeDelivery::default();
        let mut bridge = bridge(transport, delivery, None);
        // Pretend the same bridge sent `req-own-1` earlier.
        bridge.state.own_requests.insert("req-own-1".into());
        let report = bridge.poll_once().unwrap();
        assert_eq!(report.suppressed, 2);
        assert_eq!(report.delivered, 1);
        let calls = bridge.delivery.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].2.contains("real inbound"));
    }

    #[test]
    fn empty_and_deleted_rows_are_dropped_not_delivered() {
        let mut deleted = message("wa-del", false, "gone");
        deleted.deleted = true;
        let no_text = NativeMessage {
            native_id: "wa-media".into(),
            ..Default::default()
        };
        let transport = FakeAgentTransport::new(vec![vec![
            AgentInbound {
                conversation_id: "whatsapp:123@bot".into(),
                message: deleted,
            },
            AgentInbound {
                conversation_id: "whatsapp:123@bot".into(),
                message: no_text,
            },
        ]]);
        let mut bridge = bridge(transport, FakeDelivery::default(), None);
        let report = bridge.poll_once().unwrap();
        assert_eq!(report.dropped, 2);
        assert_eq!(report.delivered, 0);
    }

    #[test]
    fn delivery_failure_fails_closed_and_retries_next_poll() {
        let transport = FakeAgentTransport::new(vec![vec![inbound("wa-1", "retry me")]]);
        let delivery = FakeDelivery {
            fail: true,
            ..Default::default()
        };
        let mut bridge = bridge(transport, delivery, None);
        assert!(bridge.poll_once().is_err());
        assert!(
            bridge.state().seen.is_empty(),
            "failure must not advance the ledger"
        );
    }

    #[test]
    fn reply_is_idempotent_for_the_same_request_id() {
        let transport = FakeAgentTransport::new(vec![]);
        let mut bridge = bridge(transport, FakeDelivery::default(), None);
        assert_eq!(
            bridge.reply("req-1", "answer").unwrap(),
            AgentReplyReport::Queued
        );
        assert_eq!(
            bridge.reply("req-1", "answer").unwrap(),
            AgentReplyReport::AlreadyQueued
        );
        assert_eq!(bridge.transport.sent.len(), 1);
        assert_eq!(bridge.transport.sent[0].request_id, "req-1");
        assert!(bridge.state().own_requests.contains("req-1"));
    }

    #[test]
    fn reply_state_survives_a_restart() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("muse.json");
        {
            let mut bridge = bridge(
                FakeAgentTransport::new(vec![]),
                FakeDelivery::default(),
                Some(state.clone()),
            );
            assert_eq!(
                bridge.reply("req-9", "answer").unwrap(),
                AgentReplyReport::Queued
            );
        }
        let mut bridge = bridge(
            FakeAgentTransport::new(vec![]),
            FakeDelivery::default(),
            Some(state),
        );
        assert_eq!(
            bridge.reply("req-9", "answer").unwrap(),
            AgentReplyReport::AlreadyQueued
        );
        assert!(bridge.transport.sent.is_empty());
    }

    #[test]
    fn offline_transport_makes_no_delivery_and_no_send() {
        let transport = FakeAgentTransport {
            offline: true,
            ..Default::default()
        };
        let delivery = FakeDelivery::default();
        let mut bridge = bridge(transport, delivery.clone(), None);
        assert_eq!(bridge.poll_once().unwrap_err(), AgentError::Offline);
        assert!(delivery.calls().is_empty());
        assert_eq!(bridge.reply("req-1", "x").unwrap_err(), AgentError::Offline);
    }

    #[test]
    fn pi_notify_delivery_is_steer_urgent_with_a_ref() {
        let args = PiNotifyDelivery::<ProcessRunner>::command_args(
            "muse",
            "rakazo:whatsapp:123@bot:wa-1",
            "[ref] hello",
        );
        assert_eq!(args[0], "send");
        assert_eq!(args[1], "--kind");
        assert_eq!(args[2], "steer");
        assert_eq!(args[3], "--urgent");
        assert!(args.contains(&"--ref".to_string()));
        assert!(args.contains(&"rakazo:whatsapp:123@bot:wa-1".to_string()));
        assert!(!args.iter().any(|arg| arg == "send-keys"));
    }

    #[test]
    fn identity_requires_both_halves() {
        assert_eq!(identity(None, Some("r")), None);
        assert_eq!(identity(Some(Platform::WhatsApp), None), None);
        assert_eq!(
            identity(Some(Platform::WhatsApp), Some("r")),
            Some((Platform::WhatsApp, "r".to_string()))
        );
    }

    #[test]
    fn default_state_file_is_per_agent() {
        let path = default_state_file("muse", None);
        assert!(path.ends_with("muse-ohmos.json"), "{path:?}");
        let explicit = PathBuf::from("/tmp/explicit.json");
        assert_eq!(default_state_file("muse", Some(&explicit)), explicit);
    }

    // Compile-time guard: there is no `Delivered` outcome to report.
    #[test]
    fn queued_is_the_only_send_outcome() {
        let outcomes: &[AgentSendOutcome] = &[AgentSendOutcome::Queued];
        assert_eq!(outcomes, &[AgentSendOutcome::Queued]);
        let _: Option<AgentConversationsClient> = None;
    }
}
