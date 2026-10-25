//! Inbound relay: one Rakazo conversation → the orchestrator's notification queue.
//!
//! The Rakazo client RPC can read a bot thread (`threads.messages`) but cannot
//! append an assistant message, so this increment is inbound only: new
//! **user-role** messages are handed once to a [`Notifier`] seam. There is no
//! reply path here and no attempt to fake one.
//!
//! Delivery is `pi-notify send --kind note` (a queued notification, never a
//! `pi-tmux` paste and never `--kind steer`). Dedupe is a small JSON state file
//! holding the bot binding, a `seq` watermark and the recently delivered message
//! ids, so a restart never notifies twice and a notify failure never advances the
//! watermark.

use crate::orch::Allowlist;
use crate::rakazo::{Bot, Message, Rakazo};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Current UTC time as RFC 3339, without adding a wall-clock dependency.
pub fn now_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    chrono::DateTime::from_timestamp(now.as_secs() as i64, now.subsec_nanos())
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| format!("{}.{}Z", now.as_secs(), now.subsec_nanos()))
}

/// Read side of a Rakazo conversation. Production is [`Rakazo`]; tests inject a
/// scripted fake so no live call is ever made.
pub trait RakazoSource {
    fn bots(&self) -> impl Future<Output = Result<Vec<Bot>>> + Send;
    fn fetch_after(&self, bot: &str, seq: i64)
    -> impl Future<Output = Result<Vec<Message>>> + Send;
}

impl RakazoSource for Rakazo {
    fn bots(&self) -> impl Future<Output = Result<Vec<Bot>>> + Send {
        Rakazo::bots(self)
    }

    fn fetch_after(
        &self,
        bot: &str,
        seq: i64,
    ) -> impl Future<Output = Result<Vec<Message>>> + Send {
        Rakazo::after(self, bot, seq)
    }
}

/// Delivery side effect. The only production impl is [`PiNotify`].
pub trait Notifier {
    fn notify(&self, worker: &str, reference: &str, text: &str) -> Result<()>;
}

/// Real notifier: `pi-notify send --kind note` with a stable `--ref`.
///
/// `note` is the queued-notification kind used by the rest of this bridge; the
/// notifier never pastes and never uses `--kind steer`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PiNotify;

impl PiNotify {
    pub const KIND: &'static str = "note";

    /// The full argument vector. `--ref` is `rakazo:<conversation>:<message>`.
    pub fn command_args(worker: &str, reference: &str, text: &str) -> Vec<String> {
        vec![
            "send".to_string(),
            "--kind".to_string(),
            Self::KIND.to_string(),
            "--worker".to_string(),
            worker.to_string(),
            "--ref".to_string(),
            reference.to_string(),
            "--text".to_string(),
            text.to_string(),
        ]
    }
}

