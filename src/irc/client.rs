//! A small blocking IRC client for the fleet bus.
//!
//! No async runtime. The same [`Client`] works over any `Read + Write`, which is
//! how the capability tests drive it with a scripted socket and how the live TLS
//! transport plugs in (see [`super::tls`]).
//!
//! Sending rules:
//! * every wire line fits IRC's 512-byte limit including tags and CRLF;
//! * tagged forms are used only when the server advertised the capability;
//! * reply/reaction fall back to plain `PRIVMSG` text that the ircd transcript
//!   preserves;
//! * an ambiguous send is never retried automatically.

use super::{
    config::IrcConfig,
    proto::{MAX_LINE_BYTES, Message, line_fits, parse_line, split_numbered, tags_wire},
    tls::{self, Transport},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeSet,
    io::{ErrorKind, Read, Write},
    time::{Duration, Instant},
};

pub struct Client<S: Read + Write> {
    stream: S,
    pending: Vec<u8>,
    pub nick: String,
    pub caps: BTreeSet<String>,
    pub receive_index: u64,
    /// Channels already joined on *this* socket. A reconnect creates a new
    /// `Client`, so the JOIN is replayed on the wire; on one socket it is a
    /// no-op rather than another JOIN that the server would reject.
    joined: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendForm {
    Single,
    NumberedChunks(usize),
    Multiline(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionForm {
    Tagged,
    Fallback,
}

pub type OpenClient = Client<Transport>;

impl OpenClient {
    /// Open the configured transport (plain TCP or TLS + private CA).
    pub fn open(config: &IrcConfig, connect_timeout: Duration) -> Result<Self> {
        let stream = tls::connect(config, connect_timeout)?;
        Ok(Self::new(stream, &config.nick))
    }
}

impl<S: Read + Write> Client<S> {
    pub fn new(stream: S, nick: &str) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            nick: nick.to_string(),
            caps: BTreeSet::new(),
            receive_index: 0,
            joined: BTreeSet::new(),
        }
    }

    pub fn into_stream(self) -> S {
        self.stream
    }

    pub fn has_cap(&self, cap: &str) -> bool {
        self.caps.contains(cap)
    }

    /// Read one complete line, returning `None` on the socket read timeout.
    /// A timeout is normal: it is how the caller stays responsive without a
    /// second thread.
    pub fn read_message(&mut self) -> Result<Option<Message>> {
        loop {
            if let Some(position) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=position).collect();
                let text = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]);
                if let Some(message) = parse_line(&text) {
                    return Ok(Some(message));
                }
                continue;
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => bail!("IRC connection closed by server"),
                Ok(read) => self.pending.extend_from_slice(&chunk[..read]),
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(None);
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("read from IRC server"),
            }
            if self.pending.len() > 64 * 1024 {
                bail!("IRC line exceeded 64 KiB without a newline");
            }
        }
    }

    /// Send one already-complete IRC line. Enforces the 512-byte budget before
    /// anything reaches the socket.
    pub fn send_line(&mut self, line: &str) -> Result<()> {
        ensure!(
            !line.contains(['\r', '\n']),
            "IRC line must not contain CR or LF"
        );
        ensure!(
            line.len() + 2 <= MAX_LINE_BYTES,
            "IRC line is {} bytes; the limit including CRLF is {MAX_LINE_BYTES}",
            line.len() + 2
        );
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\r\n")?;
        self.stream.flush()?;
        Ok(())
    }

    /// `CAP LS` + optional `PASS` + `NICK` + `USER`, then wait for `001`.
    /// Capabilities are recorded but never required.
    ///
    /// `password` is the ZNC secret. It is written to the socket once and is
    /// never echoed, logged, or included in an error message.
    pub fn handshake(
        &mut self,
        deadline: Duration,
        user: &str,
        password: Option<&str>,
    ) -> Result<()> {
        self.send_line("CAP LS 302")?;
        if let Some(password) = password {
            // ZNC's user listener expects `PASS <account>:<password>`.
            self.send_line(&format!("PASS {}:{}", user, password))?;
        }
        self.send_line(&format!("NICK {}", self.nick))?;
        self.send_line(&format!("USER {} 0 * :{}", user, user))?;
        let started = Instant::now();
        let mut cap_ended = false;
        while started.elapsed() < deadline {
            let Some(message) = self.read_message()? else {
                continue;
            };
            self.observe(&message)?;
            match message.command.as_str() {
                "CAP" if message.cap_name() == Some("LS") && !cap_ended => {
                    // `echo-message` makes the server echo our own PRIVMSG, which
                    // is how a send is acknowledged. Requesting the caps we use
                    // also keeps msgid/server-time tags available on ZNC.
                    self.send_line("CAP REQ :echo-message message-tags server-time")?;
                    // CAP negotiation suspends registration until CAP END.
                    self.send_line("CAP END")?;
                    cap_ended = true;
                }
                "001" => {
                    if !cap_ended {
                        // Server without CAP support: registration is complete.
                        self.send_line("CAP END")?;
                    }
                    return Ok(());
                }
                "432" | "433" => bail!("IRC nick {} rejected by the server", self.nick),
                _ => {}
            }
        }
        bail!("IRC registration timed out after {deadline:?}")
    }

    /// `JOIN` and wait for the end-of-names numeric. Replay lines that arrive
    /// while waiting are returned for the caller to store.
    ///
    /// Idempotent per socket: joining a channel we are already in returns no
    /// replay instead of sending a second JOIN. A new connection rebuilds the
    /// set, which is what makes reconnect-then-replay safe.
    pub fn join(&mut self, channel: &str, deadline: Duration) -> Result<Vec<Message>> {
        if self.joined.contains(channel) {
            return Ok(Vec::new());
        }
        self.send_line(&format!("JOIN {channel}"))?;
        let started = Instant::now();
        let mut replay = Vec::new();
        while started.elapsed() < deadline {
            let Some(message) = self.read_message()? else {
                continue;
            };
            self.observe(&message)?;
            if message.command == "366" && message.params.get(1) == Some(&channel.to_string()) {
                self.joined.insert(channel.to_string());
                return Ok(replay);
            }
            if matches!(message.command.as_str(), "403" | "404")
                && message.params.get(1) == Some(&channel.to_string())
            {
                bail!("server refused JOIN {channel}: {:?}", message.trailing);
            }
            if matches!(message.command.as_str(), "PRIVMSG" | "NOTICE" | "TAGMSG") {
                self.receive_index += 1;
                replay.push(message);
            }
        }
        bail!("JOIN {channel} timed out after {deadline:?}")
    }

    /// Apply server-side state from any message. PING is answered here so every
    /// read path keeps the connection alive.
    pub fn observe(&mut self, message: &Message) -> Result<()> {
        if message.command == "PING" {
            let token = message.tail().unwrap_or("");
            self.send_line(&format!("PONG :{token}"))?;
        }
        if matches!(message.command.as_str(), "PART" | "KICK")
            && let Some(channel) = message.params.first()
        {
            self.joined.remove(channel);
        }
        if message.command == "CAP" {
            match message.cap_name() {
                Some("LS") | Some("NEW") => {
                    if let Some(list) = message.cap_list() {
                        for cap in list.split_whitespace() {
                            let name = cap.split_once('=').map_or(cap, |(name, _)| name);
                            self.caps.insert(name.to_string());
                        }
                    }
                }
                Some("DEL") => {
                    if let Some(list) = message.cap_list() {
                        for cap in list.split_whitespace() {
                            let name = cap.split_once('=').map_or(cap, |(name, _)| name);
                            self.caps.remove(name);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Send a `PRIVMSG`, splitting only when the server offers no multiline
    /// support. `msgid` is shared by every chunk when tags are available.
    pub fn send_privmsg(
        &mut self,
        target: &str,
        text: &str,
        msgid: &str,
        reply_to: Option<&str>,
    ) -> Result<SendForm> {
        let tags_ok = self.has_cap("message-tags");
        let tagged_reply = reply_to.is_some() && tags_ok && self.has_cap("draft/reply");
        let mut segments: Vec<String> = text.split('\n').map(str::to_string).collect();
        if let Some(parent) = reply_to {
            if !tagged_reply {
                // Plain-text fallback the fleet transcript preserves. The
                // renderer strips this leading quote line. It is a separate
                // IRC line because a wire line can never contain CR/LF.
                segments.insert(0, format!("> {parent}"));
            }
        }
        let mut total = 0usize;
        let mut used_multiline = false;
        let mut used_numbered = false;
        for (index, segment) in segments.iter().enumerate() {
            let reply = if index == 0 && tagged_reply {
                reply_to
            } else {
                None
            };
            let form = self.send_segment(target, segment, msgid, reply)?;
            match form {
                SendForm::Single => total += 1,
                SendForm::NumberedChunks(count) => {
                    used_numbered = true;
                    total += count;
                }
                SendForm::Multiline(count) => {
                    used_multiline = true;
                    total += count;
                }
            }
        }
        if total == 1 {
            Ok(SendForm::Single)
        } else if used_multiline {
            Ok(SendForm::Multiline(total))
        } else if used_numbered {
            Ok(SendForm::NumberedChunks(total))
        } else {
            Ok(SendForm::Multiline(total))
        }
    }

    fn send_segment(
        &mut self,
        target: &str,
        body: &str,
        msgid: &str,
        reply_to: Option<&str>,
    ) -> Result<SendForm> {
        let tags_ok = self.has_cap("message-tags");
        let mut tags = Vec::new();
        if tags_ok {
            tags.push(("msgid".to_string(), msgid.to_string()));
        }
        if let Some(parent) = reply_to {
            tags.push(("+draft/reply".to_string(), parent.to_string()));
        }
        let single = format!("{}PRIVMSG {target} :{body}", tags_wire(&tags));
        if line_fits(&single) {
            self.send_line(&single)?;
            return Ok(SendForm::Single);
        }
        if self.has_cap("draft/multiline") {
            let count = self.send_multiline(target, body, &tags, msgid)?;
            return Ok(SendForm::Multiline(count));
        }
        let count = self.send_numbered(target, body, &tags)?;
        Ok(SendForm::NumberedChunks(count))
    }

    fn send_numbered(
        &mut self,
        target: &str,
        body: &str,
        tags: &[(String, String)],
    ) -> Result<usize> {
        let overhead = tags_wire(tags).len() + format!("PRIVMSG {target} :").len();
        let chunks = split_numbered(body, overhead);
        ensure!(
            !chunks.is_empty(),
            "message cannot fit the IRC line budget for {target}"
        );
        let total = chunks.len();
        for (index, chunk) in chunks.iter().enumerate() {
            let line = format!(
                "{}PRIVMSG {target} :[{}/{}] {chunk}",
                tags_wire(tags),
                index + 1,
                total
            );
            self.send_line(&line)?;
        }
        Ok(total)
    }

    /// IRCv3 `draft/multiline` over a `BATCH`. Continuation lines carry
    /// `+draft/multiline-concat`; the final `BATCH -id` closes the group.
    fn send_multiline(
        &mut self,
        target: &str,
        body: &str,
        tags: &[(String, String)],
        msgid: &str,
    ) -> Result<usize> {
        let batch = format!("ml{}", &msgid[msgid.len().saturating_sub(8)..]);
        let start = format!("BATCH +{batch} draft/multiline {target}");
        ensure!(line_fits(&start), "multiline BATCH start line too long");
        // Budget against the longest possible per-chunk tag set.
        let worst_tags = {
            let mut worst = tags.to_vec();
            worst.push(("batch".to_string(), batch.clone()));
            worst.push(("+draft/multiline-concat".to_string(), String::new()));
            worst
        };
        let overhead = tags_wire(&worst_tags).len() + format!("PRIVMSG {target} :").len();
        let available = MAX_LINE_BYTES.saturating_sub(2 + overhead);
        ensure!(available > 0, "multiline chunk budget is empty");
        let chunks = super::proto::split_bytes(body, available);
        ensure!(!chunks.is_empty(), "empty multiline body");
        self.send_line(&start)?;
        for (index, chunk) in chunks.iter().enumerate() {
            let mut line_tags = tags.to_vec();
            line_tags.push(("batch".to_string(), batch.clone()));
            if index > 0 {
                line_tags.push(("+draft/multiline-concat".to_string(), String::new()));
            }
            let line = format!("{}PRIVMSG {target} :{chunk}", tags_wire(&line_tags));
            self.send_line(&line)?;
        }
        let end = format!("BATCH -{batch}");
        self.send_line(&end)?;
        Ok(chunks.len())
    }

    /// Reactions use `TAGMSG` only when the server advertises the draft tags;
    /// otherwise the plain-text machine-readable fallback is sent.
    pub fn send_reaction(
        &mut self,
        target: &str,
        msgid: &str,
        emoji: &str,
        wire_id: &str,
    ) -> Result<ReactionForm> {
        if self.has_cap("message-tags")
            && self.has_cap("draft/react")
            && self.has_cap("draft/reply")
        {
            let tags = vec![
                ("+draft/react".to_string(), emoji.to_string()),
                ("+draft/reply".to_string(), msgid.to_string()),
            ];
            let line = format!("{}TAGMSG {target}", tags_wire(&tags));
            self.send_line(&line)?;
            Ok(ReactionForm::Tagged)
        } else {
            self.send_privmsg(target, &format!("reaction {msgid} {emoji}"), wire_id, None)?;
            Ok(ReactionForm::Fallback)
        }
    }
}

/// A preloaded in-memory socket for capability tests and `irc selftest`.
/// Reads come from `input`; writes append to `output`.
pub struct ScriptedIo {
    input: std::io::Cursor<Vec<u8>>,
    pub output: Vec<u8>,
}

impl ScriptedIo {
    pub fn new(input: impl Into<Vec<u8>>) -> Self {
        Self {
            input: std::io::Cursor::new(input.into()),
            output: Vec::new(),
        }
    }

    pub fn output_string(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
}

impl Read for ScriptedIo {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buffer)
    }
}

impl Write for ScriptedIo {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.output.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake_input(caps: &str) -> Vec<u8> {
        format!(":srv CAP * LS :{caps}\r\n:srv 001 rakazo :welcome\r\n").into_bytes()
    }

    fn client_with_caps(caps: &str) -> Client<ScriptedIo> {
        let mut client = Client::new(ScriptedIo::new(handshake_input(caps)), "rakazo");
        client
            .handshake(Duration::from_secs(1), "rakazo", None)
            .expect("scripted handshake");
        client
    }

    #[test]
    fn empty_cap_ls_still_registers() {
        let client = client_with_caps("");
        assert!(client.caps.is_empty());
    }

    #[test]
    fn caps_are_recorded_when_advertised() {
        let client =
            client_with_caps("message-tags draft/reply draft/react draft/multiline sasl=PLAIN");
        assert!(client.has_cap("message-tags"));
        assert!(client.has_cap("draft/reply"));
        assert!(client.has_cap("draft/react"));
        assert!(client.has_cap("draft/multiline"));
        assert!(!client.has_cap("sasl=PLAIN"));
    }

    #[test]
    fn reply_falls_back_to_a_quote_line_without_caps() {
        let mut client = client_with_caps("");
        client
            .send_privmsg("#orch", "hello", "out1", Some("m1"))
            .unwrap();
        let output = client.into_stream().output_string();
        assert!(output.contains("PRIVMSG #orch :> m1\r\n"));
        assert!(output.contains("PRIVMSG #orch :hello\r\n"));
        assert!(!output.contains("+draft/reply"));
        assert!(!output.contains("@msgid"));
    }

    #[test]
    fn reply_uses_a_tag_when_draft_reply_is_advertised() {
        let mut client = client_with_caps("message-tags draft/reply");
        client
            .send_privmsg("#orch", "hello", "out1", Some("m1"))
            .unwrap();
        let output = client.into_stream().output_string();
        assert!(output.contains("@msgid=out1;+draft/reply=m1 PRIVMSG #orch :hello"));
        assert!(!output.contains("> m1"));
    }

    #[test]
    fn reaction_uses_tagmsg_when_advertised() {
        let mut client = client_with_caps("message-tags draft/react draft/reply");
        let form = client.send_reaction("#orch", "m1", "👍", "out1").unwrap();
        assert_eq!(form, ReactionForm::Tagged);
        let output = client.into_stream().output_string();
        assert!(output.contains("TAGMSG #orch"));
        assert!(output.contains("+draft/react="));
        assert!(output.contains("+draft/reply=m1"));
    }

    #[test]
    fn reaction_falls_back_to_plain_text_without_caps() {
        let mut client = client_with_caps("");
        let form = client.send_reaction("#orch", "m1", "👍", "out1").unwrap();
        assert_eq!(form, ReactionForm::Fallback);
        let output = client.into_stream().output_string();
        assert!(output.contains("PRIVMSG #orch :reaction m1 👍"));
        assert!(!output.contains("TAGMSG"));
    }

    #[test]
    fn long_message_is_numbered_and_every_line_fits() {
        let mut client = client_with_caps("");
        let text = "a".repeat(1500);
        let form = client.send_privmsg("#orch", &text, "out1", None).unwrap();
        let SendForm::NumberedChunks(count) = form else {
            panic!("expected numbered chunks, got {form:?}");
        };
        assert!(count > 1);
        let output = client.into_stream().output_string();
        let mut assembled = String::new();
        for line in output
            .lines()
            .filter(|line| line.starts_with("PRIVMSG #orch :["))
        {
            assert!(line_fits(line), "line exceeds budget: {} bytes", line.len());
            let body = line.split_once("] ").unwrap().1;
            assembled.push_str(body);
        }
        assert_eq!(assembled, text);
    }

    #[test]
    fn join_is_idempotent_on_one_socket() {
        let input =
            b":srv CAP * LS :\r\n:srv 001 rakazo :welcome\r\n:srv 366 rakazo #orch :End\r\n";
        let mut client = Client::new(ScriptedIo::new(input.to_vec()), "rakazo");
        client
            .handshake(Duration::from_secs(1), "rakazo", None)
            .unwrap();
        assert!(
            client
                .join("#orch", Duration::from_secs(1))
                .unwrap()
                .is_empty()
        );
        assert!(
            client
                .join("#orch", Duration::from_secs(1))
                .unwrap()
                .is_empty()
        );
        let output = client.into_stream().output_string();
        assert_eq!(output.matches("JOIN #orch").count(), 1);
    }

    #[test]
    fn server_ping_is_answered_with_its_token() {
        let mut client = Client::new(ScriptedIo::new(Vec::new()), "rakazo");
        let ping = parse_line("PING :srv-token-1").expect("ping");
        client.observe(&ping).unwrap();
        let ping_without_colon = parse_line("PING srv-token-2").expect("ping");
        client.observe(&ping_without_colon).unwrap();
        let output = client.into_stream().output_string();
        assert!(output.contains("PONG :srv-token-1\r\n"));
        assert!(output.contains("PONG :srv-token-2\r\n"));
    }

    #[test]
    fn long_message_uses_multiline_batch_when_advertised() {
        let mut client = client_with_caps("message-tags draft/multiline");
        let text = "b".repeat(1500);
        let form = client.send_privmsg("#orch", &text, "out1", None).unwrap();
        let SendForm::Multiline(count) = form else {
            panic!("expected multiline, got {form:?}");
        };
        assert!(count > 1);
        let output = client.into_stream().output_string();
        assert!(output.contains("BATCH +ml"));
        assert!(output.contains("draft/multiline #orch"));
        assert!(output.contains("+draft/multiline-concat"));
        assert!(output.contains("BATCH -ml"));
        for line in output.lines() {
            assert!(line_fits(line), "line exceeds budget: {} bytes", line.len());
        }
    }
}
