//! Message relationships are scoped to a tenant and bot, including during upgrades.
use crate::{
    media::{self, Rejected},
    rakazo::Message,
    service::{TenantRuntime, text_chunks},
    stable_id,
    store::{Binding, Inbound, MessageLink},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub struct Part {
    pub key: String,
    pub kind: String,
    pub content: Value,
}

/// @implements github:mautrix/go@v0.28.1 event/capabilities.go RoomFeatures -- protocol signaling, not bridge implementation
pub fn room_features(preferences: &crate::rakazo::Preferences) -> Value {
    // Compatibility snapshot: older Rakazo me responses advertise the size limit
    // but no MIME list. Prefer the server's source-of-truth list when available.
    // @see github:FuturePresentLabs/fpl-agents@0d1fb12 packages/contracts/src/attachments.ts
    const LEGACY_TYPES: &[&str] = &[
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/gif",
        "video/mp4",
        "application/pdf",
        "text/plain",
        "text/markdown",
        "text/csv",
        "text/html",
        "application/json",
        "application/zip",
    ];
    let types = preferences
        .attachment_mime_types
        .as_ref()
        .map(|types| types.iter().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_else(|| LEGACY_TYPES.to_vec());
    let size = preferences
        .max_upload_size_bytes
        .unwrap_or(media::MAX_BYTES)
        .min(media::MAX_BYTES);
    let mut files = serde_json::Map::new();
    for kind in [
        "m.image",
        "m.video",
        "m.audio",
        "m.file",
        "m.sticker",
        "fi.mau.gif",
        "org.matrix.msc3245.voice",
    ] {
        let supported = types
            .iter()
            .filter(|mime| match kind {
                "m.file" => true,
                "m.sticker" | "m.image" => mime.starts_with("image/"),
                "m.video" => mime.starts_with("video/"),
                "fi.mau.gif" => **mime == "image/gif",
                _ => mime.starts_with("audio/"),
            })
            .map(|mime| ((*mime).to_string(), json!(2)))
            .collect::<serde_json::Map<_, _>>();
        if !supported.is_empty() {
            files.insert(
                kind.to_string(),
                json!({"mime_types":supported,"caption":2,"max_size":size}),
            );
        }
    }
    let mut features = json!({"file":files,"max_text_length":64000,"reply":2,"thread":1,"reaction":1,"allowed_reactions":["👍","👎","❤️","😂","🎉","😮"],"edit":-2,"delete":-2,"location_message":-2,"poll":-2});
    features["id"] = json!(stable_id(&["rakazo-rich-v1", &features.to_string()]));
    features
}

fn hash(value: &Value) -> String {
    stable_id(&[&value.to_string()])
}

/// @implements url:https://spec.matrix.org/v1.16/client-server-api/#rich-replies -- remove only the plain-text reply fallback
pub fn reply_body(content: &Value) -> String {
    let body = content["body"].as_str().unwrap_or("");
    if content["m.relates_to"]["m.in_reply_to"]["event_id"].is_string()
        && let Some((quote, text)) = body.split_once("\n\n")
        && quote
            .lines()
            .all(|line| line.starts_with("> ") || line == ">")
    {
        return text.to_string();
    }
    body.to_string()
}

#[derive(Default)]
struct Prepared {
    events: Vec<Value>,
    links: Vec<MessageLink>,
}

impl TenantRuntime {
    pub fn source_event_id(
        &self,
        binding: &Binding,
        message: &Message,
        key: &str,
        baseline: i64,
    ) -> String {
        let digest = if message.seq <= baseline && key.starts_with("text:") {
            stable_id(&[
                "history-v1",
                &binding.tenant,
                &binding.bot,
                &binding.room,
                &message.id,
                key.strip_prefix("text:").unwrap(),
            ])
        } else {
            stable_id(&[
                "message-v2",
                &binding.tenant,
                &binding.bot,
                &binding.room,
                &message.id,
                key,
            ])
        };
        format!("${digest}:{}", self.matrix.domain)
    }

    pub async fn source_message(&self, binding: &Binding, id: &str) -> Result<Message> {
        let page: crate::rakazo::Page = self
            .rakazo
            .rpc(
                &["threads", "messages"],
                json!({"botId":binding.bot,"around":{"messageId":id}}),
            )
            .await?;
        page.messages
            .into_iter()
            .find(|message| message.id == id)
            .context("reply target is not accessible in this agent thread")
    }

    pub async fn target(&self, binding: &Binding, event: &str) -> Result<MessageLink> {
        if let Some(link) = self.store.matrix_link(binding, event)? {
            return Ok(link);
        }
        if let Some(seq) = self.store.receipt(binding, event)? {
            let page = self
                .rakazo
                .history_page(
                    &binding.bot,
                    seq.checked_add(1).context("sequence overflow")?,
                )
                .await?;
            let message = page
                .messages
                .into_iter()
                .find(|message| message.seq == seq && message.role == "user")
                .context("sent message receipt is not visible yet")?;
            let link = MessageLink {
                rakazo_id: message.id,
                matrix_id: event.to_string(),
                part: "text:0".into(),
                role: "user".into(),
                kind: "m.room.message".into(),
                hash: String::new(),
            };
            self.store.link(binding, &link)?;
            return Ok(link);
        }
        Err(Rejected("That message is not linked to this Rakazo chat.").into())
    }

    /// Recover v1 IDs from existing server events, never by resending an expired transaction.
    pub async fn bootstrap_links(&self, binding: &Binding) -> Result<()> {
        if self.store.links_initialized(binding)? {
            return Ok(());
        }
        let baseline = self
            .store
            .baseline(&binding.tenant, &binding.bot, binding.cursor)?;
        let messages = self.rakazo.after(&binding.bot, -1).await?;
        if messages.is_empty() {
            self.store.initialize_links(binding)?;
            return Ok(());
        }
        let history_before = self.store.history_before(binding)?;
        let mut needed = HashSet::new();
        for message in messages.iter().filter(|message| {
            message.seq > baseline && message.seq <= binding.cursor && message.role == "bot"
        }) {
            for (index, _) in text_chunks(&message.text(), 40_000).iter().enumerate() {
                if self
                    .store
                    .links(binding, &message.id)?
                    .iter()
                    .any(|link| link.part == format!("text:{index}"))
                {
                    continue;
                }
                needed.insert(stable_id(&[
                    &binding.tenant,
                    &binding.bot,
                    &message.id,
                    &index.to_string(),
                ]));
            }
        }
        let mut transactions = HashMap::new();
        let mut from = None::<String>;
        for _ in 0..1000 {
            if needed.is_empty() {
                break;
            }
            let page = self
                .matrix
                .room_messages(&binding.room, &binding.ghost, from.as_deref())
                .await?;
            let rows = page["chunk"]
                .as_array()
                .context("Matrix messages missing chunk")?;
            for event in rows {
                if event["room_id"]
                    .as_str()
                    .is_some_and(|room| room != binding.room)
                {
                    continue;
                }
                if let Some(id) = event["event_id"].as_str()
                    && event["sender"] == binding.ghost
                    && let Some(txn) = event["unsigned"]["transaction_id"].as_str()
                {
                    transactions.insert(txn.to_string(), id.to_string());
                    needed.remove(txn);
                }
            }
            let next = page["end"].as_str().map(str::to_string);
            if rows.len() < 1000 || next.is_none() {
                break;
            }
            ensure!(next != from, "Matrix link recovery did not progress");
            from = next;
        }
        ensure!(
            needed.is_empty(),
            "an existing live message could not be recovered; refusing to replay it"
        );
        for message in messages
            .iter()
            .filter(|message| message.seq <= binding.cursor)
        {
            let text = message.text();
            for (index, chunk) in text_chunks(&text, 40_000).into_iter().enumerate() {
                let key = format!("text:{index}");
                let id = if message.seq <= baseline {
                    let id = self.source_event_id(binding, message, &key, baseline);
                    history_before
                        .is_none_or(|before| message.seq >= before)
                        .then_some(id)
                } else if message.role == "bot" {
                    transactions
                        .get(&stable_id(&[
                            &binding.tenant,
                            &binding.bot,
                            &message.id,
                            &index.to_string(),
                        ]))
                        .cloned()
                } else if let Some(nonce) = message.client_nonce.as_deref() {
                    self.store.event_for_nonce(binding, nonce)?
                } else {
                    None
                };
                if let Some(id) = id {
                    self.store.link(
                        binding,
                        &MessageLink {
                            rakazo_id: message.id.clone(),
                            matrix_id: id,
                            part: key,
                            role: message.role.clone(),
                            kind: "m.room.message".into(),
                            hash: hash(&json!({"msgtype":"m.text","body":chunk})),
                        },
                    )?;
                }
            }
        }
        self.store.initialize_links(binding)?;
        Ok(())
    }

    pub async fn inbound_content(&self, binding: &Binding, event: &Inbound) -> Result<Option<i64>> {
        let nonce = stable_id(&[&binding.tenant, &event.event_id]);
        if event.content.is_null() {
            return Ok(Some(
                self.rakazo
                    .send_content(&binding.bot, &event.text, &[], None, &nonce)
                    .await?,
            ));
        }
        let raw = &event.content;
        let content = &raw["content"];
        if raw["type"] == "m.room.redaction" {
            return Err(
                Rejected("Removing a Beeper message does not retract it from the agent.").into(),
            );
        }
        if raw["type"] == "m.reaction" {
            ensure!(
                content["m.relates_to"]["rel_type"] == "m.annotation",
                Rejected("Unsupported reaction relationship.")
            );
            let target = self
                .target(
                    binding,
                    content["m.relates_to"]["event_id"]
                        .as_str()
                        .ok_or(Rejected("Reaction is missing its target."))?,
                )
                .await?;
            let reaction = content["m.relates_to"]["key"]
                .as_str()
                .ok_or(Rejected("Reaction is missing its emoji."))?;
            self.rakazo
                .react(&binding.bot, &target.rakazo_id, reaction, &nonce)
                .await?;
            return Ok(None);
        }
        ensure!(
            raw["type"] == "m.room.message" || raw["type"] == "m.sticker",
            Rejected("This message type is not supported by Rakazo.")
        );
        ensure!(
            content["m.relates_to"]["rel_type"] != "m.replace",
            Rejected("Edits are not forwarded; send a new message to correct the agent.")
        );
        let reply = if let Some(id) = content["m.relates_to"]["m.in_reply_to"]["event_id"]
            .as_str()
            .or_else(|| {
                (content["m.relates_to"]["rel_type"] == "m.thread")
                    .then(|| content["m.relates_to"]["event_id"].as_str())
                    .flatten()
            }) {
            Some(self.target(binding, id).await?.rakazo_id)
        } else {
            None
        };
        let kind = content["msgtype"].as_str().unwrap_or("");
        let (text, artifacts) = if matches!(kind, "m.image" | "m.file" | "m.video" | "m.audio")
            || raw["type"] == "m.sticker"
        {
            let _permit = media::TRANSFER
                .acquire()
                .await
                .context("media transfer budget unavailable")?;
            let media = self
                .matrix
                .download_media(&binding.ghost, content, media::MAX_BYTES)
                .await?;
            let id = self
                .rakazo
                .create_artifact(&binding.bot, &media, &nonce)
                .await?;
            let body = reply_body(content);
            let text = if content["filename"].is_string()
                && body != content["filename"].as_str().unwrap()
            {
                body
            } else {
                String::new()
            };
            (text, vec![id])
        } else {
            ensure!(
                matches!(kind, "m.text" | "m.notice" | "m.emote"),
                Rejected("This message format cannot be sent to Rakazo.")
            );
            (reply_body(content), vec![])
        };
        let receipt = self
            .rakazo
            .send_routed(&binding.bot, &text, &artifacts, reply.as_deref(), &nonce)
            .await?;
        let thread = (content["m.relates_to"]["rel_type"] == "m.thread")
            .then(|| content["m.relates_to"]["event_id"].as_str())
            .flatten();
        for run in std::iter::once(&receipt.run_id).chain(receipt.run_ids.iter()) {
            self.store
                .save_run_route(binding, run, &event.event_id, thread)?;
        }
        Ok(Some(receipt.seq))
    }

    pub async fn message_parts(&self, binding: &Binding, message: &Message) -> Result<Vec<Part>> {
        let mut parts = Vec::new();
        let existing = self.store.links(binding, &message.id)?;
        let text = message.text();
        // Rakazo itself projects a single-emoji owner reply as a quiet reaction.
        // @implements github:FuturePresentLabs/fpl-agents@0d1fb12 packages/core/src/message-reactions.ts -- native reaction projection
        if message.role == "user"
            && message.reply_to_message_id.is_some()
            && message.blocks.len() == 1
            && matches!(text.as_str(), "👍" | "👎" | "❤️" | "😂" | "🎉" | "😮")
        {
            // Preserve old flattened emoji bubbles rather than duplicating them
            // as a second reaction during an upgrade.
            if existing.iter().any(|link| link.kind == "m.room.message") {
                return Ok(parts);
            }
            let target = self
                .reply_target(binding, message.reply_to_message_id.as_deref().unwrap())
                .await?;
            return Ok(vec![Part {
                key: "reaction".into(),
                kind: "m.reaction".into(),
                content: json!({"m.relates_to":{"rel_type":"m.annotation","event_id":target,"key":text}}),
            }]);
        }
        for (index, chunk) in text_chunks(&text, 40_000).into_iter().enumerate() {
            parts.push(Part {
                key: format!("text:{index}"),
                kind: "m.room.message".into(),
                content: json!({"msgtype":"m.text","body":chunk}),
            });
        }
        for block in &message.blocks {
            if !matches!(block["kind"].as_str(), Some("image" | "file")) {
                continue;
            }
            let id = block["artifactId"]
                .as_str()
                .context("attachment block missing artifactId")?;
            if existing
                .iter()
                .any(|link| link.part == format!("artifact:{id}"))
            {
                continue;
            }
            let _permit = media::TRANSFER
                .acquire()
                .await
                .context("media transfer budget unavailable")?;
            let media = match self.rakazo.artifact(&binding.bot, id).await {
                Ok(media) => media,
                Err(error) if error.downcast_ref::<Rejected>().is_some() => {
                    parts.push(Part {key:format!("rejected-artifact:{id}"),kind:"m.room.message".into(),content:json!({"msgtype":"m.notice","body":format!("Could not bridge an attachment: {}",error.downcast_ref::<Rejected>().unwrap())})});
                    continue;
                }
                Err(error) => return Err(error),
            };
            let uri = if let Some(uri) = self.store.cached_media(binding, id)? {
                uri
            } else {
                let uri = self
                    .matrix
                    .upload_media(
                        &binding.ghost,
                        &media.name,
                        &media.mime,
                        media.bytes.clone(),
                    )
                    .await?;
                self.store.cache_media(binding, id, &uri)?;
                uri
            };
            parts.push(Part {key:format!("artifact:{id}"),kind:"m.room.message".into(),content:json!({"msgtype":media::msgtype(&media.mime),"body":media.name,"filename":media.name,"url":uri,"info":{"mimetype":media.mime,"size":media.bytes.len()}})});
        }
        if let Some(parent) = message.reply_to_message_id.as_deref() {
            let target = self.reply_target(binding, parent).await?;
            for part in &mut parts {
                part.content["m.relates_to"] = json!({"m.in_reply_to":{"event_id":target}});
            }
        }
        // @implements url:https://spec.matrix.org/v1.16/client-server-api/#threading -- preserve the originating Beeper thread across the agent run
        if message.role == "bot"
            && let Some(run) = message.run_id.as_deref()
            && let Some((parent, thread)) = self.store.run_route(binding, run)?
        {
            for part in &mut parts {
                if let Some(root) = thread.as_deref() {
                    let reply = part.content["m.relates_to"]["m.in_reply_to"]["event_id"]
                        .as_str()
                        .unwrap_or(&parent)
                        .to_string();
                    part.content["m.relates_to"] = json!({"rel_type":"m.thread","event_id":root,"is_falling_back":true,"m.in_reply_to":{"event_id":reply}});
                } else if part.content["m.relates_to"].is_null() {
                    part.content["m.relates_to"] = json!({"m.in_reply_to":{"event_id":parent}});
                }
            }
        }
        Ok(parts)
    }

    async fn reply_target(&self, binding: &Binding, id: &str) -> Result<String> {
        if let Some(link) = self.store.links(binding, id)?.into_iter().next() {
            return Ok(link.matrix_id);
        }
        let parent = self.source_message(binding, id).await?;
        if parent.role == "user" {
            let original = if let Some(nonce) = parent.client_nonce.as_deref() {
                self.store.event_for_nonce(binding, nonce)?
            } else {
                self.store.event_for_seq(binding, parent.seq)?
            };
            if let Some(original) = original {
                return Ok(original);
            }
        }
        let baseline = self
            .store
            .baseline(&binding.tenant, &binding.bot, binding.cursor)?;
        let key = if parent.text().is_empty() {
            let artifact = parent
                .blocks
                .iter()
                .find_map(|block| block["artifactId"].as_str())
                .context("reply target has no bridgeable content")?;
            format!("artifact:{artifact}")
        } else {
            "text:0".into()
        };
        Ok(self.source_event_id(binding, &parent, &key, baseline))
    }

    async fn prepare_message(&self, binding: &Binding, message: &Message) -> Result<Prepared> {
        if !matches!(message.role.as_str(), "user" | "bot") {
            return Ok(Prepared::default());
        }
        if message.role == "user" {
            let event = if let Some(nonce) = message.client_nonce.as_deref() {
                self.store.event_for_nonce(binding, nonce)?
            } else {
                self.store.event_for_seq(binding, message.seq)?
            };
            if let Some(event) = event {
                let raw = self
                    .matrix
                    .event(&binding.room, &binding.ghost, &event)
                    .await?;
                let kind = raw["type"].as_str().unwrap_or("m.room.message").to_string();
                self.store.link(
                    binding,
                    &MessageLink {
                        rakazo_id: message.id.clone(),
                        matrix_id: event,
                        part: if kind == "m.reaction" {
                            "reaction"
                        } else {
                            "text:0"
                        }
                        .into(),
                        role: "user".into(),
                        kind,
                        hash: hash(&raw["content"]),
                    },
                )?;
                return Ok(Prepared::default());
            }
        }
        let parts = self.message_parts(binding, message).await?;
        let baseline = self
            .store
            .baseline(&binding.tenant, &binding.bot, binding.cursor)?;
        let existing = self.store.links(binding, &message.id)?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(
            message
                .created_at
                .as_deref()
                .context("message missing createdAt")?,
        )?
        .timestamp_millis();
        ensure!(timestamp > 0, "message has invalid timestamp");
        let sender = if message.role == "user" {
            &self.config.owner
        } else {
            &binding.ghost
        };
        let mut events = Vec::new();
        let mut links = Vec::new();
        for part in parts {
            if existing.iter().any(|link| link.part == part.key) {
                continue;
            }
            let id = self.source_event_id(binding, message, &part.key, baseline);
            links.push(MessageLink {
                rakazo_id: message.id.clone(),
                matrix_id: id.clone(),
                part: part.key,
                role: message.role.clone(),
                kind: part.kind.clone(),
                hash: hash(&part.content),
            });
            events.push(json!({"event_id":id,"room_id":binding.room,"sender":sender,"type":part.kind,"origin_server_ts":timestamp,"content":part.content}));
        }
        Ok(Prepared { events, links })
    }

    async fn send_prepared(
        &self,
        binding: &Binding,
        prepared: Prepared,
        forward: bool,
    ) -> Result<()> {
        if prepared.events.is_empty() {
            return Ok(());
        }
        self.matrix
            .check_room(&binding.room, &binding.ghost, &self.config.owner)
            .await?;
        let ids = prepared
            .links
            .iter()
            .map(|link| link.matrix_id.clone())
            .collect::<Vec<_>>();
        self.store.reserve_history(&binding.tenant, &ids)?;
        self.matrix
            .batch_send(&binding.room, prepared.events, forward)
            .await?;
        for link in prepared.links {
            self.store.link(binding, &link)?;
        }
        Ok(())
    }

    pub async fn relay_message(
        &self,
        binding: &Binding,
        message: &Message,
        forward: bool,
    ) -> Result<()> {
        let prepared = self.prepare_message(binding, message).await?;
        self.send_prepared(binding, prepared, forward).await
    }

    /// A backwards batch prepends its entire ordered page. Sending each message
    /// separately would reverse the transcript even with correct timestamps.
    pub async fn relay_page(&self, binding: &Binding, messages: &[Message]) -> Result<()> {
        let mut page = Prepared::default();
        for message in messages {
            let prepared = self.prepare_message(binding, message).await?;
            page.events.extend(prepared.events);
            page.links.extend(prepared.links);
        }
        self.send_prepared(binding, page, false).await
    }
}
