//! Rakazo as an IRC frontend.
//!
//! Rakazo is an authed UI over the fleet's existing IRC bus. This module is a
//! real IRC *client*: it connects, joins, reads and writes the bus like any
//! other client. The agent loop never learns about Rakazo — there is no relay,
//! no pi-notify tagging, and no agent extension here.
//!
//! The local rehearsal talks to a private loopback `pi-bus-ircd`; the live hop
//! speaks TLS to ZNC using a private CA and a password file. No client
//! certificate is presented: `CN=rakazo-beeper` is ZNC's upstream identity to
//! `pi-irc-tls`, not this client's.

pub mod client;
pub mod config;
pub mod inbox;
pub mod keepalive;
pub mod proto;
pub mod run;
pub mod tls;

pub use client::{Client, OpenClient, ReactionForm, SendForm};
pub use config::IrcConfig;
pub use inbox::{InboundRecord, Inbox, StoreOutcome, StoredInbound};
