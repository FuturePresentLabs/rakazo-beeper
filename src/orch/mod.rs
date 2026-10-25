//! Fleet orchestrator entries in Rakazo.
//!
//! The orchestrator and each vertical PM become first-class Rakazo agents so the
//! human can see and talk to them in the normal Rakazo UI. This module only
//! speaks the documented RPC surface; it creates no rooms and starts no runs
//! unless `send` is used explicitly.
//!
//! Design and gaps: `docs/orchestrator-in-rakazo.md`.

pub mod reply;
pub mod watch;

use crate::rakazo::{Bot, NewBot, Rakazo};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use url::Url;

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

/// How an entry is matched to an agent and how it may be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Created with `bots.create`; our own `spawnKey` is the idempotency key.
    Root,
    /// A real sub-chat under the orchestrator. Only the orchestrator's own
    /// `spawn_bot` tool can set `parentBotId`; no client RPC exposes it.
    Child,
}

/// One local pane that should appear as a Rakazo conversation.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// Stable local name. For roots this is the server-side idempotency key.
    pub key: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub kind: Kind,
}

const ORCH: Entry = Entry {
    key: "fleet-orch-v1",
    name: "Fleet Orchestrator",
    title: "Local fleet orchestrator (pi)",
    description: "Runs on the operator's machine. Triages escalations, dispatches fleet workers, and reports merged work.",
    kind: Kind::Root,
};

const PMS: [Entry; 5] = [
    Entry {
        key: "fleet-pm-agentsmith-v1",
        name: "PM · Agentsmith",
        title: "Agentsmith vertical PM",
        description: "Single-threaded PM for the agentsmith vertical: fleet tooling, bus, IRC and reaper glue.",
        kind: Kind::Child,
    },
    // Transmog is paused, so it deliberately has no live entry. Re-add its
    // `fleet-pm-transmog-v1` entry when the vertical is resumed.
    Entry {
        key: "fleet-pm-edgerunner-v1",
        name: "PM · Edgerunner",
        title: "Edgerunner vertical PM",
        description: "Single-threaded PM for the edgerunner vertical (native agent sessions, ACP, sandboxes).",
        kind: Kind::Child,
    },
    Entry {
        key: "fleet-pm-fpl-v1",
        name: "PM · FPL",
        title: "FPL core vertical PM",
        description: "Single-threaded PM for FPL core repos and the Rakazo bridge itself.",
        kind: Kind::Child,
    },
    Entry {
        key: "fleet-pm-fpl-cloud-v1",
        name: "PM · FPL Cloud",
        title: "FPL cloud vertical PM",
        description: "Single-threaded PM for FPL cloud, shroud and hosted deployments.",
        kind: Kind::Child,
    },
    Entry {
        key: "fleet-pm-marbles-v1",
        name: "PM · Marbles",
        title: "Marbles vertical PM",
        description: "Single-threaded PM for the marbles issue tracker and its workers.",
        kind: Kind::Child,
    },
];

pub fn roster() -> Vec<Entry> {
    let mut entries = Vec::with_capacity(PMS.len() + 1);
    entries.push(ORCH);
    entries.extend(PMS);
    entries
}

pub struct Orch {
    pub rakazo: Rakazo,
    pub space: String,
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
        Ok(Self {
            rakazo: Rakazo::from_parts(base, &token, &space)?,
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
        match entry.kind {
            Kind::Root => bots.iter().find(|bot| {
                bot.archived_at.is_none() && bot.spawn_key.as_deref() == Some(entry.key)
            }),
            Kind::Child => {
                let root = bots.iter().find(|bot| {
                    bot.archived_at.is_none() && bot.spawn_key.as_deref() == Some(ORCH.key)
                })?;
                bots.iter().find(|bot| {
                    bot.archived_at.is_none()
                        && bot.parent_bot_id.as_deref() == Some(root.id.as_str())
                        && bot.name == entry.name
                })
            }
        }
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
                name: entry.name,
                title: entry.title,
                description: entry.description,
                instructions: entry.description,
                notify_on_finish: false,
                spawn_key: entry.key,
                // A fleet entry must never take over the deployment's shared computer.
                computer_mode: "team",
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
    /// text reconciles, reusing it with different text must not happen.
    pub async fn say(&self, bot: &str, text: &str, nonce: &str) -> Result<i64> {
        ensure!(!text.trim().is_empty(), "refusing to send an empty message");
        let receipt = self.rakazo.send_routed(bot, text, &[], None, nonce).await?;
        Ok(receipt.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_is_unique_and_has_one_parent() {
        let entries = roster();
        assert_eq!(entries.iter().filter(|e| e.kind == Kind::Root).count(), 1);
        let mut keys: Vec<_> = entries.iter().map(|e| e.key).collect();
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
        let bot = |id: &str, spawn: Option<&str>, archived: Option<&str>| Bot {
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
        };
        let orch = Orch {
            rakazo: Rakazo::from_parts(
                Url::parse("https://agents.example.test").unwrap(),
                "token",
                "space",
            )
            .unwrap(),
            space: "space".into(),
        };
        let mut bots = vec![
            bot("a", Some("other-v1"), None),
            bot("b", Some(ORCH.key), Some("2026-01-01T00:00:00Z")),
            bot("c", Some(ORCH.key), None),
        ];
        assert_eq!(orch.resolve(&bots, &ORCH).map(|b| b.id.as_str()), Some("c"));
        // A PM resolves only as an exact-name child of the live orchestrator.
        let mut child = bot("d", Some("run:spawn_bot:hash"), None);
        let marbles = roster()
            .into_iter()
            .find(|entry| entry.key == "fleet-pm-marbles-v1")
            .unwrap();
        child.name = marbles.name.to_string();
        child.parent_bot_id = Some("c".into());
        bots.push(child);
        assert_eq!(
            orch.resolve(&bots, &marbles).map(|b| b.id.as_str()),
            Some("d")
        );
        assert!(
            orch.resolve(&bots, &roster()[1]).is_none(),
            "a child must belong to the live orchestrator"
        );
    }
}
