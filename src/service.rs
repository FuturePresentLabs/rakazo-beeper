use crate::{
    config::{Config, Tenant},
    fleet::ack::{
        ACK_REACTION, AckEffects, AckPolicy, AckRequest, Keeper, Obligation, SweepReport,
    },
    matrix::Matrix,
    rakazo::Rakazo,
    stable_id,
    store::{Binding, Inbound, MessageLink, SqliteObligations, Store},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct TenantRuntime {
    pub config: Tenant,
    pub matrix: Matrix,
    pub rakazo: Rakazo,
    pub store: Arc<Store>,
    pub hs_token: String,
    pub as_token: String,
    pub connected: AtomicBool,
    pub last_success: AtomicU64,
    pub unsupported_encryption: AtomicBool,
    pub initialized: AtomicBool,
    /// The bot ids currently selected for this tenant. `discover` refreshes
    /// this from the live space when automatic selectors are enabled, so inbound/queued
    /// delivery cannot keep using a stale allowlist between ticks.
    pub selected: std::sync::RwLock<std::collections::HashSet<String>>,
}

pub struct Runtime {
    pub tenants: Vec<Arc<TenantRuntime>>,
    pub poll_seconds: u64,
    pub store: Arc<Store>,
}

impl Runtime {
    pub fn new(config: &Config, store: Arc<Store>) -> Result<Self> {
        let mut tenants = Vec::new();
        for tenant in &config.tenants {
            let bundle = tenant.bundle()?;
            // Credentials can rotate; identity and namespace cannot silently reuse another tenant's data.
            let fingerprint = stable_id(&[
                &tenant.owner,
                tenant.homeserver.as_str(),
                tenant.rakazo_url.as_str(),
                &tenant.rakazo_space_id,
                &bundle.registration.id,
                &bundle.registration.sender_localpart,
                &bundle.homeserver_domain,
            ]);
            store.bind_tenant(&tenant.id, &fingerprint)?;
            tenants.push(Arc::new(TenantRuntime {
                config: tenant.clone(),
                matrix: Matrix::new(tenant, &bundle)?,
                rakazo: Rakazo::new(tenant)?,
                store: store.clone(),
                hs_token: bundle.registration.hs_token,
                as_token: bundle.registration.as_token,
                connected: AtomicBool::new(!tenant.websocket),
                last_success: AtomicU64::new(0),
                unsupported_encryption: AtomicBool::new(false),
                initialized: AtomicBool::new(false),
                selected: std::sync::RwLock::new(
                    tenant
                        .bot_ids
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<_>>(),
                ),
            }));
        }
        Ok(Self {
            tenants,
            poll_seconds: config.poll_seconds,
            store,
        })
    }
    pub fn ready(&self) -> bool {
        self.tenants.iter().all(|t| {
            t.connected.load(Ordering::Relaxed)
                && !t.unsupported_encryption.load(Ordering::Relaxed)
                && now().saturating_sub(t.last_success.load(Ordering::Relaxed))
                    < (self.poll_seconds * 4).max(90)
        })
    }
}

/// Build the selected bot ids from the live space. Kept as a pure function so
/// the auto-fleet rule can be tested without a Matrix/Rakazo runtime.
fn select_bot_ids(
    bots: &[crate::rakazo::Bot],
    explicit: &[String],
    auto_bots: bool,
    auto_named_bots: &[String],
) -> Vec<String> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let mut push = |id: &str| {
        if seen.insert(id.to_string()) {
            selected.push(id.to_string());
        }
    };
    let active = |bot: &&crate::rakazo::Bot| bot.archived_at.is_none();
    let roots: Vec<&crate::rakazo::Bot> = bots
        .iter()
        .filter(active)
        .filter(|bot| {
            bot.spawn_key.as_deref() == Some("fleet-orch-v1") || bot.name == "Fleet Orchestrator"
        })
        .collect();
    if auto_bots {
        for root in &roots {
            push(&root.id);
            for bot in bots.iter().filter(active) {
                if bot.parent_bot_id.as_deref() == Some(root.id.as_str()) {
                    push(&bot.id);
                }
            }
        }
    }
    for name in auto_named_bots {
        for bot in bots
            .iter()
            .filter(active)
            .filter(|bot| bot.name.eq_ignore_ascii_case(name))
        {
            push(&bot.id);
        }
    }
    for id in explicit {
        if bots.iter().filter(active).any(|bot| &bot.id == id) {
            push(id);
        }
    }
    selected
}

