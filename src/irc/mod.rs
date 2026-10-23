//! Rakazo as an IRC frontend.
//!
//! Rakazo is an authed UI over the fleet's existing IRC bus. This module is a
//! real IRC *client*: it connects, joins, reads and writes the bus like any
//! other client. The agent loop never learns about Rakazo — there is no relay,
//! no pi-notify tagging, and no agent extension here.
//!
//! The local rehearsal talks only to a private loopback `pi-bus-ircd`. The TLS +
//! client-certificate hop to `pi-irc-tls`/ZNC is a later increment; certificate
//! paths are carried in [`config::IrcConfig`] but unused today.

pub mod client;
pub mod config;
pub mod inbox;
pub mod proto;
pub mod run;

pub use client::{Client, ReactionForm, SendForm};
pub use config::IrcConfig;
pub use inbox::{InboundRecord, Inbox, StoreOutcome, StoredInbound};
