//! End-to-end operations for the IRC client CLI.
//!
//! The agent loop is never involved: this module only connects to an IRC server,
//! reads/writes the bus channels, and persists rows in the durable inbox.

use super::{
    client::{Client, ReactionForm, SendForm},
    config::IrcConfig,
    inbox::{InboundRecord, Inbox, StoreOutcome, StoredInbound},
    proto::{Message, bus_pointer, msgid_key, source_key},
};
use anyhow::{Result, ensure};
use chrono::SecondsFormat;
use rand::RngCore;
use std::{
    io::Write,
    net::TcpStream,
    thread,
    time::{Duration, Instant, SystemTime},
};

fn now() -> String {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now())
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn new_msgid() -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("rzk-{}", hex::encode(bytes))
}

fn print_inbound(stored: &StoredInbound) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "INBOUND\t{}\t{}\t{}",
        stored.thread_id,
        stored.msg_id,
        stored.body.replace('\n', "\\n")
    )?;
    stdout.flush()?;
    Ok(())
}

/// Translate one parsed server line into inbox state. Returns a row only when a
/// *new* logical message was inserted; replay duplicates return `None` and are
/// never printed or notified.
fn handle_inbound(
    inbox: &Inbox,
    own_nick: &str,
    message: &Message,
    ordinal: u64,
) -> Result<Option<StoredInbound>> {
    match message.command.as_str() {
        "PRIVMSG" => {
            let Some(target) = message.params.first() else {
                return Ok(None);
            };
            let Some(body) = message.trailing.as_deref() else {
                return Ok(None);
            };
            let from = message.prefix_nick().unwrap_or("unknown");
            if from.eq_ignore_ascii_case(own_nick) {
                // Our own echo is an acknowledgement, not a new inbound row.
                if let Some(msgid) = message.tags.get("msgid") {
                    inbox.mark_sent_by_msgid(msgid)?;
                } else {
                    inbox.mark_sent_by_body(target, body)?;
                }
                return Ok(None);
            }
            let tagged = message.tags.get("msgid").filter(|id| !id.is_empty());
            let msg_id = match tagged {
                Some(msgid) => msgid.clone(),
                None => source_key(
                    target,
                    from,
                    message
                        .tags
                        .get("time")
                        .map(String::as_str)
                        .unwrap_or_default(),
                    body,
                    ordinal,
                ),
            };
            let key = match tagged {
                Some(msgid) => msgid_key(msgid),
                None => msg_id.clone(),
            };
            let record = InboundRecord {
                thread_id: target.clone(),
                msg_id,
                body: body.to_string(),
                ts: message.tags.get("time").cloned().unwrap_or_else(now),
                raw_tags: serde_json::to_string(&message.tags)?,
                source_key: key,
                tagged: tagged.is_some(),
            };
            match inbox.store_inbound(record)? {
                StoreOutcome::Inserted(stored) => Ok(Some(stored)),
                StoreOutcome::Duplicate => Ok(None),
            }
        }
        "TAGMSG" => {
            let Some(target) = message.params.first() else {
                return Ok(None);
            };
            let from = message.prefix_nick().unwrap_or("unknown");
            if from.eq_ignore_ascii_case(own_nick) {
                return Ok(None);
            }
            let emoji = message
                .tags
                .get("+draft/react")
                .or_else(|| message.tags.get("draft/react"));
            let parent = message
                .tags
                .get("+draft/reply")
                .or_else(|| message.tags.get("draft/reply"));
            if let (Some(emoji), Some(parent)) = (emoji, parent) {
                if inbox.apply_reaction(target, parent, emoji)? {
                    println!("REACTION\t{target}\t{parent}\t{emoji}");
                }
            }
            Ok(None)
        }
        // NOTICE is parsed but never treated as bus traffic: the ircd uses it
        // for replay banners and "swallowed" client notices.
        _ => Ok(None),
    }
}