impl TenantRuntime {
    /// @implements url:https://spec.matrix.org/v1.16/application-service-api/ §Pushing events -- durable transaction acknowledgment
    pub fn ingest(&self, txn: &str, payload: &Value) -> Result<()> {
        ensure!(
            !txn.is_empty() && txn.len() <= 1024,
            "invalid transaction id"
        );
        let bindings = self.store.bindings(&self.config.id)?;
        let mut incoming = Vec::new();
        let events = payload
            .get("events")
            .and_then(Value::as_array)
            .context("transaction missing events array")?;
        ensure!(events.len() <= 10_000, "transaction too large");
        for event in events {
            let Some(room) = event["room_id"].as_str() else {
                continue;
            };
            let Some(binding) = bindings
                .iter()
                .find(|b| b.room == room && self.selected(&b.bot))
            else {
                continue;
            };
            if event["type"] == "m.room.encrypted" {
                self.unsupported_encryption.store(true, Ordering::Relaxed);
                tracing::error!(tenant=%self.config.id,"encrypted event received: this version requires unencrypted agent rooms");
                continue;
            }
            if event["sender"] != self.config.owner || event["sender"] == binding.ghost {
                continue;
            }
            if !matches!(
                event["type"].as_str(),
                Some("m.room.message" | "m.reaction" | "m.room.redaction" | "m.sticker")
            ) {
                continue;
            }
            let id = event["event_id"]
                .as_str()
                .context("text event missing event_id")?;
            let text = event["content"]["body"].as_str().unwrap_or("");
            ensure!(
                !id.is_empty() && id.len() <= 1024 && text.len() <= 64_000,
                "text event exceeds bridge limits"
            );
            if text.trim().is_empty()
                && event["type"] == "m.room.message"
                && event["content"]["msgtype"] == "m.text"
            {
                continue;
            }
            incoming.push(Inbound {
                event_id: id.to_string(),
                room: room.to_string(),
                text: text.to_string(),
                content: event.clone(),
            });
        }
        self.store.ingest(&self.config.id, txn, &incoming)
    }

    fn selected(&self, bot: &str) -> bool {
        self.selected
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(bot)
    }

