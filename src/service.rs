use crate::{
    config::{Config, Tenant},
    matrix::Matrix,
    rakazo::Rakazo,
    stable_id,
    store::{Binding, Inbound, Store},
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
                .find(|b| b.room == room && self.config.bot_ids.contains(&b.bot))
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
            if event["type"] != "m.room.message" {
                continue;
            }
            if event["content"]["msgtype"] != "m.text"
                || event["content"]["m.relates_to"]["rel_type"] == "m.replace"
            {
                tracing::warn!(tenant=%self.config.id,"unsupported message type or edit ignored; only new text messages are bridged");
                continue;
            }
            let id = event["event_id"]
                .as_str()
                .context("text event missing event_id")?;
            let text = event["content"]["body"]
                .as_str()
                .context("text event missing body")?;
            ensure!(
                !id.is_empty() && id.len() <= 1024 && text.len() <= 64_000,
                "text event exceeds bridge limits"
            );
            if text.trim().is_empty() {
                continue;
            }
            incoming.push(Inbound {
                event_id: id.to_string(),
                room: room.to_string(),
                text: text.to_string(),
            });
        }
        self.store.ingest(&self.config.id, txn, &incoming)
    }

    pub async fn discover(&self) -> Result<()> {
        if self.config.bot_ids.is_empty() {
            self.initialized.store(true, Ordering::Relaxed);
            return Ok(());
        }
        let bots = self.rakazo.bots().await?;
        let avatar_style = self.rakazo.avatar_style().await?;
        let bindings = self.store.bindings(&self.config.id)?;
        for bot_id in &self.config.bot_ids {
            let bot = bots
                .iter()
                .find(|b| &b.id == bot_id)
                .context("configured bot is not accessible in this Rakazo space")?;
            if let Some(binding) = bindings.iter().find(|b| b.bot == bot.id) {
                self.sync_profile(binding, bot, &avatar_style).await?;
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
            self.sync_profile(&binding, bot, &avatar_style).await?;
            tracing::info!(tenant=%self.config.id,"provisioned agent chat");
        }
        self.initialized.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn sync_profile(
        &self,
        binding: &Binding,
        bot: &crate::rakazo::Bot,
        style: &str,
    ) -> Result<()> {
        let avatar_hash = stable_id(&["rakazo-avatar-v1", &bot.id, &bot.color, style]);
        let applied_hash = stable_id(&[&avatar_hash, &bot.name]);
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
        self.store
            .applied_profile(&self.config.id, &bot.id, &applied_hash)?;
        tracing::info!(tenant=%self.config.id,"synced agent profile picture");
        Ok(())
    }

    pub async fn deliver_inbound(&self) -> Result<()> {
        // One worker per tenant and a process lock preserve send order across retries.
        for _ in 0..20 {
            let Some(pending) = self.store.next(&self.config.id, now() as i64)? else {
                break;
            };
            let bindings = self.store.bindings(&self.config.id)?;
            let binding = bindings.iter().find(|b| b.room == pending.event.room && self.config.bot_ids.contains(&b.bot))
                .context("queued message belongs to a disabled bot; restore configuration before delivery")?;
            let result = async {
                self.matrix
                    .check_room(&binding.room, &binding.ghost, &self.config.owner)
                    .await?;
                let nonce = stable_id(&[&self.config.id, &pending.event.event_id]);
                self.rakazo
                    .send(&binding.bot, &pending.event.text, &nonce)
                    .await
            }
            .await;
            match result {
                Ok(()) => self.store.complete(&self.config.id, pending.id)?,
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
        let snapshot = self.rakazo.snapshot(&binding.bot).await?;
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
        for message in messages.into_iter().filter(|m| m.seq <= through) {
            if message.role == "bot" {
                let text = message.text();
                for (index, chunk) in text_chunks(&text, 40_000).into_iter().enumerate() {
                    let txn = stable_id(&[
                        &self.config.id,
                        &binding.bot,
                        &message.id,
                        &index.to_string(),
                    ]);
                    self.matrix
                        .send(&binding.room, &binding.ghost, &txn, chunk)
                        .await?;
                }
            }
            self.store.cursor(binding, message.seq)?;
        }
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
            .filter(|b| self.config.bot_ids.contains(&b.bot))
        {
            if let Err(error) = self.poll_binding(binding).await {
                tracing::warn!(tenant=%self.config.id,error=%error,"agent sync failed; cursor retained");
                failure = Some(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        self.last_success.store(now(), Ordering::Relaxed);
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
