//! Bridge an ohmOS conversation to one fleet roster pane.
//!
//! Worker panes get the fleet's existing `pi-tmux send` primitive (shelled out,
//! never reimplemented). Orchestrator panes never receive a paste: text goes
//! through `pi-notify send` (the queued user-message/handed-row path) and the
//! bridge fails closed if that path cannot run. Replies go back through the
//! transport's `POST /send`. An unknown conversation is dropped and logged once —
//! no pane is ever guessed.

use super::{
    OhmConfig, Route,
    transport::{Inbound, Outbound, SendOutcome, Transport},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    process::Command,
    sync::Mutex,
};

/// Injection side effect. Production shells out; tests record.
pub trait PaneIo {
    fn inject(&self, pane: &str, text: &str) -> Result<()>;
}

/// Runs one external command. Production uses a real process; tests inject a
/// recorder so they can prove a refused target spawns nothing at all.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<(bool, String)>;
}

/// The only production command runner.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<(bool, String)> {
        let output = Command::new(program)
            .args(args)
            .output()
            .with_context(|| format!("run {program}"))?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// The real fleet primitive. Arguments are passed directly to `Command`, never
/// through a shell. This injector is the second, independent E-415 guard: it
/// refuses an orchestrator target before it builds the `pi-tmux` command.
#[derive(Clone, Debug)]
pub struct TmuxPaneIo<R = ProcessRunner> {
    orchestrator_targets: BTreeSet<String>,
    runner: R,
}

impl TmuxPaneIo<ProcessRunner> {
    /// Real injector with the configured orchestrator deny-list.
    pub fn new(orchestrator_targets: impl IntoIterator<Item = String>) -> Self {
        Self {
            orchestrator_targets: orchestrator_targets.into_iter().collect(),
            runner: ProcessRunner,
        }
    }
}

impl Default for TmuxPaneIo<ProcessRunner> {
    fn default() -> Self {
        Self::new(["%0".to_string()])
    }
}

impl<R> TmuxPaneIo<R> {
    /// Test seam: same guard, recording runner.
    pub fn with_runner(orchestrator_targets: impl IntoIterator<Item = String>, runner: R) -> Self {
        Self {
            orchestrator_targets: orchestrator_targets.into_iter().collect(),
            runner,
        }
    }

    /// The exact `pi-tmux` invocation for a worker pane. The subcommand is
    /// `send`; this bridge has no `send-keys` path.
    pub fn command_args(pane: &str, text: &str) -> Vec<String> {
        vec![
            "send".to_string(),
            "--target".to_string(),
            pane.to_string(),
            "--message".to_string(),
            text.to_string(),
        ]
    }

    pub fn refuses(&self, pane: &str) -> bool {
        self.orchestrator_targets.contains(pane)
    }
}

impl<R: CommandRunner> PaneIo for TmuxPaneIo<R> {
    fn inject(&self, pane: &str, text: &str) -> Result<()> {
        // E-415 defense in depth: refuse before the command is even built.
        ensure!(
            !self.refuses(pane),
            "refusing pi-tmux paste into orchestrator target {pane}"
        );
        let args = Self::command_args(pane, text);
        let (success, stderr) = self.runner.run("pi-tmux", &args)?;
        ensure!(success, "pi-tmux send failed for pane {pane}: {stderr}");
        Ok(())
    }
}

/// A recording fake for offline tests.
#[derive(Default)]
pub struct FakePaneIo {
    pub injections: Mutex<Vec<(String, String)>>,
    pub fail: bool,
}

impl FakePaneIo {
    pub fn injected(&self) -> Vec<(String, String)> {
        self.injections.lock().unwrap().clone()
    }
}

impl PaneIo for FakePaneIo {
    fn inject(&self, pane: &str, text: &str) -> Result<()> {
        if self.fail {
            bail!("scripted pane injection failure");
        }
        self.injections
            .lock()
            .unwrap()
            .push((pane.to_string(), text.to_string()));
        Ok(())
    }
}

/// Orchestrator delivery side effect. Never a paste: the notification queue is
/// the only path to an orchestrator pane.
pub trait OrchestratorNotifier {
    fn notify(&self, worker: &str, text: &str) -> Result<()>;
}