fn connect_and_join(config: &IrcConfig, target: Option<&str>) -> Result<Client<TcpStream>> {
    let connect_timeout = Duration::from_secs(5);
    let mut client = Client::tcp(config, connect_timeout)?;
    client.handshake(Duration::from_secs(5))?;
    let mut channels = config.channels.clone();
    if let Some(target) = target {
        if target.starts_with('#') && !channels.iter().any(|channel| channel == target) {
            channels.push(target.to_string());
        }
    }
    for channel in &channels {
        let _ = client.join(channel, Duration::from_secs(5))?;
    }
    Ok(client)
}

fn session(config: &IrcConfig, inbox: &Inbox, once: bool) -> Result<()> {
    let mut client = Client::tcp(config, Duration::from_secs(5))?;
    client.handshake(Duration::from_secs(5))?;
    let mut ordinal = 0u64;
    let mut idle_at = Instant::now();
    for channel in &config.channels {
        let replay = client.join(channel, Duration::from_secs(5))?;
        for message in replay {
            ordinal += 1;
            client.observe(&message)?;
            if let Some(stored) = handle_inbound(inbox, &client.nick, &message, ordinal)? {
                print_inbound(&stored)?;
                idle_at = Instant::now();
            }
        }
    }
    loop {
        match client.read_message()? {
            Some(message) => {
                client.observe(&message)?;
                if matches!(message.command.as_str(), "PRIVMSG" | "TAGMSG" | "NOTICE") {
                    ordinal += 1;
                    if let Some(stored) = handle_inbound(inbox, &client.nick, &message, ordinal)? {
                        print_inbound(&stored)?;
                        idle_at = Instant::now();
                    }
                }
            }
            None => {
                if once && idle_at.elapsed() >= config.idle() {
                    return Ok(());
                }
            }
        }
    }
}

/// Run the durable client. Without `--once` it reconnects with capped backoff
/// and never resends: replay + `source_key` dedupe does the recovery.
pub fn connect(config: &IrcConfig, once: bool) -> Result<()> {
    let inbox = Inbox::open(&config.inbox)?;
    let mut backoff = 1u64;
    loop {
        match session(config, &inbox, once) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if once {
                    return Err(error);
                }
                eprintln!("irc connect failed: {error:#}; reconnecting in {backoff}s (no resend)");
                thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(config.max_backoff_seconds.max(1));
            }
        }
    }
}

