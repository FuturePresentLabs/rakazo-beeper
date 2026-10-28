//! Fleet orchestrator entries in Rakazo.
//!
//! The orchestrator and each vertical PM become first-class Rakazo agents so the
//! human can see and talk to them in the normal Rakazo UI. This module only
//! speaks the documented RPC surface; it creates no rooms and starts no runs
//! unless `send` is used explicitly.
//!
//! Design and gaps: `docs/orchestrator-in-rakazo.md`.

pub mod agentsmith;
pub mod bridge;
pub mod reply;
pub mod roster;
pub mod supervisor;
pub mod watch;

use crate::fleet::{BotKey, Entry as RouteEntry, Table};
use crate::rakazo::{Bot, NewBot, Rakazo, SendReceipt};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use url::Url;

pub use roster::{Entry, Kind, Live, ORCH_KEY, ORCH_NAME, orchestrator_entry};

/// The single production conversation this process may read and reply to.
///
/// Every value is explicit and non-empty; there is no wildcard, no fallback to
/// "whatever the session can see", and no cross-space read. `from_env` is the
/// only production constructor so the binding is visible in the unit/config
/// diff rather than inferred from the live bot list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowlist {
    pub bot_id: String,
    pub space_id: String,
    pub thread_id: String,
}

impl Allowlist {
    /// `RAKAZO_ORCH_BOT_ID`, `RAKAZO_ORCH_SPACE_ID`, `RAKAZO_ORCH_THREAD_ID`.
    pub fn from_env() -> Result<Self> {
        let read = |name: &str| -> Result<String> {
            let value = std::env::var(name).with_context(|| format!("set {name}"))?;
            let value = value.trim().to_string();
            ensure!(!value.is_empty(), "empty {name}");
            Ok(value)
        };
        Ok(Self {
            bot_id: read("RAKAZO_ORCH_BOT_ID")?,
            space_id: read("RAKAZO_ORCH_SPACE_ID")?,
            thread_id: read("RAKAZO_ORCH_THREAD_ID")?,
        })
    }

    /// Fail closed unless `bot` is exactly the configured conversation.
    pub fn check(&self, bot: &Bot) -> Result<()> {
        ensure!(
            bot.id == self.bot_id,
            "refusing bot {}: allowlist is {}",
            bot.id,
            self.bot_id
        );
        ensure!(
            bot.archived_at.is_none(),
            "refusing archived bot {}",
            self.bot_id
        );
        ensure!(
            !bot.space_id.is_empty() && bot.space_id == self.space_id,
            "refusing bot {} in space {}: allowlist space is {}",
            self.bot_id,
            if bot.space_id.is_empty() {
                "<unknown>"
            } else {
                &bot.space_id
            },
            self.space_id
        );
        let actual = bot.thread_id.as_deref().unwrap_or(&bot.id);
        ensure!(
            actual == self.thread_id,
            "refusing thread {}: allowlist thread is {}",
            actual,
            self.thread_id
        );
        Ok(())
    }
}

/// The production roster: orchestrator plus every vertical in `verticals.toml`,
/// joined with live `pi-pm` state. A missing verticals file is an error (we must
/// not silently show an empty fleet); missing live state only marks entries
/// offline.
pub fn roster() -> Result<Vec<Entry>> {
    roster::load()
}

pub struct Orch {
    pub rakazo: Rakazo,
    /// Bot-bound service principal for assistant-append replies, when configured.
    pub assistant: Option<Rakazo>,
    pub space: String,
}

/// Fleet route key for a roster entry. The orchestrator is `orch`; every PM is
/// `pm:<vertical>`, derived from the same stable key shown in `orch list`.
pub fn route_key(entry: &Entry) -> Result<BotKey> {
    match entry.kind {
        Kind::Root => Ok(BotKey::Orch),
        Kind::Child => entry
            .vertical()
            .map(BotKey::pm)
            .context("PM roster key is not of the form fleet-pm-<vertical>-v1"),
    }
}

/// Match live agents to roster entries without needing a Rakazo session. Roots
/// match on the create idempotency key; children match by exact name under the
/// live orchestrator's `parentBotId`.
pub fn resolve<'a>(bots: &'a [Bot], entry: &Entry) -> Option<&'a Bot> {
    match entry.kind {
        Kind::Root => bots.iter().find(|bot| {
            bot.archived_at.is_none() && bot.spawn_key.as_deref() == Some(entry.key.as_str())
        }),
        Kind::Child => {
            let root = bots.iter().find(|bot| {
                bot.archived_at.is_none() && bot.spawn_key.as_deref() == Some(ORCH_KEY)
            })?;
            bots.iter().find(|bot| {
                bot.archived_at.is_none()
                    && bot.parent_bot_id.as_deref() == Some(root.id.as_str())
                    && bot.name == entry.name
            })
        }
    }
}

