//! Fleet identity, routing, tapback and A2A logic.
//!
//! M0 is intentionally local: no Rakazo RPC, no bot creation and no
//! credentials. `main.rs` is not wired to this module until M1.
//!
//! `ack` is the exception: the acknowledgement discipline is live policy, so
//! the bridge configuration and the bridge runtime both use it.

pub mod a2a;
pub mod ack;
pub mod route;
pub mod tapback;

pub use a2a::{Dedupe, DedupeResult, Message};
pub use ack::{
    ACK_REACTION, AckBlock, AckDecision, AckEffects, AckOutcome, AckPolicy, AckRequest, Obligation,
    ObligationLedger, ObligationStatus, SweepReport,
};
pub use route::{BotKey, Entry, Resolution, Table, TableError, Vertical};
pub use tapback::{AnswerLedger, Ask, InMemoryLedger, RecordOutcome, Signal, Verb};

/// A T2 worker line for the PM's conversation. The worker has no bot and no
/// conversation of its own; the attribution is the only identity it gets.
pub fn worker_attribution(slug: &str, body: &str) -> String {
    format!("worker:{slug}: {body}")
}
