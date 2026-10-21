//! Agent-to-agent bus messages.
//!
//! The schema is the minimum contract from the identity design. Transport and
//! live rendering are M1; this module is deliberately local and has no RPC or
//! network calls.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::route::Table;

/// One bus message, as it appears on the wire and in the recipient history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub ts: String,
    pub body: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
}

impl Message {
    /// Reject an empty identity/body or a `from`/`to` that is not in the
    /// mapping table. Unknown keys are a schema error, not a routing fallback.
    pub fn validate(&self, table: &Table) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("empty id".to_string());
        }
        if self.body.trim().is_empty() {
            return Err("empty body".to_string());
        }
        if self.from.trim().is_empty() {
            return Err("empty from".to_string());
        }
        if self.to.trim().is_empty() {
            return Err("empty to".to_string());
        }
        if !table.contains_key_str(&self.from) {
            return Err(format!("unknown from: {}", self.from));
        }
        if !table.contains_key_str(&self.to) {
            return Err(format!("unknown to: {}", self.to));
        }
        Ok(())
    }

    /// Render in the recipient's conversation.
    pub fn render(&self) -> String {
        format!("[from {}] {}", self.from, self.body)
    }

    /// The content key used by [`Dedupe`].
    pub fn content_hash(&self) -> String {
        hash_body(&self.body)
    }
}

pub fn hash_body(body: &str) -> String {
    hex::encode(Sha256::digest(body.as_bytes()))
}

/// The outcome of accepting one bus message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DedupeResult {
    New,
    Duplicate,
    Echo,
}

/// Content-hash dedupe with a rolling window.
///
/// The key deliberately excludes the message id: relays may rewrite ids, but a
/// repeated `(from, to, body hash)` within 24 hours is one render.
#[derive(Clone, Debug)]
pub struct Dedupe {
    window: Duration,
    seen: HashMap<(String, String, String), Instant>,
}

impl Default for Dedupe {
    fn default() -> Self {
        Self::new()
    }
}

impl Dedupe {
    pub const WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

    pub fn new() -> Self {
        Self::with_window(Self::WINDOW)
    }

    pub fn with_window(window: Duration) -> Self {
        Self {
            window,
            seen: HashMap::new(),
        }
    }

    /// Never echo back to the sender; otherwise accept the first copy and
    /// suppress identical copies inside the window.
    pub fn accept(&mut self, message: &Message) -> DedupeResult {
        if message.to == message.from {
            return DedupeResult::Echo;
        }

        let key = (
            message.from.clone(),
            message.to.clone(),
            message.content_hash(),
        );
        let now = Instant::now();
        if let Some(seen_at) = self.seen.get(&key)
            && now.duration_since(*seen_at) < self.window
        {
            return DedupeResult::Duplicate;
        }
        self.seen.insert(key, now);
        DedupeResult::New
    }
}