impl Orch {
    /// `RAKAZO_ORCH_TOKEN[_FILE]`, `RAKAZO_ORCH_SPACE_ID`, optional `RAKAZO_ORCH_URL`.
    /// These are the only credentials required; nothing is read from the bridge config.
    pub fn from_env() -> Result<Self> {
        let token = crate::config::secret("RAKAZO_ORCH_TOKEN")
            .context("Rakazo session missing: set RAKAZO_ORCH_TOKEN or RAKAZO_ORCH_TOKEN_FILE")?;
        let space = std::env::var("RAKAZO_ORCH_SPACE_ID")
            .context("set RAKAZO_ORCH_SPACE_ID to the Rakazo space that owns the fleet entries")?;
        ensure!(!space.trim().is_empty(), "empty RAKAZO_ORCH_SPACE_ID");
        let base = Url::parse(
            &std::env::var("RAKAZO_ORCH_URL")
                .unwrap_or_else(|_| "https://agents.fpl.dev".to_string()),
        )
        .context("invalid RAKAZO_ORCH_URL")?;
        crate::config::validate_url(&base)?;
        let assistant = crate::config::secret("RAKAZO_ORCH_SERVICE_TOKEN")
            .ok()
            .map(|token| Rakazo::from_service_parts(base.clone(), &token, &space))
            .transpose()?;
        Ok(Self {
            rakazo: Rakazo::from_parts(base, &token, &space)?,
            assistant,
            space,
        })
    }

    /// Read-only: confirm the session and the space it resolves to.
    pub async fn whoami(&self) -> Result<(String, String)> {
        let me: Value = self.rakazo.rpc(&["me"], Value::Null).await?;
        let user = me["email"].as_str().unwrap_or_default().to_string();
        let space = me["spaceId"].as_str().unwrap_or_default().to_string();
        Ok((user, space))
    }

    pub async fn bots(&self) -> Result<Vec<Bot>> {
        self.rakazo.bots().await
    }