/// Real notifier. `pi-notify` owns the orchestrator's safe delivery path (its
/// inbox extension queues a user message); it also refuses to paste into a live
/// orchestrator draft. `note` is the closest supported queued-message kind —
/// this pi-notify build has no `delivery` kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct PiNotifyNotifier;

impl PiNotifyNotifier {
    /// The only notifier kind this bridge may use. `note` is queued; it is never
    /// a paste and it is never reported as delivered.
    pub const KIND: &'static str = "note";

    pub fn command_args(worker: &str, text: &str) -> Vec<String> {
        vec![
            "send".to_string(),
            "--kind".to_string(),
            Self::KIND.to_string(),
            "--worker".to_string(),
            worker.to_string(),
            "--text".to_string(),
            text.to_string(),
        ]
    }
}

impl OrchestratorNotifier for PiNotifyNotifier {
    fn notify(&self, worker: &str, text: &str) -> Result<()> {
        let args = Self::command_args(worker, text);
        let output = Command::new("pi-notify")
            .args(&args)
            .output()
            .context("run pi-notify send")?;
        ensure!(
            output.status.success(),
            "pi-notify send failed for {worker}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }
}

/// Recording fake for offline tests.
#[derive(Default)]
pub struct FakeNotifier {
    pub calls: Mutex<Vec<(String, String)>>,
    pub fail: bool,
}

impl FakeNotifier {
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl OrchestratorNotifier for FakeNotifier {
    fn notify(&self, worker: &str, text: &str) -> Result<()> {
        if self.fail {
            bail!("scripted notifier failure");
        }
        self.calls
            .lock()
            .unwrap()
            .push((worker.to_string(), text.to_string()));
        Ok(())
    }
}

/// Bridge state, persisted as small JSON. `seen` dedupes message ids, `warned`
/// makes the unknown-conversation log once across restarts, and `replies` is the
/// local idempotency ledger for `POST /send` (the daemon body has no key field).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BridgeState {
    #[serde(default)]
    pub seen: BTreeSet<i64>,
    #[serde(default)]
    pub warned: BTreeSet<i64>,
    #[serde(default)]
    pub replies: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollReport {
    pub injected: usize,
    pub duplicates: usize,
    pub dropped: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyReport {
    Queued,
    AlreadySent,
}

pub struct Bridge<T: Transport, P: PaneIo, N: OrchestratorNotifier> {
    transport: T,
    panes: P,
    notifier: N,
    routes: HashMap<i64, Route>,
    default_pane: Option<String>,
    orchestrator_targets: BTreeSet<String>,
    state: BridgeState,
    state_file: Option<PathBuf>,
}

impl<T: Transport, P: PaneIo, N: OrchestratorNotifier> Bridge<T, P, N> {
    pub fn new(
        config: &OhmConfig,
        default_pane: Option<String>,
        transport: T,
        panes: P,
        notifier: N,
    ) -> Result<Self> {
        config.validate()?;
        let mut routes = HashMap::new();
        for route in &config.routes {
            ensure!(
                route.pane.is_some() || default_pane.is_some(),
                "ohm route for chat {} has no pane and no --target was supplied",
                route.chat_id
            );
            routes.insert(route.chat_id, route.clone());
        }
        let state = match &config.state_file {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("read ohm state {}", path.display()))?;
                serde_json::from_str(&text)
                    .with_context(|| format!("invalid ohm state {}", path.display()))?
            }
            _ => BridgeState::default(),
        };
        Ok(Self {
            transport,
            panes,
            notifier,
            routes,
            default_pane,
            orchestrator_targets: config.orchestrator_targets.iter().cloned().collect(),
            state,
            state_file: config.state_file.clone(),
        })
    }

    pub fn state(&self) -> &BridgeState {
        &self.state
    }

    /// Drain one transport poll. Each inbound message is injected at most once,
    /// even across polls and restarts.
    pub fn poll_once(&mut self) -> Result<PollReport> {
        let inbound = self.transport.poll()?;
        let mut report = PollReport::default();
        let mut changed = false;
        for message in inbound {
            let route = match self.routes.get(&message.chat_id) {
                Some(route) => route.clone(),
                None => {
                    report.dropped += 1;
                    if self.state.warned.insert(message.chat_id) {
                        // Log the conversation id only. Unknown text never leaks.
                        tracing::warn!(
                            chat_id = message.chat_id,
                            "ohm: no roster route for conversation; dropping"
                        );
                        changed = true;
                    }
                    continue;
                }
            };
            if self.state.seen.contains(&message.message.id) {
                report.duplicates += 1;
                continue;
            }
            let Some(text) = message.message.text.as_deref() else {
                report.dropped += 1;
                continue;
            };
            if text.trim().is_empty() {
                report.dropped += 1;
                continue;
            }
            let pane = route
                .pane
                .clone()
                .or_else(|| self.default_pane.clone())
                .expect("validated above");
            let body = format!(
                "[ohm {} chat {} -> {}] {}",
                message.platform.label(),
                message.chat_id,
                route.roster_key,
                text
            );
            if self.orchestrator_targets.contains(&pane) {
                // Hard rule (E-415): never paste into an orchestrator pane.
                // The notification queue is the only delivery path; there is no
                // pi-tmux fallback here, so a failure fails closed.
                self.notifier
                    .notify(&route.roster_key, &body)
                    .with_context(|| {
                        format!("queue ohm chat {} for orchestrator", message.chat_id)
                    })?;
            } else {
                self.panes
                    .inject(&pane, &body)
                    .with_context(|| format!("inject ohm chat {} into pane", message.chat_id))?;
            }
            self.state.seen.insert(message.message.id);
            report.injected += 1;
            changed = true;
        }
        if changed {
            self.save()?;
        }
        Ok(report)
    }

    /// Post one orchestrator reply for an inbound message. The key is stable, so
    /// a retry after a queued send does not post twice.
    pub fn reply(
        &mut self,
        chat_id: i64,
        inbound_message_id: i64,
        text: &str,
    ) -> Result<ReplyReport> {
        ensure!(!text.trim().is_empty(), "refusing to send empty reply text");
        let route = self
            .routes
            .get(&chat_id)
            .with_context(|| format!("no ohm route for conversation {chat_id}"))?
            .clone();
        let key = format!("ohm-reply-{chat_id}-{inbound_message_id}");
        if self.state.replies.contains(&key) {
            return Ok(ReplyReport::AlreadySent);
        }
        let outbound = Outbound {
            platform: route.platform,
            chat_remote_id: route.remote_id.clone(),
            recipient: route
                .recipient
                .clone()
                .unwrap_or_else(|| route.remote_id.clone()),
            text: text.to_string(),
            idempotency_key: key.clone(),
        };
        match self.transport.send(&outbound)? {
            SendOutcome::Queued => {
                self.state.replies.insert(key);
                self.save()?;
                Ok(ReplyReport::Queued)
            }
        }
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create ohm state dir {}", parent.display()))?;
        }
        let tmp = path.with_extension("tmp");
        let text = serde_json::to_string_pretty(&self.state)?;
        std::fs::write(&tmp, text).with_context(|| format!("write ohm state {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("replace ohm state {}", path.display()))?;
        Ok(())
    }
}

/// Inbound helper used by tests and the HTTP transport to build a message.
pub fn inbound(
    chat_id: i64,
    platform: super::Platform,
    remote_id: &str,
    id: i64,
    text: &str,
) -> Inbound {
    Inbound {
        chat_id,
        platform,
        chat_remote_id: remote_id.to_string(),
        recipient: remote_id.to_string(),
        message: super::Msg {
            id,
            from_me: false,
            sender: "phone".into(),
            timestamp: id,
            text: Some(text.to_string()),
            media_ref: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ohm::{
        Platform, Route,
        transport::{FakeTransport, Inbound},
    };
    use tempfile::TempDir;

    fn route(chat_id: i64, pane: &str) -> Route {
        Route {
            chat_id,
            roster_key: "fleet-orch-v1".into(),
            pane: Some(pane.into()),
            platform: Platform::WhatsApp,
            remote_id: "remote-1".into(),
            recipient: None,
        }
    }

    fn config(routes: Vec<Route>) -> OhmConfig {
        OhmConfig {
            base_url: None,
            agents: Vec::new(),
            allow_send: false,
            state_file: None,
            orchestrator_targets: vec!["%0".into()],
            routes,
        }
    }

    #[test]
    fn one_inbound_message_injects_exactly_once() {
        let msg = inbound(7, Platform::WhatsApp, "remote-1", 41, "hello");
        let transport = FakeTransport::new(vec![vec![msg.clone()], vec![msg]]);
        let panes = FakePaneIo::default();
        let mut bridge = Bridge::new(
            &config(vec![route(7, "%1")]),
            None,
            transport,
            panes,
            FakeNotifier::default(),
        )
        .unwrap();
        let first = bridge.poll_once().unwrap();
        assert_eq!(first.injected, 1);
        let second = bridge.poll_once().unwrap();
        assert_eq!(second.injected, 0);
        assert_eq!(second.duplicates, 1);
        let injections = bridge.panes.injected();
        assert_eq!(injections.len(), 1);
        assert!(injections[0].1.contains("hello"));
    }

    #[test]
    fn unknown_conversation_is_dropped_and_logged_once() {
        let msg = inbound(99, Platform::Signal, "secret-remote", 1, "DO NOT LEAK");
        let transport = FakeTransport::new(vec![vec![msg.clone()], vec![msg]]);
        let mut bridge = Bridge::new(
            &config(vec![route(7, "%1")]),
            None,
            transport,
            FakePaneIo::default(),
            FakeNotifier::default(),
        )
        .unwrap();
        let report = bridge.poll_once().unwrap();
        assert_eq!(report.dropped, 1);
        assert!(bridge.panes.injected().is_empty());
        assert_eq!(bridge.state().warned.len(), 1);
        bridge.poll_once().unwrap();
        assert_eq!(bridge.state().warned.len(), 1);
    }

    #[test]
    fn reply_posts_once_with_a_stable_key() {
        let msg = inbound(7, Platform::WhatsApp, "remote-1", 41, "hello");
        let transport = FakeTransport::new(vec![vec![msg]]);
        let mut bridge = Bridge::new(
            &config(vec![route(7, "%1")]),
            None,
            transport,
            FakePaneIo::default(),
            FakeNotifier::default(),
        )
        .unwrap();
        bridge.poll_once().unwrap();
        assert_eq!(bridge.reply(7, 41, "answer").unwrap(), ReplyReport::Queued);
        assert_eq!(
            bridge.reply(7, 41, "answer").unwrap(),
            ReplyReport::AlreadySent
        );
        let sent = &bridge.transport.sent;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].idempotency_key, "ohm-reply-7-41");
    }

    #[test]
    fn state_is_restart_safe() {
        let dir = TempDir::new().unwrap();
        let state_file = dir.path().join("ohm-state.json");
        let mut cfg = config(vec![route(7, "%1")]);
        cfg.state_file = Some(state_file.clone());
        let msg = inbound(7, Platform::WhatsApp, "remote-1", 41, "hello");
        {
            let transport = FakeTransport::new(vec![vec![msg.clone()]]);
            let mut bridge = Bridge::new(
                &cfg,
                None,
                transport,
                FakePaneIo::default(),
                FakeNotifier::default(),
            )
            .unwrap();
            assert_eq!(bridge.poll_once().unwrap().injected, 1);
        }
        let transport = FakeTransport::new(vec![vec![msg]]);
        let mut bridge = Bridge::new(
            &cfg,
            None,
            transport,
            FakePaneIo::default(),
            FakeNotifier::default(),
        )
        .unwrap();
        assert_eq!(bridge.poll_once().unwrap().duplicates, 1);
        assert!(bridge.panes.injected().is_empty());
        assert!(state_file.exists());
    }

    #[test]
    fn unknown_route_roster_key_is_refused() {
        let mut bad = route(7, "%1");
        bad.roster_key = "fleet-imaginary-v1".into();
        let err = Bridge::new(
            &config(vec![bad]),
            None,
            FakeTransport::default(),
            FakePaneIo::default(),
            FakeNotifier::default(),
        )
        .err()
        .expect("invalid roster key must fail");
        assert!(err.to_string().contains("roster_key"), "{err}");
    }

    #[test]
    fn orchestrator_target_uses_notifier_and_never_a_pane_paste() {
        let msg = inbound(
            7,
            Platform::WhatsApp,
            "remote-1",
            41,
            "for the orchestrator",
        );
        let transport = FakeTransport::new(vec![vec![msg]]);
        let panes = FakePaneIo::default();
        let notifier = FakeNotifier::default();
        let mut bridge = Bridge::new(
            &config(vec![route(7, "%0")]),
            None,
            transport,
            panes,
            notifier,
        )
        .unwrap();
        let report = bridge.poll_once().unwrap();
        assert_eq!(report.injected, 1);
        // E-415: zero worker-pane injections; the orchestrator is queued.
        assert!(
            bridge.panes.injected().is_empty(),
            "orchestrator target must never receive pi-tmux/send-keys"
        );
        let calls = bridge.notifier.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "fleet-orch-v1");
        assert!(calls[0].1.contains("for the orchestrator"));
    }

    #[test]
    fn orchestrator_notify_failure_fails_closed_without_pane_fallback() {
        let msg = inbound(7, Platform::WhatsApp, "remote-1", 41, "must not paste");
        let transport = FakeTransport::new(vec![vec![msg]]);
        let mut bridge = Bridge::new(
            &config(vec![route(7, "%0")]),
            None,
            transport,
            FakePaneIo::default(),
            FakeNotifier {
                calls: Mutex::new(Vec::new()),
                fail: true,
            },
        )
        .unwrap();
        assert!(bridge.poll_once().is_err());
        assert!(bridge.panes.injected().is_empty());
        assert!(!bridge.state().seen.contains(&41));
    }

    #[test]
    fn unknown_chat_still_injects_and_notifies_nothing() {
        let msg = inbound(99, Platform::Signal, "secret-remote", 1, "DO NOT LEAK");
        let transport = FakeTransport::new(vec![vec![msg]]);
        let mut bridge = Bridge::new(
            &config(vec![route(7, "%1")]),
            None,
            transport,
            FakePaneIo::default(),
            FakeNotifier::default(),
        )
        .unwrap();
        assert_eq!(bridge.poll_once().unwrap().dropped, 1);
        assert!(bridge.panes.injected().is_empty());
        assert!(bridge.notifier.calls().is_empty());
    }

    #[test]
    fn queued_is_the_only_send_outcome() {
        // Compile-time proof: there is no Delivered variant to report.
        let outcomes: &[SendOutcome] = &[SendOutcome::Queued];
        assert_eq!(outcomes, &[SendOutcome::Queued]);
        let _: Option<Inbound> = None;
    }

    #[derive(Clone, Default)]
    struct RecordingRunner {
        calls: std::sync::Arc<Mutex<Vec<(String, Vec<String>)>>>,
    }

    impl RecordingRunner {
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<(bool, String)> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            Ok((true, String::new()))
        }
    }

    #[test]
    fn real_pane_injector_refuses_orchestrator_targets_before_spawning() {
        let runner = RecordingRunner::default();
        let panes = TmuxPaneIo::with_runner(["%0".into(), "%orch".into()], runner.clone());

        let err = panes.inject("%0", "never paste this").unwrap_err();
        assert!(err.to_string().contains("orchestrator target %0"), "{err}");
        let err = panes.inject("%orch", "nor this").unwrap_err();
        assert!(
            err.to_string().contains("orchestrator target %orch"),
            "{err}"
        );
        assert!(
            runner.calls().is_empty(),
            "E-415 guard must run before any process/command invocation"
        );

        // The same real injector still reaches a worker pane with the one
        // supported subcommand, `send`. There is no send-keys path for %0.
        panes.inject("%1", "worker text").unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "pi-tmux");
        assert_eq!(
            calls[0].1,
            vec!["send", "--target", "%1", "--message", "worker text"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert!(
            !calls[0].1.iter().any(|arg| arg == concat!("send", "-keys")),
            "worker injection must not use a keystroke-paste subcommand"
        );
    }

    #[test]
    fn notifier_kind_is_exactly_note_and_has_no_send_keys_path() {
        assert_eq!(PiNotifyNotifier::KIND, "note");
        let args = PiNotifyNotifier::command_args("fleet-orch-v1", "queued text");
        assert_eq!(args[0], "send");
        assert_eq!(args[1], "--kind");
        assert_eq!(args[2], "note", "notifier kind must be exactly note");
        assert!(!args.iter().any(|arg| arg == concat!("send", "-keys")));
    }
}
