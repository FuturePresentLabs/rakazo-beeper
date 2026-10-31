//! Transport-neutral group-conversation primitives.
//!
//! Rakazo core historically models only 1:1 threads (`Bot { thread_id }`).
//! This module adds a *group* as a first-class, transport-neutral concept so
//! IRC channels, Beeper chats and ohmOS conversations can all feed the same
//! routing rules instead of each adapter inventing its own.
//!
//! Design rules (see docs/group-conversation.md):
//!   - a stable, transport-prefixed group id (`irc:#orch`, `beeper:<chat>`,
//!     `ohm:<conv>`, `rakazo:<thread>` for the 1:1 backward-compat case);
//!   - real sender/member attribution via canonical agent keys (from
//!     pi-agentsmith: `pm-<vertical>`, `agora-one`, `neo`);
//!   - mention/reply routing on top of `reply_to_message_id`;
//!   - opt-in response policy (orchestrator-only by default);
//!   - no agent echo loops (sender == self, or a known bot, never replies);
//!   - membership is separate from routing (who is in vs. who gets notified);
//!   - dedupe keyed on (group, message, sender);
//!   - durable receipt semantics (delivered when the notifier acks).
//!
//! Nothing here touches a transport. Adapters map their native group id to a
//! `GroupId` and feed `Message`s with `group_id`/`sender_key`/`mentions` set.

use serde::{Deserialize, Serialize};

/// Stable, transport-neutral group id. Format: "<transport>:<native-group-id>".
/// For 1:1 threads this is "rakazo:<thread_id>" (backward compat).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct GroupId(pub String);