    fn set_selected<'a>(&self, bots: impl IntoIterator<Item = &'a String>) {
        let selected = bots.into_iter().cloned().collect();
        *self
            .selected
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = selected;
    }

    /// The live fleet selection. Explicit `bot_ids` are always included when
    /// they are visible. `auto_bots` adds the orchestrator root and direct child
    /// conversations; `auto_named_bots` adds configured exact names.
    fn selected_bot_ids(&self, bots: &[crate::rakazo::Bot]) -> Vec<String> {
        select_bot_ids(
            bots,
            &self.config.bot_ids,
            self.config.auto_bots,
            &self.config.auto_named_bots,
        )
    }

    pub async fn discover(&self) -> Result<()> {
        if !self.config.auto_bots
            && self.config.bot_ids.is_empty()
            && self.config.auto_named_bots.is_empty()
        {
            self.set_selected(std::iter::empty());
            self.initialized.store(true, Ordering::Relaxed);
            return Ok(());
        }
        let bots = self.rakazo.bots().await?;
        let selected = self.selected_bot_ids(&bots);
        if selected.is_empty() {
            self.set_selected(std::iter::empty());
            self.initialized.store(true, Ordering::Relaxed);
            return Ok(());
        }
        let preferences = self.rakazo.preferences().await?;
        let space = self.rakazo.current_space().await?;
        ensure!(
            space.id == self.config.rakazo_space_id,
            "Rakazo token resolved to a different Space"
        );
        let space_name = space.name.trim();
        let avatar_style = &preferences.avatar_style;
        let features = crate::rich::room_features(&preferences);
        let bridge_logo = self.sync_bridge_identity().await?;
        let bindings = self.store.bindings(&self.config.id)?;
        for bot_id in &selected {
            let bot = bots
                .iter()
                .find(|b| &b.id == bot_id)
                .context("configured bot is not accessible in this Rakazo space")?;
            if let Some(binding) = bindings.iter().find(|b| b.bot == bot.id) {
                self.sync_profile(
                    binding,
                    bot,
                    space_name,
                    avatar_style,
                    &bridge_logo,
                    &features,
                )
                .await?;
                continue;
            }
            let snapshot = self.rakazo.snapshot(&bot.id).await?;
            ensure!(
                !snapshot.busy(),
                "new agent is busy; waiting to establish a completed-message baseline"
            );
            let cursor = snapshot.messages.iter().map(|m| m.seq).max().unwrap_or(-1);
            // Preserve the first baseline if room creation succeeded but its reply was lost.
            let cursor = self.store.baseline(&self.config.id, &bot.id, cursor)?;
            let ghost = self.matrix.ghost(&self.config.id, &bot.id)?;
            self.matrix.register(&ghost, &bot.name).await?;
            let room = self
                .matrix
                .room(
                    &self.config.id,
                    &bot.id,
                    &bot.name,
                    &ghost,
                    &self.config.owner,
                )
                .await?;
            self.matrix
                .check_room(&room, &ghost, &self.config.owner)
                .await?;
            self.store.add_binding(&Binding {
                tenant: self.config.id.clone(),
                bot: bot.id.clone(),
                room,
                ghost,
                cursor,
            })?;
            let binding = self
                .store
                .bindings(&self.config.id)?
                .into_iter()
                .find(|binding| binding.bot == bot.id)
                .context("new binding missing")?;
            self.sync_profile(
                &binding,
                bot,
                space_name,
                avatar_style,
                &bridge_logo,
                &features,
            )
            .await?;
            tracing::info!(tenant=%self.config.id,"provisioned agent chat");
        }
        self.set_selected(selected.iter());
        self.initialized.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn sync_profile(
        &self,
        binding: &Binding,
        bot: &crate::rakazo::Bot,
        space_name: &str,
        style: &str,
        bridge_logo: &str,
        features: &Value,
    ) -> Result<()> {
        let avatar_hash = stable_id(&["rakazo-avatar-v1", &bot.id, &bot.color, style]);
        let applied_hash = stable_id(&[
            &avatar_hash,
            &bot.name,
            space_name,
            bridge_logo,
            features["id"].as_str().unwrap(),
        ]);
        let cached = self.store.profile(&self.config.id, &bot.id)?;
        if cached
            .as_ref()
            .is_some_and(|profile| profile.applied_hash == applied_hash)
        {
            return Ok(());
        }
        self.matrix
            .check_room(&binding.room, &binding.ghost, &self.config.owner)
            .await?;
        let avatar_uri =
            if let Some(profile) = cached.filter(|profile| profile.avatar_hash == avatar_hash) {
                profile.avatar_uri
            } else {
                let avatar = crate::avatar::render(&bot.id, &bot.color, style)?;
                let uri = self.matrix.upload_avatar(&binding.ghost, avatar).await?;
                self.store
                    .cache_avatar(&self.config.id, &bot.id, &avatar_hash, &uri)?;
                uri
            };
        self.matrix
            .sync_profile(&binding.ghost, &binding.room, &bot.name, &avatar_uri)
            .await?;
        self.matrix
            .bridge_info(binding, space_name, &bot.name, bridge_logo, &avatar_uri)
            .await?;
        self.matrix.rich_permissions(binding).await?;
        self.matrix.room_features(binding, features.clone()).await?;
        self.store
            .applied_profile(&self.config.id, &bot.id, &applied_hash)?;
        tracing::info!(tenant=%self.config.id,"synced agent profile picture");
        Ok(())
    }

    async fn sync_bridge_identity(&self) -> Result<String> {
        // Empty bot ID is reserved for tenant-level branding; configured bot IDs are nonempty.
        let bytes = include_bytes!("../assets/fpl-logo.png");
        let hash = stable_id(&["rakazo-bridge-identity-v1", &hex::encode(bytes)]);
        let cached = self.store.profile(&self.config.id, "")?;
        let uri = if let Some(profile) = cached
            .as_ref()
            .filter(|profile| profile.avatar_hash == hash)
        {
            profile.avatar_uri.clone()
        } else {
            let uri = self
                .matrix
                .upload_avatar(
                    &self.matrix.sender,
                    crate::avatar::Avatar {
                        mime: "image/png",
                        bytes: bytes.to_vec(),
                    },
                )
                .await?;
            self.store.cache_avatar(&self.config.id, "", &hash, &uri)?;
            uri
        };
        if cached.is_none_or(|profile| profile.applied_hash != hash) {
            self.matrix.bridge_identity(&uri).await?;
            self.store.applied_profile(&self.config.id, "", &hash)?;
        }
        Ok(uri)
    }

    pub async fn deliver_inbound(&self) -> Result<()> {
        // One worker per tenant and a process lock preserve send order across retries.
        for _ in 0..20 {
            let Some(pending) = self.store.next(&self.config.id, now() as i64)? else {
                break;
            };
            let bindings = self.store.bindings(&self.config.id)?;
            let binding = bindings.iter().find(|b| b.room == pending.event.room && self.selected(&b.bot))
                .context("queued message belongs to a disabled bot; restore configuration before delivery")?;
            // The mid-thread test reads the agent's state *before* this message
            // starts a run: an idle agent answers, it does not acknowledge.
            let result = async {
                self.matrix
                    .check_room(&binding.room, &binding.ghost, &self.config.owner)
                    .await?;
                self.bootstrap_links(binding).await?;
                let busy = self.agent_busy(binding).await;
                self.inbound_content(binding, &pending.event)
                    .await
                    .map(|seq| (seq, busy))
            }
            .await;
            match result {
                Ok((Some(seq), busy)) => {
                    self.store
                        .complete_send(binding, pending.id, &pending.event.event_id, seq)?;
                    // An acknowledgement is a promise, so it is only made when
                    // the policy allows it and the promise can be recorded.
                    if let Err(error) = self.consider_ack(binding, &pending.event, seq, busy).await
                    {
                        tracing::warn!(
                            tenant = %self.config.id,
                            bot = %binding.bot,
                            error = %error,
                            "acknowledgement failed; the message was still delivered"
                        );
                    }
                }
                Ok((None, _)) => self.store.complete(&self.config.id, pending.id)?,
                Err(error) if error.downcast_ref::<crate::media::Rejected>().is_some() => {
                    self.matrix
                        .check_room(&binding.room, &binding.ghost, &self.config.owner)
                        .await?;
                    let text = format!(
                        "Could not bridge this event: {}",
                        error.downcast_ref::<crate::media::Rejected>().unwrap()
                    );
                    let txn = stable_id(&[&binding.tenant, &pending.event.event_id, "rejected"]);
                    let notice_id = self
                        .matrix
                        .send_event(
                            &binding.room,
                            &binding.ghost,
                            &txn,
                            "m.room.message",
                            serde_json::json!({"msgtype":"m.notice","body":text}),
                        )
                        .await?;
                    self.store.link(
                        binding,
                        &MessageLink {
                            rakazo_id: format!("bridge-notice:{}", pending.event.event_id),
                            matrix_id: notice_id,
                            part: "bridge-notice".into(),
                            role: "bot".into(),
                            kind: "m.room.message".into(),
                            hash: stable_id(&[&text]),
                        },
                    )?;
                    self.store.fail(&self.config.id, pending.id)?;
                    tracing::warn!(tenant=%self.config.id,"event rejected; owner notified");
                }
                Err(error) => {
                    self.store.retry(
                        &self.config.id,
                        pending.id,
                        pending.attempts,
                        now() as i64,
                    )?;
                    return Err(error).context("queued message retained for retry");
                }
            }
        }
        Ok(())
    }

    pub async fn poll_binding(&self, binding: &Binding) -> Result<()> {
        // Recover an ambiguous send receipt before delivering its reply, so a
        // lost HTTP response cannot drop the original Beeper thread routing.
        if self.store.pending_room_count(binding)? > 0 {
            self.matrix
                .check_room(&binding.room, &binding.ghost, &self.config.owner)
                .await?;
            return Ok(());
        }
        let event_cursor = self
            .store
            .sync_cursor(binding, "thread-events")?
            .unwrap_or(-1);
        let snapshot = self.rakazo.snapshot(&binding.bot).await?;
        let head = self.rakazo.thread_head(&binding.bot).await?;
        ensure!(
            snapshot.thread_id == head.thread_id && snapshot.cursor == head.cursor,
            "Rakazo thread changed during sync"
        );
        let events = self
            .rakazo
            .thread_events(&binding.bot, event_cursor, head.cursor)
            .await?;
        let mut previous = event_cursor;
        for event in &events {
            ensure!(
                event.space_id == self.config.rakazo_space_id,
                "Rakazo event belongs to a different Space"
            );
            ensure!(
                event.bot_id == binding.bot,
                "Rakazo event belongs to a different bot"
            );
            ensure!(
                event.thread_id == head.thread_id,
                "Rakazo event belongs to a different thread"
            );
            ensure!(
                event.seq > previous && event.seq <= head.cursor,
                "Rakazo event stream did not progress"
            );
            previous = event.seq;
        }
        if let Some(clear_index) = events
            .iter()
            .rposition(|event| event.kind == "thread.cleared")
        {
            let clear = &events[clear_index];
            let mut preserve = std::collections::HashSet::new();
            let mut preserve_runs = std::collections::HashSet::new();
            for event in &events[clear_index + 1..] {
                if event.kind == "thread.message.created" {
                    preserve.insert(
                        event.payload["messageId"]
                            .as_str()
                            .context("Rakazo message event is missing its message ID")?
                            .to_owned(),
                    );
                    if let Some(run) = event.run_id.as_deref() {
                        preserve_runs.insert(run.to_owned());
                    }
                    for run in event.payload["runIds"].as_array().into_iter().flatten() {
                        preserve_runs.insert(
                            run.as_str()
                                .context("Rakazo message event contains an invalid run ID")?
                                .to_owned(),
                        );
                    }
                }
            }
            self.clear_matrix_timeline(binding, &clear.id, clear.seq, &preserve, &preserve_runs)
                .await?;
        }
        if event_cursor < head.cursor {
            self.store
                .set_sync_cursor(binding, "thread-events", head.cursor)?;
        }
        if snapshot.busy() {
            return Ok(());
        }
        // Freeze the upper bound before paginating: a new run can begin during the read.
        let through = snapshot
            .messages
            .iter()
            .map(|m| m.seq)
            .max()
            .unwrap_or(binding.cursor);
        let messages = self.rakazo.after(&binding.bot, binding.cursor).await?;
        if messages.is_empty() {
            return Ok(());
        }
        self.matrix
            .check_room(&binding.room, &binding.ghost, &self.config.owner)
            .await?;
        self.bootstrap_links(binding).await?;
        for message in messages.into_iter().filter(|m| m.seq <= through) {
            self.relay_message(binding, &message, true).await?;
            if message.role == "bot" {
                // The agent answered in this thread: every promise it owed
                // here is kept.
                self.fulfil_obligations(binding, &message).await?;
            }
            self.store.cursor(binding, message.seq)?;
        }
        Ok(())
    }

    /// True when the agent is already working on a prior task.
    ///
    /// This is the mid-thread signal: an agent with a live run cannot answer
    /// a new message yet, so an acknowledgement is a promise instead of a
    /// receipt. An unreadable state is treated as idle: no acknowledgement is
    /// the safe answer.
    pub async fn agent_busy(&self, binding: &Binding) -> bool {
        if self.config.ack_policy != AckPolicy::MidThreadOnly {
            return false;
        }
        match self.rakazo.snapshot(&binding.bot).await {
            Ok(snapshot) => snapshot.busy(),
            Err(error) => {
                tracing::warn!(
                    tenant = %self.config.id,
                    bot = %binding.bot,
                    error = %error,
                    "agent state unknown; no acknowledgement"
                );
                false
            }
        }
    }

    /// Consider acknowledging one delivered message.
    ///
    /// The promise is recorded before the tapback is sent, so a promise that
    /// cannot be stored is never made.
    async fn consider_ack(
        &self,
        binding: &Binding,
        event: &Inbound,
        seq: i64,
        busy: bool,
    ) -> Result<()> {
        let request = AckRequest {
            agent: binding.bot.clone(),
            thread: binding.room.clone(),
            message_id: event.event_id.clone(),
            seq,
            busy,
            // A channel the agent cannot answer is never acknowledged. The
            // room check above already failed closed when the bridge bot was
            // no longer joined, so a selected agent here can answer here.
            can_reply: self.selected(&binding.bot),
            run_id: self
                .store
                .runs_for_event(binding, &event.event_id)?
                .into_iter()
                .next(),
        };
        let mut keeper = self.ack_keeper();
        let outcome = keeper.consider(&request, now() as i64).await;
        tracing::debug!(
            tenant = %self.config.id,
            agent = %request.agent,
            thread = %request.thread,
            message = %request.message_id,
            ack_id = ?outcome.ack_id,
            reason = %outcome.reason,
            "acknowledgement considered"
        );
        Ok(())
    }

    /// Close the promises an agent reply kept.
    ///
    /// The thread and the ordering are enough: the reply does not have to
    /// quote the acknowledged message.
    async fn fulfil_obligations(
        &self,
        binding: &Binding,
        message: &crate::rakazo::Message,
    ) -> Result<()> {
        let mut keeper = self.ack_keeper();
        keeper
            .fulfil(
                &binding.bot,
                &binding.room,
                &message.id,
                message.run_id.as_deref(),
                message.seq,
            )
            .await?;
        Ok(())
    }

    /// The hook: enforce every acknowledgement promise that is due.
    ///
    /// One nudge per promise, then one visible admission in the thread. The
    /// sweep runs on the bridge's existing cadence; it is not a new daemon.
    pub async fn sweep_obligations(&self, now: i64) -> Result<SweepReport> {
        let mut keeper = self.ack_keeper();
        let report = keeper.sweep(now).await?;
        for error in &report.errors {
            tracing::warn!(
                tenant = %self.config.id,
                error = %error,
                "acknowledgement sweep failed"
            );
        }
        Ok(report)
    }

    /// The acknowledgement keeper of this tenant.
    fn ack_keeper(&self) -> Keeper<SqliteObligations, BridgeAckEffects<'_>> {
        Keeper::new(
            SqliteObligations::new(self.store.clone(), self.config.id.clone()),
            BridgeAckEffects { runtime: self },
            self.config.ack_policy,
            self.config.ack_due_seconds as i64,
            self.config.ack_grace_seconds as i64,
        )
    }

    pub async fn backfill_binding(&self, binding: &Binding) -> Result<()> {
        let Some(before) = self.store.history_before(binding)? else {
            // Add media omitted by v1 without replaying its text transcript.
            let before = self.store.sync_cursor(binding, "rich-history")?.unwrap_or(
                binding
                    .cursor
                    .checked_add(1)
                    .context("history cursor overflow")?,
            );
            if before < 0 {
                return Ok(());
            }
            self.bootstrap_links(binding).await?;
            let mut page = self.rakazo.history_page(&binding.bot, before).await?;
            page.messages.sort_by_key(|message| message.seq);
            self.relay_page(binding, &page.messages).await?;
            self.store
                .set_sync_cursor(binding, "rich-history", page.older_cursor.unwrap_or(-1))?;
            return Ok(());
        };
        let mut page = self.rakazo.history_page(&binding.bot, before).await?;
        page.messages.sort_by_key(|message| message.seq);
        page.messages.dedup_by_key(|message| message.seq);
        self.relay_page(binding, &page.messages).await?;
        self.store.history_progress(binding, page.older_cursor)?;
        Ok(())
    }

    pub async fn tick(&self) -> Result<()> {
        // A failing agent must not starve other agents or inbound delivery.
        let mut failure = None;
        if let Err(error) = self.discover().await {
            failure = Some(error);
        }
        if let Err(error) = self.deliver_inbound().await {
            failure = Some(error);
        }
        for binding in self
            .store
            .bindings(&self.config.id)?
            .iter()
            .filter(|b| self.selected(&b.bot))
        {
            if let Err(error) = self.backfill_binding(binding).await {
                tracing::warn!(tenant=%self.config.id,error=%error,"history import failed; cursor retained");
                failure = Some(error);
            }
            if let Err(error) = self.poll_binding(binding).await {
                tracing::warn!(tenant=%self.config.id,error=%error,"agent sync failed; cursor retained");
                failure = Some(error);
            }
        }
        // The acknowledgement hook runs on the same cadence as everything
        // else: one nudge per broken promise, then one visible admission.
        if let Err(error) = self.sweep_obligations(now() as i64).await {
            tracing::warn!(tenant=%self.config.id,error=%error,"acknowledgement sweep failed; obligations retained");
            failure = Some(error);
        }
        if let Some(error) = failure {
            return Err(error);
        }
        self.last_success.store(now(), Ordering::Relaxed);
        Ok(())
    }
}