impl Notifier for PiNotify {
    fn notify(&self, worker: &str, reference: &str, text: &str) -> Result<()> {
        let args = Self::command_args(worker, reference, text);
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
    pub calls: Mutex<Vec<(String, String, String)>>,
    pub fail: bool,
}

impl FakeNotifier {
    pub fn calls(&self) -> Vec<(String, String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Notifier for FakeNotifier {
    fn notify(&self, worker: &str, reference: &str, text: &str) -> Result<()> {
        if self.fail {
            anyhow::bail!("scripted notifier failure");
        }
        self.calls.lock().unwrap().push((
            worker.to_string(),
            reference.to_string(),
            text.to_string(),
        ));
        Ok(())
    }
}

/// How many delivered ids are kept so a long-lived watcher stays bounded.
pub const MAX_DELIVERED_IDS: usize = 512;
/// The notification body keeps one line and at most this many text characters.
pub const SNIPPET_CHARS: usize = 240;

/// Persisted binding + dedupe watermark.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WatchState {
    /// The agent this state file is bound to. Empty until the first successful bind.
    #[serde(default)]
    pub bot_id: String,
    /// Rakazo thread id when the bot advertises one, else the bot id.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Highest sequence already accepted (notified or deliberately ignored).
    #[serde(default)]
    pub last_seq: i64,
    /// Message ids already handed to the notifier, oldest first (bounded).
    #[serde(default)]
    pub delivered_ids: VecDeque<String>,
    /// `clientNonce`s this bridge sent itself. Empty for the inbound-only
    /// increment, but it is what makes a future outbound echo recognisable.
    #[serde(default)]
    pub own_nonces: VecDeque<String>,
    /// Total user messages handed to the notifier since this state was created.
    #[serde(default)]
    pub delivered_total: u64,
    /// Total already-seen rows skipped since this state was created.
    #[serde(default)]
    pub duplicates_total: u64,
    /// Message id of the most recent successful delivery.
    #[serde(default)]
    pub last_delivered_id: Option<String>,
    /// `None` before the first poll; explicit so an unstarted watcher is not healthy.
    #[serde(default)]
    pub last_poll_ok: Option<bool>,
    #[serde(default)]
    pub last_poll_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

impl WatchState {
    /// Load a state file when it exists; a missing file is a fresh state.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        match path {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("read watch state {}", path.display()))?;
                serde_json::from_str(&text)
                    .with_context(|| format!("invalid watch state {}", path.display()))
            }
            _ => Ok(Self::default()),
        }
    }

    fn delivered(&self, id: &str) -> bool {
        self.delivered_ids.iter().any(|seen| seen == id)
    }

    fn record_delivered(&mut self, id: &str) {
        if !self.delivered(id) {
            self.delivered_ids.push_back(id.to_string());
        }
        while self.delivered_ids.len() > MAX_DELIVERED_IDS {
            self.delivered_ids.pop_front();
        }
    }

    fn is_echo(&self, message: &Message) -> bool {
        message
            .client_nonce
            .as_deref()
            .is_some_and(|nonce| self.own_nonces.iter().any(|own| own == nonce))
    }
}

/// One poll's outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub delivered: usize,
    pub duplicates: usize,
    pub ignored: usize,
}

/// Watches exactly one bound agent's conversation.
pub struct Watch<S: RakazoSource, N: Notifier> {
    source: S,
    notifier: N,
    worker: String,
    state_file: Option<PathBuf>,
    allowlist: Allowlist,
    state: WatchState,
    bound: Option<Bot>,
}

impl<S: RakazoSource, N: Notifier> Watch<S, N> {
    /// Load the state file when it exists. Nothing is bound yet; call [`bind`].
    ///
    /// [`bind`]: Watch::bind
    pub fn new(
        source: S,
        notifier: N,
        worker: &str,
        state_file: Option<PathBuf>,
        allowlist: Allowlist,
    ) -> Result<Self> {
        ensure!(
            !worker.trim().is_empty(),
            "orchestrator worker name is empty"
        );
        let state = WatchState::load(state_file.as_deref())?;
        Ok(Self {
            source,
            notifier,
            worker: worker.to_string(),
            state_file,
            allowlist,
            state,
            bound: None,
        })
    }

    pub fn state(&self) -> &WatchState {
        &self.state
    }

    pub fn bound(&self) -> Option<&Bot> {
        self.bound.as_ref()
    }

    pub fn notifier(&self) -> &N {
        &self.notifier
    }