impl GroupId {
    pub fn new(transport: &str, native_id: &str) -> Self {
        Self(format!("{transport}:{native_id}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Irc,
    Beeper,
    Ohm,
    Rakazo,
}

impl TransportKind {
    pub fn prefix(&self) -> &'static str {
        match self {
            TransportKind::Irc => "irc",
            TransportKind::Beeper => "beeper",
            TransportKind::Ohm => "ohm",
            TransportKind::Rakazo => "rakazo",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptInPolicy {
    /// Only the orchestrator replies, and only when addressed or on a human
    /// message with no explicit mentions.
    OrchestratorOnly,
    /// Only agents explicitly @mentioned reply.
    AddressedOnly,
    /// Every non-relay member may reply.
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    Orchestrator,
    Pm,
    Worker,
    Human,
    Muse,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct GroupMember {
    /// Canonical agent key from pi-agentsmith (e.g. "fleet-pm-rakazo-v1",
    /// "fleet-orch-v1", "agora-one", "neo"). Humans use "human:<nick>".
    pub agent_key: String,
    pub role: MemberRole,
    /// Whether this member is relay-only (may never initiate a reply).
    #[serde(default)]
    pub relay_only: bool,
    pub joined_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Group {
    pub id: GroupId,
    pub transport: TransportKind,
    pub native_id: String,
    pub members: Vec<GroupMember>,
    pub opt_in: OptInPolicy,
    pub created_at: Option<String>,
}

impl Group {
    pub fn new(id: GroupId, transport: TransportKind, native_id: String) -> Self {
        Self {
            id,
            transport,
            native_id,
            members: Vec::new(),
            opt_in: OptInPolicy::OrchestratorOnly,
            created_at: None,
        }
    }

    pub fn member(&self, key: &str) -> Option<&GroupMember> {
        self.members.iter().find(|m| m.agent_key == key)
    }

    pub fn is_member(&self, key: &str) -> bool {
        self.member(key).is_some()
    }
}

/// Stable dedupe key for a group message. Extends event_nonces.
pub fn dedupe_key(group_id: &GroupId, message_id: &str, sender_key: &str) -> String {
    format!("{}:{}:{}", group_id.as_str(), message_id, sender_key)
}

/// Whether `self_key` should respond to `msg` in `group`.
///
/// Rules (transport-neutral):
///   - never reply to yourself (no echo);
///   - never reply to a known bot unless explicitly @mentioned;
///   - relay-only members never reply;
///   - OptInPolicy decides the rest.
pub fn should_respond(group: &Group, msg: &Message, self_key: &str) -> bool {
    // sender attribution is required to make any decision
    let sender = match msg.sender_key.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    // no echo
    if sender == self_key {
        return false;
    }
    let me = match group.member(self_key) {
        Some(m) => m,
        None => return false, // not a member -> cannot respond
    };
    if me.relay_only {
        return false;
    }
    let mentioned = msg.mentions.iter().any(|m| m == self_key);
    let sender_is_bot = group
        .member(sender)
        .map(|m| !matches!(m.role, MemberRole::Human))
        .unwrap_or(false);

    match group.opt_in {
        OptInPolicy::OrchestratorOnly => {
            if !matches!(me.role, MemberRole::Orchestrator) {
                return false;
            }
            // orchestrator responds to human messages (unless a specific agent
            // is @mentioned, in which case only that agent responds) and to
            // explicit mentions of itself.
            if mentioned {
                return true;
            }
            if sender_is_bot {
                return false; // bots talking to bots: stay quiet unless addressed
            }
            // human message with no explicit mention -> orchestrator may respond
            true
        }
        OptInPolicy::AddressedOnly => mentioned,
        OptInPolicy::All => {
            // bots talking to bots stay quiet unless addressed
            if sender_is_bot && !mentioned {
                return false;
            }
            true
        }
    }
}

/// Whether a message is an echo of our own send (own_nonces or self sender).
pub fn is_echo(msg: &Message, self_key: &str, own_nonces: &[String]) -> bool {
    if msg.sender_key.as_deref() == Some(self_key) {
        return true;
    }
    msg.client_nonce
        .as_deref()
        .is_some_and(|n| own_nonces.iter().any(|o| o == n))
}

// Re-export Message from rakazo so callers don't need both.
use crate::rakazo::Message;

#[cfg(test)]
mod tests {
    use super::*;

    fn member(key: &str, role: MemberRole, relay_only: bool) -> GroupMember {
        GroupMember {
            agent_key: key.to_string(),
            role,
            relay_only,
            joined_at: None,
        }
    }

    fn group(policy: OptInPolicy) -> Group {
        let mut g = Group::new(GroupId("irc:#orch".into()), TransportKind::Irc, "#orch".into());
        g.opt_in = policy;
        g.members.push(member("fleet-orch-v1", MemberRole::Orchestrator, false));
        g.members.push(member("fleet-pm-rakazo-v1", MemberRole::Pm, true));
        g.members.push(member("human:alice", MemberRole::Human, false));
        g
    }

    fn msg(sender: &str, mentions: &[&str]) -> Message {
        Message {
            id: "m1".into(),
            seq: 1,
            role: "user".into(),
            blocks: vec![],
            created_at: None,
            client_nonce: None,
            reply_to_message_id: None,
            reply_quote: None,
            run_id: None,
            reactions: vec![],
            group_id: Some(GroupId("irc:#orch".into())),
            sender_key: Some(sender.into()),
            mentions: mentions.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn orchestrator_only_human_message_notifies_orch() {
        let g = group(OptInPolicy::OrchestratorOnly);
        assert!(should_respond(&g, &msg("human:alice", &[]), "fleet-orch-v1"));
    }

    #[test]
    fn orchestrator_only_pm_message_does_not_notify_orch() {
        let g = group(OptInPolicy::OrchestratorOnly);
        // a PM (relay-only bot) speaking does not wake the orchestrator unless
        // explicitly mentioned
        assert!(!should_respond(
            &g,
            &msg("fleet-pm-rakazo-v1", &[]),
            "fleet-orch-v1"
        ));
    }

    #[test]
    fn addressed_only_notifies_mentioned() {
        // relay_only agents never reply even when mentioned; a non-relay
        // member who is @mentioned does.
        let mut g = group(OptInPolicy::AddressedOnly);
        g.members.push(member("worker-x", MemberRole::Worker, false));
        assert!(should_respond(
            &g,
            &msg("human:alice", &["worker-x"]),
            "worker-x"
        ));
        assert!(!should_respond(
            &g,
            &msg("human:alice", &["worker-x"]),
            "fleet-orch-v1"
        ));
    }

    #[test]
    fn no_echo() {
        let g = group(OptInPolicy::All);
        assert!(!should_respond(&g, &msg("fleet-orch-v1", &[]), "fleet-orch-v1"));
    }

    #[test]
    fn relay_only_never_responds() {
        let g = group(OptInPolicy::All);
        assert!(!should_respond(&g, &msg("human:alice", &[]), "fleet-pm-rakazo-v1"));
    }

    #[test]
    fn dedupe_key_stable() {
        let gid = GroupId("irc:#orch".into());
        let k1 = dedupe_key(&gid, "m1", "human:alice");
        let k2 = dedupe_key(&gid, "m1", "human:alice");
        assert_eq!(k1, k2);
        assert_eq!(k1, "irc:#orch:m1:human:alice");
    }

    #[test]
    fn is_echo_by_sender() {
        let m = msg("fleet-orch-v1", &[]);
        assert!(is_echo(&m, "fleet-orch-v1", &[]));
    }

    #[test]
    fn is_echo_by_nonce() {
        let mut m = msg("human:alice", &[]);
        m.client_nonce = Some("n1".into());
        assert!(is_echo(&m, "fleet-orch-v1", &["n1".to_string()]));
    }

    #[test]
    fn backward_compat_no_group_fields() {
        // old Message JSON without group_id/sender_key/mentions must parse
        let raw = r#"{"id":"x","seq":1,"role":"user","blocks":[]}"#;
        let m: Message = serde_json::from_str(raw).unwrap();
        assert!(m.group_id.is_none());
        assert!(m.sender_key.is_none());
        assert!(m.mentions.is_empty());
        // routing treats it as not-a-group: no sender -> no response
        let g = group(OptInPolicy::All);
        assert!(!should_respond(&g, &m, "fleet-orch-v1"));
    }

    #[test]
    fn group_id_serde_roundtrip() {
        let g = GroupId("irc:#orch".into());
        let s = serde_json::to_string(&g).unwrap();
        let back: GroupId = serde_json::from_str(&s).unwrap();
        assert_eq!(g, back);
    }
}
