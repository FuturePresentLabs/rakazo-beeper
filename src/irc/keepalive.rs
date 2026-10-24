//! Client-side liveness and reconnect state machines.
//!
//! The live hop sits behind ZNC, whose per-user `NoTrafficTimeout` (180 s in the
//! fleet config) closes a client that is completely quiet. A server-side PING is
//! not enough in that direction: ZNC does not necessarily send one before its
//! idle timer expires. [`Keepalive`] therefore sends a client `PING` on a fixed
//! cadence, answers the server's own `PING` elsewhere, and declares the link
//! dead only after a full timeout with no traffic in either direction.
//!
//! [`Backoff`] is deliberately separate: it resets only after a connection has
//! stayed up for [`Backoff::stable_for`], so a server that accepts a JOIN and
//! immediately drops the socket cannot make the client hammer it once a second.

use std::time::{Duration, Instant};

/// What the read loop should do after a keepalive tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeepaliveAction {
    None,
    /// Send this exact line and mark the PING as outstanding.
    SendPing(String),
    /// No traffic in either direction for the full timeout.
    TimedOut,
}

/// Tracks the last traffic in each direction and emits client PINGs.
///
/// A received `PONG` is not special-cased: *any* inbound line proves the link is
/// alive, including channel traffic or a ZNC status message. That avoids
/// disconnecting a busy connection merely because a PONG raced with other data.
#[derive(Debug)]
pub struct Keepalive {
    interval: Duration,
    timeout: Duration,
    last_received: Instant,
    last_sent: Instant,
    awaiting_pong: bool,
    sequence: u64,
}

