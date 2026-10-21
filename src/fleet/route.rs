//! Fleet identity routing table.
//!
//! This is the local half of the three-tier identity model: a stable bot key
//! (`orch`, `pm:<vertical>`, `worker:<slug>`, or a T3 node name) maps to one
//! Rakazo conversation and one local pane. Panes are inherently transient, so
//! the table is a cache refreshed from live state; nothing here hardcodes a
//! `%NN` pane id.
//!
//! Resolution never silently falls back to the orchestrator. An unknown or
//! paused conversation returns [`Resolution::Unrouted`] and the caller decides
//! whether to surface it or queue it.

use std::collections::HashMap;
use std::fmt;

/// A vertical name, e.g. `fpl` in the stable key `pm:fpl`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Vertical(pub String);

impl Vertical {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Vertical {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The stable identity of one participant in the fleet.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BotKey {
    /// The single T1 control-plane bot.
    Orch,
    /// A vertical PM, parented under [`BotKey::Orch`].
    Pm(Vertical),
    /// An ephemeral T2 worker. It has no Rakazo identity of its own.
    Worker(String),
    /// An opt-in T3 persistent worker node.
    Node(String),
}

impl BotKey {
    pub fn orch() -> Self {
        Self::Orch
    }

    pub fn pm(name: impl Into<String>) -> Self {
        Self::Pm(Vertical::new(name))
    }

    pub fn worker(slug: impl Into<String>) -> Self {
        Self::Worker(slug.into())
    }

    pub fn node(name: impl Into<String>) -> Self {
        Self::Node(name.into())
    }

    /// Parse one stable string key. `orch`, `pm:<v>` and `worker:<slug>` are
    /// reserved forms; every other non-empty key is a T3 node name.
    pub fn parse(key: &str) -> Option<Self> {
        if key.is_empty() {
            return None;
        }
        if key == "orch" {
            return Some(Self::Orch);
        }
        if let Some(vertical) = key.strip_prefix("pm:") {
            return (!vertical.is_empty()).then(|| Self::pm(vertical));
        }
        if let Some(slug) = key.strip_prefix("worker:") {
            return (!slug.is_empty()).then(|| Self::worker(slug));
        }
        Some(Self::node(key))
    }

    /// The exact stable string used in the mapping table and A2A messages.
    pub fn as_key(&self) -> String {
        match self {
            Self::Orch => "orch".to_string(),
            Self::Pm(vertical) => format!("pm:{vertical}"),
            Self::Worker(slug) => format!("worker:{slug}"),
            Self::Node(name) => name.clone(),
        }
    }

    pub fn is_worker(&self) -> bool {
        matches!(self, Self::Worker(_))
    }
}

impl fmt::Display for BotKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_key())
    }
}

/// One explicit row of the mapping table.
#[derive(Clone, Debug)]
pub struct Entry {
    bot_key: BotKey,
    bot_id: Option<String>,
    conversation_id: Option<String>,
    pane_id: Option<String>,
    notify_target: Option<String>,
    parent: Option<BotKey>,
    paused: bool,
}

impl Entry {
    /// A routing row. Pass `None` for a worker's missing conversation, pane
    /// and bot id; use [`Entry::routed`] for the common live case.
    pub fn new(
        bot_key: BotKey,
        conversation_id: Option<String>,
        pane_id: Option<String>,
        notify_target: Option<String>,
    ) -> Self {
        Self {
            bot_key,
            bot_id: None,
            conversation_id,
            pane_id,
            notify_target,
            parent: None,
            paused: false,
        }
    }

    /// Convenience constructor for a live bot with a conversation and pane.
    pub fn routed(
        bot_key: BotKey,
        conversation_id: impl Into<String>,
        pane_id: impl Into<String>,
        notify_target: impl Into<String>,
    ) -> Self {
        Self::new(
            bot_key,
            Some(conversation_id.into()),
            Some(pane_id.into()),
            Some(notify_target.into()),
        )
    }