    /// Match live agents to roster entries.
    ///
    /// Roots are matched by their create idempotency key. Children cannot carry
    /// our `spawnKey` (only the orchestrator's `spawn_bot` tool creates them), so
    /// they are matched by name under the live orchestrator's `parentBotId`.
    pub fn resolve<'a>(&self, bots: &'a [Bot], entry: &Entry) -> Option<&'a Bot> {
        resolve(bots, entry)
    }

    /// Build the routing table from the *same* derived roster.
    ///
    /// A vertical that is paused, offline, or has no pane becomes an explicit
    /// [`fleet::Resolution::Unrouted`] row; it never falls back to the
    /// orchestrator. Unknown conversations are unrouted by [`Table::resolve`].
    pub fn route_table(&self, bots: &[Bot], roster: &[Entry]) -> Result<Table> {
        let mut entries = Vec::with_capacity(roster.len());
        for entry in roster.iter().filter(|entry| !entry.paused) {
            let bot = self.resolve(bots, entry);
            let conversation =
                bot.map(|bot| bot.thread_id.clone().unwrap_or_else(|| bot.id.clone()));
            let notify = match entry.kind {
                Kind::Root => "orchestrator".to_string(),
                Kind::Child => format!("pm:{}", entry.vertical().unwrap_or(&entry.key)),
            };
            let mut row = RouteEntry::new(
                route_key(entry)?,
                conversation,
                entry.pane().map(str::to_string),
                Some(notify),
            )
            .with_paused(entry.paused);
            if entry.kind == Kind::Child {
                row = row.with_parent(BotKey::Orch);
            }
            let row = match bot.map(|bot| bot.id.clone()) {
                Some(bot_id) => row.with_bot_id(bot_id),
                None => row,
            };
            entries.push(row);
        }
        Table::new(entries).map_err(Into::into)
    }

    /// Text that asks the orchestrator to nest one PM sub-chat.
    ///
    /// `parentBotId` is not writable through any client RPC: the orchestrator's
    /// own `spawn_bot` tool is the only thing that can set it. We therefore send
    /// an instruction into the orchestrator's conversation instead of creating
    /// the child ourselves.
    pub fn spawn_prompt(entry: &Entry) -> String {
        format!(
            "Spawn one persistent sub-chat for the {title} vertical using your spawn_bot tool.\n\nname: {name}\ntitle: {title}\ninstructions: {description}\n\nUse exactly that name so `rakazo-beeper orch list` resolves it, and reply with the new bot id.",
            title = entry.title,
            name = entry.name,
            description = entry.description,
        )
    }

    /// Create a root agent. Children must be spawned by the orchestrator.
    pub async fn create(&self, entry: &Entry) -> Result<Bot> {
        ensure!(
            entry.kind == Kind::Root,
            "{} is a sub-chat: only the orchestrator's spawn_bot tool can nest it",
            entry.name
        );
        self.rakazo
            .create_bot(&NewBot {
                name: &entry.name,
                title: &entry.title,
                description: &entry.description,
                instructions: &entry.description,
                notify_on_finish: false,
                spawn_key: &entry.key,
                // A fleet entry must never take over the deployment's shared computer.
                computer_mode: "team",
                // Relay-only: append assistant messages, never run a model.
                relay_only: true,
            })
            .await
    }

    /// Human text typed in Rakazo for one agent, newest last.
    pub async fn recent(&self, bot: &str, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let page = self
            .rakazo
            .rpc::<crate::rakazo::Page>(&["threads", "messages"], serde_json::json!({"botId": bot}))
            .await?;
        let mut rows: Vec<_> = page
            .messages
            .into_iter()
            .map(|message| (message.seq, message.role.clone(), message.text()))
            .collect();
        rows.sort_by_key(|(seq, _, _)| *seq);
        let keep = rows.len().saturating_sub(limit);
        Ok(rows.split_off(keep))
    }

    /// Post one message into an agent's Rakazo conversation.
    ///
    /// `nonce` is the caller's durable idempotency key: reusing it with the same
    /// text reconciles, reusing it with different text must not happen. A
    /// relay-only conversation returns a receipt with no run id.
    pub async fn say(&self, bot: &str, text: &str, nonce: &str) -> Result<SendReceipt> {
        ensure!(!text.trim().is_empty(), "refusing to send an empty message");
        self.rakazo.send_routed(bot, text, &[], None, nonce).await
    }

    /// Append a local orchestrator reply as a bot message. Requires the
    /// bot-bound service token; a session token is never used for this path.
    pub async fn reply_assistant(
        &self,
        bot: &str,
        text: &str,
        nonce: &str,
        reply_to: Option<&str>,
    ) -> Result<crate::rakazo::AppendReceipt> {
        let assistant = self
            .assistant
            .as_ref()
            .context("set RAKAZO_ORCH_SERVICE_TOKEN[_FILE] for assistant append")?;
        ensure!(!text.trim().is_empty(), "refusing to send an empty reply");
        assistant.append_assistant(bot, text, nonce, reply_to).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::Resolution;
    use crate::orch::roster::Vertical;
    use std::collections::BTreeMap;

    fn vertical(name: &str, paused: bool) -> Vertical {
        Vertical {
            name: name.to_string(),
            owner: Some(name.to_string()),
            scope: None,
            paused,
        }
    }

    fn bot(id: &str, spawn: Option<&str>, archived: Option<&str>) -> Bot {
        Bot {
            id: id.into(),
            name: "x".into(),
            color: String::new(),
            title: String::new(),
            description: String::new(),
            space_id: "space".into(),
            thread_id: None,
            parent_bot_id: None,
            spawn_key: spawn.map(str::to_string),
            archived_at: archived.map(str::to_string),
            capabilities: None,
            relay_only: None,
        }
    }

    fn orch() -> Orch {
        Orch {
            rakazo: Rakazo::from_parts(
                Url::parse("https://agents.example.test").unwrap(),
                "token",
                "space",
            )
            .unwrap(),
            assistant: None,
            space: "space".into(),
        }
    }

    #[test]
    fn roster_is_unique_and_has_one_root() {
        let entries = roster::from_parts(
            &[
                vertical("agentsmith", false),
                vertical("fpl", false),
                vertical("fpl-cloud", false),
                vertical("marbles", false),
            ],
            &BTreeMap::new(),
        );
        assert_eq!(entries.iter().filter(|e| e.kind == Kind::Root).count(), 1);
        let mut keys: Vec<_> = entries.iter().map(|e| e.key.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), entries.len());
        assert!(
            entries
                .iter()
                .all(|e| !e.name.is_empty() && e.key.contains("-v1"))
        );
    }

    #[test]
    fn resolution_ignores_archived_and_foreign_agents() {
        let orch = orch();
        let mut bots = vec![
            bot("a", Some("other-v1"), None),
            bot("b", Some(ORCH_KEY), Some("2026-01-01T00:00:00Z")),
            bot("c", Some(ORCH_KEY), None),
        ];
        let entries = roster::from_parts(
            &[vertical("marbles", false), vertical("fpl", false)],
            &BTreeMap::new(),
        );
        let root = &entries[0];
        let marbles = entries
            .iter()
            .find(|entry| entry.key == "fleet-pm-marbles-v1")
            .unwrap();
        let fpl = entries
            .iter()
            .find(|entry| entry.key == "fleet-pm-fpl-v1")
            .unwrap();
        assert_eq!(orch.resolve(&bots, root).map(|b| b.id.as_str()), Some("c"));
        // A PM resolves only as an exact-name child of the live orchestrator.
        let mut child = bot("d", Some("run:spawn_bot:hash"), None);
        child.name = marbles.name.clone();
        child.parent_bot_id = Some("c".into());
        bots.push(child);
        assert_eq!(
            orch.resolve(&bots, marbles).map(|b| b.id.as_str()),
            Some("d")
        );
        assert!(
            orch.resolve(&bots, fpl).is_none(),
            "a child must belong to the live orchestrator"
        );
    }

    #[test]
    fn route_table_uses_derived_entries_and_never_falls_back_to_orch() {
        let orch = orch();
        let entries = roster::from_parts(
            &[
                vertical("fpl", false),
                vertical("transmog", true),
                vertical("marbles", false),
            ],
            &roster::parse_live_tsv("fpl\tpm-fpl\t%40\tlive:node\treview\t1\t-\t0\n"),
        );
        let mut bots = vec![bot("orch-bot", Some(ORCH_KEY), None)];
        for (id, key) in [
            ("fpl-bot", "fleet-pm-fpl-v1"),
            ("marbles-bot", "fleet-pm-marbles-v1"),
        ] {
            let entry = entries.iter().find(|entry| entry.key == key).unwrap();
            let mut child = bot(id, Some("run:spawn_bot:hash"), None);
            child.name = entry.name.clone();
            child.parent_bot_id = Some("orch-bot".into());
            bots.push(child);
        }
        let table = orch.route_table(&bots, &entries).expect("valid table");

        // Live PM routes to its own pane.
        assert_eq!(
            table.resolve("fpl-bot"),
            Resolution::Routed {
                bot: BotKey::pm("fpl"),
                pane: "%40".to_string(),
            }
        );
        // Offline PM is present but unrouted; no orchestrator fallback.
        match table.resolve("marbles-bot") {
            Resolution::Unrouted { reason } => assert!(reason.contains("no pane"), "{reason}"),
            other => panic!("expected Unrouted, got {other:?}"),
        }
        // Paused verticals are not inserted into the routing table at all.
        assert!(!table.contains_key(&BotKey::pm("transmog")));
        match table.resolve("transmog-bot") {
            Resolution::Unrouted { reason } => assert!(reason.contains("unknown"), "{reason}"),
            other => panic!("expected Unrouted, got {other:?}"),
        }
        // Unknown conversations never resolve to the orchestrator.
        match table.resolve("not-a-conversation") {
            Resolution::Unrouted { reason } => assert!(!reason.contains("orch"), "{reason}"),
            other => panic!("expected Unrouted, got {other:?}"),
        }
        // PM rows stay parented under orch.
        for entry in table
            .entries()
            .iter()
            .filter(|e| !matches!(e.bot_key(), BotKey::Orch))
        {
            assert_eq!(entry.parent(), Some(&BotKey::Orch), "{}", entry.bot_key());
        }
    }

    #[test]
    fn route_keys_are_derived_from_roster_keys() {
        let entries = roster::from_parts(
            &[vertical("fpl-cloud", false), vertical("agentsmith", false)],
            &BTreeMap::new(),
        );
        assert_eq!(route_key(&entries[0]).unwrap(), BotKey::Orch);
        assert_eq!(route_key(&entries[1]).unwrap(), BotKey::pm("agentsmith"));
        assert_eq!(route_key(&entries[2]).unwrap(), BotKey::pm("fpl-cloud"));
        assert_eq!(entries[1].vertical(), Some("agentsmith"));
    }
}