impl Keepalive {
    /// `timeout` should be greater than `interval`; the caller normally uses
    /// `timeout = 2 * interval`.
    pub fn new(interval: Duration, timeout: Duration, now: Instant) -> Self {
        Self {
            interval,
            timeout,
            last_received: now,
            last_sent: now,
            awaiting_pong: false,
            sequence: 0,
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Any inbound line is liveness evidence and clears an outstanding PING.
    pub fn on_received(&mut self, now: Instant) {
        self.last_received = now;
        self.awaiting_pong = false;
    }

    /// Regular outbound traffic also satisfies ZNC's idle timer. It does not
    /// clear an outstanding PING: only an inbound line proves the peer answered.
    pub fn on_sent(&mut self, now: Instant) {
        self.last_sent = now;
    }

    fn last_activity(&self) -> Instant {
        self.last_received.max(self.last_sent)
    }

    /// Call after each read attempt (including `None` on a socket timeout).
    pub fn poll(&mut self, now: Instant) -> KeepaliveAction {
        if now.saturating_duration_since(self.last_activity()) >= self.timeout {
            return KeepaliveAction::TimedOut;
        }
        if !self.awaiting_pong
            && self.interval > Duration::ZERO
            && now.saturating_duration_since(self.last_sent) >= self.interval
        {
            self.sequence += 1;
            self.last_sent = now;
            self.awaiting_pong = true;
            return KeepaliveAction::SendPing(format!("rzk-keepalive-{}", self.sequence));
        }
        KeepaliveAction::None
    }
}

/// Capped exponential reconnect delay with a stability gate.
#[derive(Clone, Debug)]
pub struct Backoff {
    initial_seconds: u64,
    current_seconds: u64,
    max_seconds: u64,
    stable_for: Duration,
}

impl Backoff {
    /// `max_seconds` is the configured cap; the first retry is always 1 s.
    pub fn new(max_seconds: u64) -> Self {
        Self {
            initial_seconds: 1,
            current_seconds: 1,
            max_seconds: max_seconds.max(1),
            stable_for: Duration::from_secs(60),
        }
    }

    /// Test/constructor seam for a different stability window.
    pub fn with_stable_for(max_seconds: u64, stable_for: Duration) -> Self {
        let mut backoff = Self::new(max_seconds);
        backoff.stable_for = stable_for;
        backoff
    }

    pub fn stable_for(&self) -> Duration {
        self.stable_for
    }

    /// Record a disconnect after `uptime` and return the delay before retrying.
    ///
    /// Uptime below [`Self::stable_for`] means the connection never became
    /// useful, so the delay keeps escalating. A connection that stayed up for
    /// at least that long starts over at one second.
    pub fn on_disconnect(&mut self, uptime: Duration) -> Duration {
        if uptime >= self.stable_for {
            self.current_seconds = self.initial_seconds;
        }
        let delay = Duration::from_secs(self.current_seconds);
        self.current_seconds = self.current_seconds.saturating_mul(2).min(self.max_seconds);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_a_ping_after_the_interval_and_times_out_after_the_timeout() {
        let t0 = Instant::now();
        let mut keepalive = Keepalive::new(Duration::from_secs(60), Duration::from_secs(120), t0);

        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(59)),
            KeepaliveAction::None
        );
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(60)),
            KeepaliveAction::SendPing("rzk-keepalive-1".into())
        );
        // PING is outstanding; do not spam another one while waiting.
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(120)),
            KeepaliveAction::None
        );
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(180)),
            KeepaliveAction::TimedOut
        );
    }

    #[test]
    fn any_inbound_traffic_resets_the_clock_and_clears_the_ping() {
        let t0 = Instant::now();
        let mut keepalive = Keepalive::new(Duration::from_secs(60), Duration::from_secs(120), t0);
        assert!(matches!(
            keepalive.poll(t0 + Duration::from_secs(60)),
            KeepaliveAction::SendPing(_)
        ));

        keepalive.on_received(t0 + Duration::from_secs(61));
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(119)),
            KeepaliveAction::None
        );
        // A fresh interval after the last send (t=60) produces the next PING.
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(120)),
            KeepaliveAction::SendPing("rzk-keepalive-2".into())
        );
        // The inbound line at t=61 keeps the timeout from firing at t=181.
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(181)),
            KeepaliveAction::None
        );
    }

    #[test]
    fn outbound_traffic_delays_the_next_ping_and_the_timeout() {
        let t0 = Instant::now();
        let mut keepalive = Keepalive::new(Duration::from_secs(60), Duration::from_secs(120), t0);
        keepalive.on_sent(t0 + Duration::from_secs(50));
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(109)),
            KeepaliveAction::None
        );
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(110)),
            KeepaliveAction::SendPing("rzk-keepalive-1".into())
        );
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(229)),
            KeepaliveAction::None
        );
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(230)),
            KeepaliveAction::TimedOut
        );
    }

    #[test]
    fn a_zero_interval_disables_client_pings_but_not_the_timeout() {
        let t0 = Instant::now();
        let mut keepalive = Keepalive::new(Duration::ZERO, Duration::from_secs(120), t0);
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(60)),
            KeepaliveAction::None
        );
        assert_eq!(
            keepalive.poll(t0 + Duration::from_secs(120)),
            KeepaliveAction::TimedOut
        );
    }

    #[test]
    fn backoff_escalates_to_the_cap_for_unstable_connections() {
        let mut backoff = Backoff::new(8);
        let mut delays = Vec::new();
        for _ in 0..6 {
            delays.push(backoff.on_disconnect(Duration::from_secs(5)).as_secs());
        }
        assert_eq!(delays, vec![1, 2, 4, 8, 8, 8]);
    }

    #[test]
    fn a_stable_connection_resets_the_backoff() {
        let mut backoff = Backoff::with_stable_for(30, Duration::from_secs(60));
        assert_eq!(backoff.on_disconnect(Duration::ZERO).as_secs(), 1);
        assert_eq!(backoff.on_disconnect(Duration::ZERO).as_secs(), 2);
        assert_eq!(backoff.on_disconnect(Duration::ZERO).as_secs(), 4);
        assert_eq!(
            backoff.on_disconnect(Duration::from_secs(60)).as_secs(),
            1,
            "a session that stayed up for stable_for must reset the delay"
        );
        assert_eq!(backoff.on_disconnect(Duration::ZERO).as_secs(), 2);
    }
}
