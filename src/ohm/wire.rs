//! Wire types for the ohmOS messaging daemon (`ohm-msgd`).
//!
//! CITATION: mirrored from ohmOS at rev `479e7f0` (the brief's pin):
//! - `crates/ohmos-shared/src/messages.rs` — `Platform`, `Message`, `Thread`, `Inbox`
//! - `crates/ohmos-msg-host/src/wire.rs` — `ChatDto`, `MessageDto`, `SendRequest`,
//!   `SendResponse`
//!
//! The pinned revision is not on the public remote (it is a local-only commit), so
//! the git dependency could not be fetched. These are deliberately *field-for-field*
//! serde mirrors of the fields this client uses, not a second protocol.

use serde::{Deserialize, Serialize};

/// `ohmos_shared::Platform` (messages.rs:22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    WhatsApp,
    Telegram,
    Signal,
    Meshtastic,
    Meshcore,
    Sms,
    Felicity,
}

impl Platform {
    pub fn label(self) -> &'static str {
        match self {
            Platform::WhatsApp => "WhatsApp",
            Platform::Telegram => "Telegram",
            Platform::Signal => "Signal",
            Platform::Meshtastic => "Meshtastic",
            Platform::Meshcore => "MeshCore",
            Platform::Sms => "SMS",
            Platform::Felicity => "Felicity",
        }
    }
}

/// `GET /inbox` item: `ohmos_msg_host::ChatDto` (wire.rs:17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub id: i64,
    pub platform: Platform,
    pub remote_id: String,
    pub title: String,
    pub contact_id: Option<String>,
    pub last_ts: i64,
    pub last_text: Option<String>,
    pub unread: u32,
}

/// `GET /thread/:id` item: `ohmos_msg_host::MessageDto` (wire.rs:42).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    pub id: i64,
    pub from_me: bool,
    pub sender: String,
    pub timestamp: i64,
    pub text: Option<String>,
    pub media_ref: Option<String>,
}

/// `POST /send` body: `ohmos_msg_host::SendRequest` (wire.rs:61).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendRequest {
    pub platform: Platform,
    pub chat_remote_id: String,
    pub recipient: String,
    pub text: String,
}

/// `POST /send` reply: `ohmos_msg_host::SendResponse` (wire.rs:72).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendResponse {
    /// Accepted into the daemon's outbox. It is **not** proof of delivery.
    pub queued: bool,
}