    /// Resolve the one allowlisted agent against the agents this session can
    /// actually see, then persist the binding.
    ///
    /// A foreign agent (not listed for this session), a different space/thread,
    /// and a state file bound to a different agent are all refused rather than
    /// silently rebound.
    pub async fn bind(&mut self) -> Result<Bot> {
        let bots = self.source.bots().await?;
        let matched: Vec<&Bot> = bots
            .iter()
            .filter(|bot| bot.id == self.allowlist.bot_id)
            .collect();
        let bot = match matched.as_slice() {
            [bot] => (*bot).clone(),
            [] => anyhow::bail!(
                "allowlisted bot {} is not visible to this session; refusing a foreign conversation",
                self.allowlist.bot_id
            ),
            many => anyhow::bail!(
                "allowlisted bot {} matched {} agents; refusing ambiguous binding",
                self.allowlist.bot_id,
                many.len()
            ),
        };
        self.allowlist.check(&bot)?;
        if !self.state.bot_id.is_empty() {
            ensure!(
                self.state.bot_id == bot.id,
                "state file is bound to agent {}; refusing to rebind to {}",
                self.state.bot_id,
                bot.id
            );
        }
        self.state.bot_id = bot.id.clone();
        self.state.thread_id = Some(self.allowlist.thread_id.clone());
        self.bound = Some(bot.clone());
        self.save()?;
        Ok(bot)
    }

    /// The persisted conversation id used in the `--ref`.
    fn conversation(&self) -> Result<&str> {
        self.state
            .thread_id
            .as_deref()
            .filter(|thread| !thread.is_empty())
            .context("watch is not bound to an agent yet")
    }

    /// Drain new messages once. Each user message is notified at most once, even
    /// across polls and restarts.
    ///
    /// A notifier failure returns an error *without* advancing the watermark past
    /// the failed message, so the next poll retries it. The poll timestamp and
    /// outcome are persisted even on failure so `status` cannot show a stale
    /// healthy timestamp after an outage.
    pub async fn poll_once(&mut self) -> Result<Report> {
        self.state.last_poll_at = Some(now_rfc3339());
        let outcome = self.poll_inner().await;
        match &outcome {
            Ok(_) => {
                self.state.last_poll_ok = Some(true);
                self.state.last_error = None;
            }
            Err(error) => {
                self.state.last_poll_ok = Some(false);
                self.state.last_error = Some(format!("{error:#}"));
            }
        }
        if let Err(save_error) = self.save() {
            if outcome.is_ok() {
                return Err(save_error);
            }
        }
        outcome
    }

    async fn poll_inner(&mut self) -> Result<Report> {
        let bot = self
            .bound
            .clone()
            .context("watch is not bound to an agent yet")?;
        let mut messages = self
            .source
            .fetch_after(&bot.id, self.state.last_seq)
            .await?;
        messages.sort_by_key(|message| message.seq);
        let mut report = Report::default();
        let mut changed = false;

        for message in messages {
            if message.seq <= self.state.last_seq {
                report.duplicates += 1;
                self.state.duplicates_total += 1;
                continue;
            }
            if self.state.delivered(&message.id) {
                // A crash between notify and save must not re-notify.
                report.duplicates += 1;
                self.state.duplicates_total += 1;
                self.state.last_seq = message.seq;
                changed = true;
                continue;
            }
            // Only human text is relayed: bot/assistant rows and our own echoes
            // are acknowledged (watermark advances) but never notified.
            let ignored = message.role != "user"
                || self.state.is_echo(&message)
                || message.text().trim().is_empty();
            if ignored {
                report.ignored += 1;
                self.state.last_seq = message.seq;
                changed = true;
                continue;
            }

            let conversation = self.conversation()?.to_string();
            let reference = format!("rakazo:{conversation}:{}", message.id);
            // The same tag appears in `--ref` and in the body so a notification
            // can be traced back to the exact Rakazo message after a restart.
            let text = format!("[{reference}] {}", summarize(&bot.name, &message));
            // Persist any earlier progress before the side effect, then only
            // advance past this message once the notifier accepted it.
            if changed {
                self.save()?;
                changed = false;
            }
            self.notifier
                .notify(&self.worker, &reference, &text)
                .with_context(|| format!("notify {reference}"))?;
            self.state.record_delivered(&message.id);
            self.state.last_delivered_id = Some(message.id.clone());
            self.state.delivered_total += 1;
            self.state.last_seq = message.seq;
            self.save()?;
            report.delivered += 1;
        }

        if changed {
            self.save()?;
        }
        Ok(report)
    }