/// The visible side of an acknowledgement promise.
///
/// * the tapback says "I saw this and I will follow up",
/// * the nudge reminds the agent once,
/// * the admission tells the human, in the thread, that the promise was
///   broken.
///
/// Every write is idempotent: the transaction or nonce is derived from the
/// obligation's `ack_id`, so a retry is a replay and never a second message.
struct BridgeAckEffects<'a> {
    runtime: &'a TenantRuntime,
}

impl BridgeAckEffects<'_> {
    fn binding_for(&self, agent: &str) -> Result<Binding> {
        self.runtime
            .store
            .bindings(&self.runtime.config.id)?
            .into_iter()
            .find(|binding| binding.bot == agent)
            .with_context(|| format!("no bridge room for agent {agent}"))
    }
}

impl AckEffects for BridgeAckEffects<'_> {
    async fn acknowledge(&self, obligation: &Obligation) -> Result<()> {
        let binding = self.binding_for(&obligation.agent)?;
        let txn = stable_id(&["ack", &obligation.ack_id]);
        self.runtime
            .matrix
            .send_event(
                &binding.room,
                &binding.ghost,
                &txn,
                "m.reaction",
                serde_json::json!({
                    "m.relates_to": {
                        "rel_type": "m.annotation",
                        "event_id": obligation.message_id,
                        "key": ACK_REACTION,
                    }
                }),
            )
            .await?;
        Ok(())
    }

    async fn nudge(&self, obligation: &Obligation) -> Result<()> {
        let binding = self.binding_for(&obligation.agent)?;
        let nonce = stable_id(&["ack-nudge", &obligation.ack_id]);
        // The bridge wrote this message, so it is never the human's: it must
        // not be relayed back into the human's room.
        self.runtime
            .store
            .record_bridge_outbound(&binding, &nonce, "ack-nudge")?;
        let text = format!(
            "[bridge] follow-up reminder: the human's message {} was acknowledged and has \
             not been answered yet. Please follow up in this thread.",
            obligation.message_id
        );
        self.runtime
            .rakazo
            .send_content(&binding.bot, &text, &[], None, &nonce)
            .await?;
        Ok(())
    }

    async fn admit(&self, obligation: &Obligation, notice: &str) -> Result<()> {
        let binding = self.binding_for(&obligation.agent)?;
        let txn = stable_id(&["ack-admission", &obligation.ack_id]);
        self.runtime
            .matrix
            .send(&binding.room, &binding.ghost, &txn, notice)
            .await?;
        Ok(())
    }
}