fn wait_for_outcome(
    client: &mut Client<TcpStream>,
    inbox: &Inbox,
    own_nick: &str,
    msgid: &str,
    wait: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + wait;
    let mut ordinal = 0u64;
    while Instant::now() < deadline {
        match client.read_message()? {
            Some(message) => {
                client.observe(&message)?;
                if matches!(message.command.as_str(), "PRIVMSG" | "TAGMSG" | "NOTICE") {
                    ordinal += 1;
                    let _ = handle_inbound(inbox, own_nick, &message, ordinal)?;
                }
            }
            None => {}
        }
        if inbox.row(msgid)?.is_some_and(|row| row.status == "sent") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn transmit(config: &IrcConfig, target: &str, text: &str, reply_to: Option<&str>) -> Result<()> {
    ensure!(!text.is_empty(), "message text must not be empty");
    let inbox = Inbox::open(&config.inbox)?;
    let mut client = connect_and_join(config, Some(target))?;
    let msgid = new_msgid();
    let timestamp = now();
    // Durable before the first byte reaches the socket.
    inbox.insert_outbound(target, &msgid, text, &timestamp, reply_to)?;
    if let Some(parent) = reply_to {
        inbox.mark_replied(target, parent)?;
    }
    let form = client.send_privmsg(target, text, &msgid, reply_to)?;
    let chunks = match form {
        SendForm::Single => 1,
        SendForm::NumberedChunks(count) | SendForm::Multiline(count) => count,
    };
    println!("sent\tmsgid={msgid}\tform={form:?}\tlines={chunks}\tstatus=pending");
    let own_nick = client.nick.clone();
    let sent = wait_for_outcome(
        &mut client,
        &inbox,
        &own_nick,
        &msgid,
        Duration::from_millis(800),
    )?;
    let status = if sent { "sent" } else { "unknown" };
    inbox.set_outbound_status(&msgid, status)?;
    println!("outcome\tmsgid={msgid}\tstatus={status}");
    Ok(())
}

pub fn send(config: &IrcConfig, target: &str, text: &str) -> Result<()> {
    transmit(config, target, text, None)
}

pub fn reply(config: &IrcConfig, target: &str, msgid: &str, text: &str) -> Result<()> {
    ensure!(!msgid.is_empty(), "reply msgid must not be empty");
    transmit(config, target, text, Some(msgid))
}

pub fn react(config: &IrcConfig, target: &str, msgid: &str, emoji: &str) -> Result<()> {
    ensure!(!msgid.is_empty(), "reaction msgid must not be empty");
    ensure!(!emoji.is_empty(), "reaction emoji must not be empty");
    let inbox = Inbox::open(&config.inbox)?;
    // Local persistence is authoritative for the UI; write it before the wire.
    inbox.apply_reaction(target, msgid, emoji)?;
    let mut client = connect_and_join(config, Some(target))?;
    let wire_id = new_msgid();
    let timestamp = now();
    let body = format!("reaction {msgid} {emoji}");
    inbox.insert_outbound(target, &wire_id, &body, &timestamp, None)?;
    let form = client.send_reaction(target, msgid, emoji, &wire_id)?;
    println!("sent\tmsgid={wire_id}\tform={form:?}\tstatus=pending");
    match form {
        ReactionForm::Tagged => {
            // TAGMSG is not echoed by the fleet ircd; the local reaction row is
            // already authoritative, so the wire outcome stays unknown.
            inbox.set_outbound_status(&wire_id, "unknown")?;
            println!("outcome\tmsgid={wire_id}\tstatus=unknown");
        }
        ReactionForm::Fallback => {
            let own_nick = client.nick.clone();
            let sent = wait_for_outcome(
                &mut client,
                &inbox,
                &own_nick,
                &wire_id,
                Duration::from_millis(800),
            )?;
            let status = if sent { "sent" } else { "unknown" };
            inbox.set_outbound_status(&wire_id, status)?;
            println!("outcome\tmsgid={wire_id}\tstatus={status}");
        }
    }
    Ok(())
}

/// Offline proof of capability handling, chunk budgets and source identity.
pub fn selftest() -> Result<()> {
    use super::client::ScriptedIo;

    let input = b":srv CAP * LS :message-tags draft/reply draft/react draft/multiline\r\n:srv 001 rakazo :welcome\r\n";
    let mut client = Client::new(ScriptedIo::new(input.to_vec()), "rakazo");
    client.handshake(Duration::from_secs(1))?;
    ensure!(client.has_cap("draft/react"), "caps not recorded");
    client.send_privmsg("#orch", "hello", "selftest-1", Some("parent"))?;
    client.send_reaction("#orch", "parent", "👍", "selftest-2")?;
    let output = client.into_stream().output_string();
    ensure!(
        output.contains("+draft/reply=parent"),
        "tagged reply not emitted"
    );
    ensure!(
        output.contains("TAGMSG #orch"),
        "tagged reaction not emitted"
    );

    let mut plain = Client::new(
        ScriptedIo::new(b":srv CAP * LS :\r\n:srv 001 rakazo :welcome\r\n".to_vec()),
        "rakazo",
    );
    plain.handshake(Duration::from_secs(1))?;
    plain.send_privmsg("#orch", "hello", "selftest-3", Some("parent"))?;
    plain.send_reaction("#orch", "parent", "👍", "selftest-4")?;
    let fallback = plain.into_stream().output_string();
    ensure!(
        fallback.contains("PRIVMSG #orch :> parent"),
        "plain reply fallback missing"
    );
    ensure!(
        fallback.contains("reaction parent 👍"),
        "plain reaction fallback missing"
    );

    ensure!(
        source_key("#orch", "bus", "", "hi", 1) == source_key("#orch", "bus", "", "hi", 1),
        "source key is not stable"
    );
    ensure!(
        bus_pointer("x (bus:global.jsonl:3)").is_some(),
        "bus pointer parsing failed"
    );
    println!("selftest\tok\tcaps+fallbacks+source_key+line_budget");
    Ok(())
}