    /// Atomic replace, so a crash never leaves a half-written state file.
    fn save(&self) -> Result<()> {
        let Some(path) = &self.state_file else {
            return Ok(());
        };
        write_state(path, &self.state)
    }
}

/// Persist a state file atomically.
pub fn write_state(path: &Path, state: &WatchState) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create watch state dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let text = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp, text).with_context(|| format!("write watch state {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replace watch state {}", path.display()))?;
    Ok(())
}

/// One compact notification line: prefix, conversation, sequence, sender role,
/// then the first [`SNIPPET_CHARS`] characters of the flattened text.
pub fn summarize(bot_name: &str, message: &Message) -> String {
    let flat = message
        .text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut snippet: String = flat.chars().take(SNIPPET_CHARS).collect();
    if flat.chars().count() > SNIPPET_CHARS {
        snippet.push('…');
    }
    format!(
        "[rakazo] {bot_name} #{} {}: {snippet}",
        message.seq, message.role
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn bot(id: &str, name: &str) -> Bot {
        Bot {
            id: id.into(),
            name: name.into(),
            color: String::new(),
            title: String::new(),
            description: String::new(),
            space_id: "space".into(),
            thread_id: Some(format!("thread-{id}")),
            parent_bot_id: None,
            spawn_key: Some("fleet-orch-v1".into()),
            archived_at: None,
            capabilities: None,
        }
    }

    fn allowlist(id: &str) -> Allowlist {
        Allowlist {
            bot_id: id.into(),
            space_id: "space".into(),
            thread_id: format!("thread-{id}"),
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

    /// Scripted source: one page per `fetch_after` call, then empty pages.
    struct FakeSource {
        bots: Vec<Bot>,
        pages: Mutex<VecDeque<Vec<Message>>>,
    }

    impl FakeSource {
        fn new(bots: Vec<Bot>, pages: Vec<Vec<Message>>) -> Self {
            Self {
                bots,
                pages: Mutex::new(pages.into()),
            }
        }
    }

    impl RakazoSource for FakeSource {
        fn bots(&self) -> impl Future<Output = Result<Vec<Bot>>> + Send {
            let bots = self.bots.clone();
            async move { Ok(bots) }
        }

        fn fetch_after(
            &self,
            _bot: &str,
            _seq: i64,
        ) -> impl Future<Output = Result<Vec<Message>>> + Send {
            // Return the scripted page verbatim (the real `after` filters, but
            // replaying rows is what proves the id/seq dedupe holds).
            let page = self.pages.lock().unwrap().pop_front().unwrap_or_default();
            async move { Ok(page) }
        }
    }

    fn watch(
        source: FakeSource,
        notifier: FakeNotifier,
        state_file: Option<PathBuf>,
        allowlist: Allowlist,
    ) -> Watch<FakeSource, FakeNotifier> {
        Watch::new(source, notifier, "fleet-orch-v1", state_file, allowlist).unwrap()
    }

    #[tokio::test]
    async fn one_user_message_notifies_once_with_the_exact_ref() {
        let source = FakeSource::new(
            vec![bot("b1", "Fleet Orchestrator")],
            vec![
                vec![
                    message("m1", 7, "user", "hello orchestrator"),
                    message("m2", 8, "bot", "assistant reply"),
                ],
                vec![message("m1", 7, "user", "hello orchestrator")],
            ],
        );
        let mut watcher = watch(source, FakeNotifier::default(), None, allowlist("b1"));
        watcher.bind().await.unwrap();

        let first = watcher.poll_once().await.unwrap();
        assert_eq!(first.delivered, 1);
        assert_eq!(first.ignored, 1, "assistant rows are skipped");
        assert_eq!(watcher.state().last_poll_ok, Some(true));
        assert!(watcher.state().last_poll_at.is_some());
        assert_eq!(
            watcher.state().last_delivered_id.as_deref(),
            Some("m1"),
            "status must name the last delivered message"
        );
        assert_eq!(watcher.state().delivered_total, 1);

        let calls = watcher.notifier.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "fleet-orch-v1");
        assert_eq!(calls[0].1, "rakazo:thread-b1:m1");
        assert!(
            calls[0]
                .2
                .starts_with("[rakazo:thread-b1:m1] [rakazo] Fleet Orchestrator #7 user: "),
            "{}",
            calls[0].2
        );
        assert!(calls[0].2.contains("hello orchestrator"));
        assert!(!calls[0].2.contains("assistant reply"));

        let second = watcher.poll_once().await.unwrap();
        assert_eq!(second.delivered, 0);
        assert_eq!(second.duplicates, 1);
        assert_eq!(watcher.notifier.calls().len(), 1);
    }

    #[tokio::test]
    async fn restart_from_the_same_state_file_notifies_nothing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("watch.json");
        let page = vec![message("m1", 7, "user", "only once")];
        {
            let source = FakeSource::new(vec![bot("b1", "Orch")], vec![page.clone()]);
            let mut watcher = watch(
                source,
                FakeNotifier::default(),
                Some(path.clone()),
                allowlist("b1"),
            );
            watcher.bind().await.unwrap();
            assert_eq!(watcher.poll_once().await.unwrap().delivered, 1);
            assert!(path.exists());
        }
        let source = FakeSource::new(vec![bot("b1", "Orch")], vec![page]);
        let mut watcher = watch(
            source,
            FakeNotifier::default(),
            Some(path.clone()),
            allowlist("b1"),
        );
        watcher.bind().await.unwrap();
        let report = watcher.poll_once().await.unwrap();
        assert_eq!(report.delivered, 0);
        assert_eq!(report.duplicates, 1);
        assert!(watcher.notifier().calls().is_empty());
        let reloaded: WatchState =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.bot_id, "b1");
        assert_eq!(reloaded.last_seq, 7);
        assert_eq!(reloaded.delivered_ids.len(), 1);
    }

    #[tokio::test]
    async fn assistant_and_echo_rows_are_ignored() {
        let mut echo = message("m3", 9, "user", "our own send");
        echo.client_nonce = Some("our-nonce".into());
        let source = FakeSource::new(
            vec![bot("b1", "Orch")],
            vec![vec![
                message("m1", 7, "bot", "model text"),
                message("m2", 8, "assistant", "model text"),
                echo,
                message("m4", 10, "user", "  "),
            ]],
        );
        let mut watcher = watch(source, FakeNotifier::default(), None, allowlist("b1"));
        watcher.bind().await.unwrap();
        watcher.state.own_nonces.push_back("our-nonce".into());
        let report = watcher.poll_once().await.unwrap();
        assert_eq!(report.delivered, 0);
        assert_eq!(report.ignored, 4);
        assert!(watcher.notifier.calls().is_empty());
        assert_eq!(watcher.state().last_seq, 10);
    }

    #[tokio::test]
    async fn notify_failure_does_not_advance_the_watermark() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("watch.json");
        let page = vec![message("m1", 7, "user", "retry me")];
        {
            let source = FakeSource::new(vec![bot("b1", "Orch")], vec![page.clone()]);
            let mut watcher = watch(
                source,
                FakeNotifier {
                    calls: Mutex::new(Vec::new()),
                    fail: true,
                },
                Some(path.clone()),
                allowlist("b1"),
            );
            watcher.bind().await.unwrap();
            assert!(watcher.poll_once().await.is_err());
            assert_eq!(watcher.state().last_seq, 0);
            assert_eq!(watcher.state().last_poll_ok, Some(false));
            assert!(watcher.state().last_poll_at.is_some());
            assert!(
                watcher
                    .state()
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("scripted notifier failure")),
                "status must surface the last error"
            );
            let saved: WatchState =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(saved.last_seq, 0);
            assert!(saved.delivered_ids.is_empty());
            assert_eq!(saved.last_poll_ok, Some(false));
        }
        // Next poll retries the same message and succeeds.
        let source = FakeSource::new(vec![bot("b1", "Orch")], vec![page]);
        let mut watcher = watch(source, FakeNotifier::default(), Some(path), allowlist("b1"));
        watcher.bind().await.unwrap();
        assert_eq!(watcher.poll_once().await.unwrap().delivered, 1);
        assert_eq!(watcher.notifier.calls().len(), 1);
    }

    #[tokio::test]
    async fn unknown_foreign_and_rebound_agents_are_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("watch.json");
        // Foreign: the allowlisted bot is not listed for this session.
        let source = FakeSource::new(vec![bot("other", "Someone Else")], vec![]);
        let mut watcher = watch(
            source,
            FakeNotifier::default(),
            None,
            allowlist("b-missing"),
        );
        assert!(watcher.bind().await.is_err());

        // Wrong space: listed, same id, but not the configured tenant.
        let source = FakeSource::new(vec![bot("b1", "Orch")], vec![]);
        let mut wrong_space = allowlist("b1");
        wrong_space.space_id = "other-space".into();
        let mut watcher = watch(source, FakeNotifier::default(), None, wrong_space);
        assert!(watcher.bind().await.is_err());

        // Wrong thread: listed, same space, but a different conversation.
        let source = FakeSource::new(vec![bot("b1", "Orch")], vec![]);
        let mut wrong_thread = allowlist("b1");
        wrong_thread.thread_id = "thread-elsewhere".into();
        let mut watcher = watch(source, FakeNotifier::default(), None, wrong_thread);
        assert!(watcher.bind().await.is_err());

        // Bound state: a different agent must not silently take over the file.
        let source = FakeSource::new(vec![bot("b1", "Orch")], vec![Vec::new()]);
        let mut watcher = watch(
            source,
            FakeNotifier::default(),
            Some(path.clone()),
            allowlist("b1"),
        );
        watcher.bind().await.unwrap();
        let source = FakeSource::new(vec![bot("b2", "Other")], vec![]);
        let mut rebind = watch(source, FakeNotifier::default(), Some(path), allowlist("b2"));
        assert!(rebind.bind().await.is_err());
    }

    #[test]
    fn notify_command_is_pi_notify_note_with_ref_and_no_tmux() {
        let args = PiNotify::command_args("fleet-orch-v1", "rakazo:t:m", "body");
        assert_eq!(args[0], "send");
        assert_eq!(args[1], "--kind");
        assert_eq!(args[2], "note");
        assert_eq!(PiNotify::KIND, "note");
        assert!(args.windows(2).any(|pair| pair == ["--ref", "rakazo:t:m"]));
        let joined = args.join(" ");
        assert!(!joined.contains("pi-tmux"), "{joined}");
        assert!(!joined.contains(concat!("send", "-keys")), "{joined}");
        assert!(!joined.contains("--kind steer"), "{joined}");
    }

    #[test]
    fn snippet_is_one_line_and_bounded() {
        let mut msg = message("m", 1, "user", &"x".repeat(500));
        msg.blocks = vec![
            json!({"kind": "text", "text": format!("line one\n\nline two {}", "y".repeat(500))}),
        ];
        let text = summarize("Orch", &msg);
        assert!(!text.contains('\n'));
        assert!(text.starts_with("[rakazo] Orch #1 user: line one line two "));
        assert_eq!(
            text.chars().filter(|c| *c == 'y').count(),
            SNIPPET_CHARS - "line one line two ".len()
        );
        assert!(text.ends_with('…'));
    }
}