    /// A T2 worker row: no conversation, no pane and, by validation, no bot id.
    pub fn worker(slug: impl Into<String>, notify_target: impl Into<String>) -> Self {
        Self::new(BotKey::worker(slug), None, None, Some(notify_target.into()))
    }

    pub fn with_bot_id(mut self, bot_id: impl Into<String>) -> Self {
        self.bot_id = Some(bot_id.into());
        self
    }

    pub fn with_parent(mut self, parent: BotKey) -> Self {
        self.parent = Some(parent);
        self
    }

    pub fn with_paused(mut self, paused: bool) -> Self {
        self.paused = paused;
        self
    }

    pub fn bot_key(&self) -> &BotKey {
        &self.bot_key
    }

    pub fn bot_id(&self) -> Option<&str> {
        self.bot_id.as_deref()
    }

    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.as_deref()
    }

    pub fn pane_id(&self) -> Option<&str> {
        self.pane_id.as_deref()
    }

    pub fn notify_target(&self) -> Option<&str> {
        self.notify_target.as_deref()
    }

    pub fn parent(&self) -> Option<&BotKey> {
        self.parent.as_ref()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    fn validate(&self) -> Result<(), TableError> {
        match &self.bot_key {
            BotKey::Orch => {}
            BotKey::Pm(vertical) if vertical.as_str().is_empty() => {
                return Err(TableError::InvalidKey(self.bot_key.clone()));
            }
            BotKey::Pm(_) => {
                if self.parent.as_ref() != Some(&BotKey::Orch) {
                    return Err(TableError::PmWithoutOrchParent(self.bot_key.clone()));
                }
            }
            BotKey::Worker(slug) => {
                if slug.is_empty() {
                    return Err(TableError::InvalidKey(self.bot_key.clone()));
                }
                if self.conversation_id.is_some() {
                    return Err(TableError::WorkerHasConversation(self.bot_key.clone()));
                }
                if self.bot_id.is_some() {
                    return Err(TableError::WorkerHasBotId(self.bot_key.clone()));
                }
            }
            BotKey::Node(name) if name.is_empty() => {
                return Err(TableError::InvalidKey(self.bot_key.clone()));
            }
            BotKey::Node(_) => {}
        }
        Ok(())
    }
}

/// The result of an inbound conversation lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Routed { bot: BotKey, pane: String },
    Unrouted { reason: String },
}