pub fn text_chunks(mut text: &str, limit: usize) -> Vec<&str> {
    assert!(limit >= 4);
    let mut chunks = Vec::new();
    while !text.is_empty() {
        let mut end = text.len().min(limit);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[..end]);
        text = &text[end..];
    }
    chunks
}

#[cfg(test)]
mod selection_tests {
    use super::select_bot_ids;
    use crate::rakazo::Bot;

    fn bot(id: &str, name: &str, parent: Option<&str>, spawn: Option<&str>, archived: bool) -> Bot {
        Bot {
            id: id.into(),
            name: name.into(),
            color: String::new(),
            title: String::new(),
            description: String::new(),
            space_id: "space".into(),
            thread_id: Some(format!("thread-{id}")),
            parent_bot_id: parent.map(str::to_string),
            spawn_key: spawn.map(str::to_string),
            archived_at: archived.then(|| "2026-10-26T00:00:00Z".into()),
            capabilities: None,
            relay_only: None,
        }
    }

    #[test]
    fn auto_fleet_selects_only_orchestrator_and_children_by_default() {
        let bots = vec![
            bot(
                "orch",
                "Fleet Orchestrator",
                None,
                Some("fleet-orch-v1"),
                false,
            ),
            bot("pm-fpl", "PM · FPL", Some("orch"), None, false),
            bot("pm-rakazo", "PM · Rakazo", Some("orch"), None, false),
            bot("alfred", "Alfred", None, None, false),
            bot("muse", "Muse", None, None, false),
            bot("old-pm", "PM · Old", Some("orch"), None, true),
        ];
        let selected = select_bot_ids(&bots, &[], true, &[]);
        assert_eq!(selected, vec!["orch", "pm-fpl", "pm-rakazo"]);
    }

