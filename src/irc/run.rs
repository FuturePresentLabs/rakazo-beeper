//! End-to-end operations for the IRC client CLI.
//!
//! The agent loop is never involved: this module only connects to an IRC server,
//! reads/writes the bus channels, and persists rows in the durable inbox.

use super::{
    client::{Client, OpenClient, ReactionForm, SendForm},
    config::{IrcConfig, expand_path},
    inbox::{InboundRecord, Inbox, StoreOutcome, StoredInbound},
    proto::{Message, bus_pointer, msgid_key, source_key},
};
use anyhow::{Context, Result, ensure};
use chrono::SecondsFormat;
use rand::RngCore;
use std::{
    collections::BTreeMap,
    io::Write,
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
pub(crate) fn handle_inbound(
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

/// Read the ZNC password from its file. The contents never leave this function
/// except as the single `PASS` line; errors name only the path.
fn read_password(config: &IrcConfig) -> Result<Option<String>> {
    let Some(path) = config.password_file.as_ref() else {
        return Ok(None);
    };
    let path = expand_path(path);
    let secret = std::fs::read_to_string(&path)
        .with_context(|| format!("read IRC password file {}", path.display()))?;
    let secret = secret.trim_end_matches(['\r', '\n']).to_string();
    ensure!(
        !secret.is_empty(),
        "IRC password file {} is empty",
        path.display()
    );
    Ok(Some(secret))
}

fn open_client(config: &IrcConfig) -> Result<OpenClient> {
    let connect_timeout = Duration::from_secs(10);
    let mut client = OpenClient::open(config, connect_timeout)?;
    let nickname = client.nick.clone();
    let password = read_password(config)?;
    client
        .handshake(
            Duration::from_secs(45),
            config.user_name(),
            password.as_deref(),
        )
        .with_context(|| format!("IRC registration as {nickname} failed"))?;
    Ok(client)
}

fn connect_and_join(config: &IrcConfig, target: Option<&str>) -> Result<OpenClient> {
    let mut client = open_client(config)?;
    let mut channels = config.channels.clone();
    if let Some(target) = target {
        if target.starts_with('#') && !channels.iter().any(|channel| channel == target) {
            channels.push(target.to_string());
        }
    }
    for channel in &channels {
        let _ = client.join(channel, Duration::from_secs(30))?;
    }
    Ok(client)
}

fn session(config: &IrcConfig, inbox: &Inbox, once: bool, idle: Duration) -> Result<()> {
    let mut client = open_client(config)?;
    let mut ordinal = 0u64;
    let mut idle_at = Instant::now();
    for channel in &config.channels {
        let replay = client.join(channel, Duration::from_secs(30))?;
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
                if once && idle_at.elapsed() >= idle {
                    return Ok(());
                }
            }
        }
    }
}