impl Resolution {
    pub fn routed_bot(&self) -> Option<&BotKey> {
        match self {
            Self::Routed { bot, .. } => Some(bot),
            Self::Unrouted { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableError {
    InvalidKey(BotKey),
    WorkerHasConversation(BotKey),
    WorkerHasBotId(BotKey),
    PmWithoutOrchParent(BotKey),
    DuplicateKey(BotKey),
    DuplicateConversation(String),
    DuplicatePane(String),
    UnknownKey(BotKey),
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey(key) => write!(f, "invalid bot key: {key}"),
            Self::WorkerHasConversation(key) => {
                write!(f, "worker {key} must not have a conversation")
            }
            Self::WorkerHasBotId(key) => write!(f, "worker {key} must not have a bot id"),
            Self::PmWithoutOrchParent(key) => {
                write!(f, "pm {key} must be a child of orch")
            }
            Self::DuplicateKey(key) => write!(f, "duplicate bot key: {key}"),
            Self::DuplicateConversation(id) => write!(f, "duplicate conversation: {id}"),
            Self::DuplicatePane(id) => write!(f, "duplicate pane: {id}"),
            Self::UnknownKey(key) => write!(f, "unknown bot key: {key}"),
        }
    }
}

impl std::error::Error for TableError {}

/// Explicit bot-key -> conversation -> pane -> notify mappings.
#[derive(Clone, Debug, Default)]
pub struct Table {
    entries: Vec<Entry>,
    by_key: HashMap<BotKey, usize>,
    by_conversation: HashMap<String, usize>,
    by_pane: HashMap<String, usize>,
}

impl Table {
    /// Build a table, enforcing all structural rules. Duplicate keys,
    /// conversations and panes are rejected rather than silently overwritten.
    pub fn new(entries: Vec<Entry>) -> Result<Self, TableError> {
        let mut table = Self::default();
        for entry in entries {
            table.insert(entry)?;
        }
        Ok(table)
    }

    pub fn insert(&mut self, entry: Entry) -> Result<(), TableError> {
        entry.validate()?;

        if self.by_key.contains_key(entry.bot_key()) {
            return Err(TableError::DuplicateKey(entry.bot_key().clone()));
        }
        if let Some(conversation) = entry.conversation_id()
            && self.by_conversation.contains_key(conversation)
        {
            return Err(TableError::DuplicateConversation(conversation.to_string()));
        }
        if let Some(pane) = entry.pane_id()
            && self.by_pane.contains_key(pane)
        {
            return Err(TableError::DuplicatePane(pane.to_string()));
        }

        let index = self.entries.len();
        if let Some(conversation) = entry.conversation_id() {
            self.by_conversation.insert(conversation.to_string(), index);
        }
        if let Some(pane) = entry.pane_id() {
            self.by_pane.insert(pane.to_string(), index);
        }
        self.by_key.insert(entry.bot_key().clone(), index);
        self.entries.push(entry);
        Ok(())
    }

    /// Conversation -> pane. An unknown, paused or pane-less key is explicitly
    /// [`Resolution::Unrouted`]; there is deliberately no orch fallback here.
    pub fn resolve(&self, conversation_id: &str) -> Resolution {
        let Some(&index) = self.by_conversation.get(conversation_id) else {
            return Resolution::Unrouted {
                reason: format!("unknown conversation: {conversation_id}"),
            };
        };
        let entry = &self.entries[index];
        let bot = entry.bot_key().clone();
        if entry.is_paused() {
            return Resolution::Unrouted {
                reason: format!("paused: {bot}"),
            };
        }
        let Some(pane) = entry.pane_id() else {
            return Resolution::Unrouted {
                reason: format!("no pane for: {bot}"),
            };
        };
        Resolution::Routed {
            bot,
            pane: pane.to_string(),
        }
    }

    /// Pane -> bot key. Exact match only: recycled pane ids must not be
    /// resolved by prefix or by "nearest window".
    pub fn target(&self, pane_id: &str) -> Option<BotKey> {
        let &index = self.by_pane.get(pane_id)?;
        let entry = &self.entries[index];
        (!entry.is_paused()).then(|| entry.bot_key().clone())
    }

    /// Durable refresh path: a pane was re-resolved from live tmux state.
    pub fn with_pane_updated(
        &mut self,
        key: &BotKey,
        pane_id: impl Into<String>,
    ) -> Result<(), TableError> {
        let pane_id = pane_id.into();
        let &index = self
            .by_key
            .get(key)
            .ok_or_else(|| TableError::UnknownKey(key.clone()))?;
        if let Some(&other) = self.by_pane.get(&pane_id)
            && other != index
        {
            return Err(TableError::DuplicatePane(pane_id));
        }

        let previous = self.entries[index].pane_id().map(str::to_string);
        if let Some(previous) = previous {
            self.by_pane.remove(&previous);
        }
        self.entries[index].pane_id = Some(pane_id.clone());
        self.by_pane.insert(pane_id, index);
        Ok(())
    }

    pub fn entry(&self, key: &BotKey) -> Option<&Entry> {
        self.by_key.get(key).map(|&index| &self.entries[index])
    }

    pub fn contains_key(&self, key: &BotKey) -> bool {
        self.by_key.contains_key(key)
    }

    pub fn contains_key_str(&self, key: &str) -> bool {
        BotKey::parse(key).is_some_and(|key| self.contains_key(&key))
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}