    #[test]
    fn configured_named_bots_are_selected_without_hardcoded_policy() {
        let bots = vec![
            bot("muse", "Muse", None, None, false),
            bot("oracle", "Oracle", None, None, false),
            bot("old", "Muse", None, None, true),
        ];
        assert_eq!(
            select_bot_ids(&bots, &[], false, &["mUsE".into()]),
            vec!["muse"]
        );
    }

    #[test]
    fn explicit_ids_are_added_when_auto_is_enabled_and_missing_ids_are_ignored() {
        let bots = vec![
            bot(
                "orch",
                "Fleet Orchestrator",
                None,
                Some("fleet-orch-v1"),
                false,
            ),
            bot("extra", "Extra", None, None, false),
        ];
        let explicit = vec!["extra".to_string(), "missing".to_string()];
        assert_eq!(
            select_bot_ids(&bots, &explicit, true, &[]),
            vec!["orch", "extra"]
        );
    }

    #[test]
    fn auto_off_keeps_the_explicit_allowlist_contract() {
        let bots = vec![
            bot(
                "orch",
                "Fleet Orchestrator",
                None,
                Some("fleet-orch-v1"),
                false,
            ),
            bot("extra", "Extra", None, None, false),
        ];
        assert!(select_bot_ids(&bots, &[], false, &[]).is_empty());
        assert_eq!(
            select_bot_ids(&bots, &["extra".into()], false, &[]),
            vec!["extra"]
        );
    }
}