/// Run the durable client. Without `--once` it reconnects with capped backoff
/// and never resends: replay + `source_key` dedupe does the recovery.
/// `seconds` overrides the configured quiet window for `--once`.
pub fn connect(config: &IrcConfig, once: bool, seconds: Option<u64>) -> Result<()> {
    let inbox = Inbox::open(&config.inbox)?;
    let idle = seconds
        .map(Duration::from_secs)
        .unwrap_or_else(|| config.idle());
    let mut backoff = 1u64;
    loop {
        match session(config, &inbox, once, idle) {
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
    client: &mut OpenClient,
    inbox: &Inbox,
    own_nick: &str,
    msgid: &str,
    wait: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + wait;
    let mut ordinal = 0u64;
    let mut announced_echo = false;
    while Instant::now() < deadline {
        match client.read_message()? {
            Some(message) => {
                client.observe(&message)?;
                if !announced_echo
                    && message.command == "PRIVMSG"
                    && message
                        .prefix_nick()
                        .is_some_and(|from| from.eq_ignore_ascii_case(own_nick))
                {
                    let target = message.params.first().cloned().unwrap_or_default();
                    // The server echo is the acknowledgement; the inbox row is
                    // updated by `handle_inbound` below.
                    println!("echo\tmsgid={msgid}\ttarget={target}\tstatus=acked");
                    announced_echo = true;
                }
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
        Duration::from_secs(3),
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
                Duration::from_secs(3),
            )?;
            let status = if sent { "sent" } else { "unknown" };
            inbox.set_outbound_status(&wire_id, status)?;
            println!("outcome\tmsgid={wire_id}\tstatus={status}");
        }
    }
    Ok(())
}

/// Backfill the durable inbox from the fleet-bus IRC transcript. This is the
/// bootstrapping path when ZNC has no live #orch traffic to replay: the same
/// `handle_inbound` used by the socket path stores each line, keyed stably so a
/// second run inserts nothing.
pub fn ingest(
    config: &IrcConfig,
    transcript: &std::path::Path,
    target: &str,
    limit: usize,
) -> Result<()> {
    ensure!(limit >= 1, "ingest limit must be positive");
    let inbox = Inbox::open(&config.inbox)?;
    let text = std::fs::read_to_string(transcript)
        .with_context(|| format!("read bus transcript {}", transcript.display()))?;
    let mut ordinal = 0u64;
    let mut inserted = 0usize;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry.get("target").and_then(|value| value.as_str()) != Some(target) {
            continue;
        }
        let Some(nick) = entry.get("nick").and_then(|value| value.as_str()) else {
            continue;
        };
        if nick.eq_ignore_ascii_case(&config.nick) {
            // Our own bus echo is not inbound traffic.
            continue;
        }
        let Some(body) = entry.get("text").and_then(|value| value.as_str()) else {
            continue;
        };
        let ts = entry
            .get("ts")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        ordinal += 1;
        let message = Message {
            tags: BTreeMap::from([("time".to_string(), ts.to_string())]),
            prefix: Some(format!("{nick}!{nick}@bus")),
            command: "PRIVMSG".to_string(),
            params: vec![target.to_string()],
            trailing: Some(body.to_string()),
        };
        if let Some(stored) = handle_inbound(&inbox, &config.nick, &message, ordinal)? {
            print_inbound(&stored)?;
            inserted += 1;
            if inserted >= limit {
                break;
            }
        }
    }
    ensure!(
        inserted > 0,
        "no new {target} bus lines in {}",
        transcript.display()
    );
    println!(
        "ingest\tok\ttarget={target}\trows={inserted}\tsource={}",
        transcript.display()
    );
    Ok(())
}

/// Offline proof of capability handling, chunk budgets and source identity.
pub fn selftest() -> Result<()> {
    use super::client::ScriptedIo;

    let input = b":srv CAP * LS :message-tags draft/reply draft/react draft/multiline\r\n:srv 001 rakazo :welcome\r\n";
    let mut client = Client::new(ScriptedIo::new(input.to_vec()), "rakazo");
    client.handshake(Duration::from_secs(1), "rakazo", None)?;
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
    plain.handshake(Duration::from_secs(1), "rakazo", None)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ingest_skips_own_nick_and_dedupes_on_a_second_pass() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let mut file = std::fs::File::create(&transcript).unwrap();
        writeln!(
            file,
            "{{\"ts\":\"2026-10-23T00:00:00Z\",\"nick\":\"rakazo\",\"target\":\"#orch\",\"text\":\"self\"}}"
        )
        .unwrap();
        writeln!(
            file,
            "{{\"ts\":\"2026-10-23T00:00:01Z\",\"nick\":\"bus\",\"target\":\"#orch\",\"text\":\"hello\"}}"
        )
        .unwrap();
        writeln!(
            file,
            "{{\"ts\":\"2026-10-23T00:00:02Z\",\"nick\":\"bus\",\"target\":\"#other\",\"text\":\"skip\"}}"
        )
        .unwrap();
        let config = IrcConfig {
            inbox: dir.path().join("inbox.sqlite"),
            ..IrcConfig::default()
        };

        ingest(&config, &transcript, "#orch", 1).unwrap();
        let inbox = Inbox::open(&config.inbox).unwrap();
        assert_eq!(inbox.count().unwrap(), 1);
        let bodies: Vec<String> = {
            let conn = rusqlite::Connection::open(&config.inbox).unwrap();
            let mut statement = conn
                .prepare("SELECT body FROM irc_messages WHERE direction='inbound'")
                .unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert_eq!(bodies, vec!["hello".to_string()]);

        let error = ingest(&config, &transcript, "#orch", 1).unwrap_err();
        assert!(format!("{error:#}").contains("no new #orch bus lines"));
        assert_eq!(Inbox::open(&config.inbox).unwrap().count().unwrap(), 1);
    }
}
