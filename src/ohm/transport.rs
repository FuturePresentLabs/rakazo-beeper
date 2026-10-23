//! The phone↔pris message transport seam.
//!
//! Shape is deliberately the same as ohmOS's own `MeshTransport`
//! (`crates/ohmos-mesh/src/transport.rs:12`: `poll()` then `send(&[u8])`), reduced
//! to the two operations this bridge needs: drain inbound messages and post one
//! outbound reply. ohmOS has no single `Transport` trait on rev `479e7f0` that
//! spans messaging; the daemon HTTP surface (`crates/ohmos-msg-host/src/main.rs`)
//! is what this trait wraps, and [`HttpTransport`] is that wrapper.

use super::{
    msgd::{HttpExchange, MsgdClient},
    wire::{Msg, Platform},
};
use anyhow::{Context, Result};

/// One inbound message that a bridge can route to a pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inbound {
    pub chat_id: i64,
    pub platform: Platform,
    pub chat_remote_id: String,
    pub recipient: String,
    pub message: Msg,
}

/// One outbound reply. `idempotency_key` is local: the daemon's `POST /send`
/// body has no such field (`crates/ohmos-msg-host/src/wire.rs:61`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outbound {
    pub platform: Platform,
    pub chat_remote_id: String,
    pub recipient: String,
    pub text: String,
    pub idempotency_key: String,
}

/// `Queued` is the strongest thing the daemon can say. There is deliberately no
/// `Delivered` variant: a queued reply is never reported as delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Queued,
}

/// The seam the bridge is written against. A real daemon and a scripted fake are
/// interchangeable.
pub trait Transport {
    fn poll(&mut self) -> Result<Vec<Inbound>>;
    fn send(&mut self, outbound: &Outbound) -> Result<SendOutcome>;
}

/// HTTP daemon transport: inbox + every thread, filtered to inbound messages.
pub struct HttpTransport<H: HttpExchange> {
    client: MsgdClient<H>,
}

impl<H: HttpExchange> HttpTransport<H> {
    pub fn new(client: MsgdClient<H>) -> Self {
        Self { client }
    }
}

impl<H: HttpExchange> Transport for HttpTransport<H> {
    fn poll(&mut self) -> Result<Vec<Inbound>> {
        let chats = self.client.inbox()?;
        let mut inbound = Vec::new();
        for chat in chats {
            let messages = self.client.thread(chat.id)?;
            for message in messages.into_iter().filter(|m| !m.from_me) {
                inbound.push(Inbound {
                    chat_id: chat.id,
                    platform: chat.platform,
                    chat_remote_id: chat.remote_id.clone(),
                    recipient: chat.remote_id.clone(),
                    message,
                });
            }
        }
        Ok(inbound)
    }

    fn send(&mut self, outbound: &Outbound) -> Result<SendOutcome> {
        self.client
            .send(
                outbound.platform,
                &outbound.chat_remote_id,
                &outbound.recipient,
                &outbound.text,
            )
            .context("post ohm reply")
    }
}

/// Scripted in-memory transport for offline tests.
#[derive(Default)]
pub struct FakeTransport {
    /// One batch per successive poll.
    pub batches: Vec<Vec<Inbound>>,
    pub sent: Vec<Outbound>,
    pub queue: bool,
}

impl FakeTransport {
    pub fn new(batches: Vec<Vec<Inbound>>) -> Self {
        Self {
            batches,
            sent: Vec::new(),
            queue: true,
        }
    }
}

impl Transport for FakeTransport {
    fn poll(&mut self) -> Result<Vec<Inbound>> {
        if self.batches.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(self.batches.remove(0))
        }
    }

    fn send(&mut self, outbound: &Outbound) -> Result<SendOutcome> {
        if !self.queue {
            anyhow::bail!("fake daemon did not queue the reply");
        }
        self.sent.push(outbound.clone());
        Ok(SendOutcome::Queued)
    }
}
