//! Minimal IRC wire handling for the Rakazo IRC client.
//!
//! This is deliberately a *client* of the existing `pi-bus-ircd` (and later the
//! TLS/ZNC hop), not a second IRC stack. It parses the subset the fleet bus
//! uses plus the IRCv3 message-tag extensions we can optionally emit:
//! `message-tags`, `draft/reply`, `draft/react`, `draft/multiline`.
//!
//! Nothing here assumes the server advertises a capability. The current ircd
//! replies to `CAP LS` with an empty list, so every tagged form has a plain-text
//! fallback.

use crate::stable_id;
use std::collections::BTreeMap;

/// IRC's hard wire limit, including the trailing CRLF and message tags.
pub const MAX_LINE_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub tags: BTreeMap<String, String>,
    pub prefix: Option<String>,
    pub command: String,
    /// Middle parameters only; the trailing parameter is kept separately so a
    /// `PRIVMSG #x :hello there` body is never split on spaces.
    pub params: Vec<String>,
    pub trailing: Option<String>,
}

impl Message {
    pub fn prefix_nick(&self) -> Option<&str> {
        self.prefix.as_deref().map(|prefix| {
            prefix
                .split_once('!')
                .map(|(nick, _)| nick)
                .or_else(|| prefix.split_once('@').map(|(nick, _)| nick))
                .unwrap_or(prefix)
        })
    }

    /// The trailing parameter, or the last middle parameter when a server
    /// sends `PING token` without a colon.
    pub fn tail(&self) -> Option<&str> {
        self.trailing
            .as_deref()
            .or_else(|| self.params.last().map(String::as_str))
    }

    pub fn cap_name(&self) -> Option<&str> {
        if self.command != "CAP" {
            return None;
        }
        // :server CAP * LS :...  -> params ["*", "LS"]
        self.params.get(1).map(String::as_str)
    }

    pub fn cap_list(&self) -> Option<&str> {
        if self.command != "CAP" {
            return None;
        }
        self.trailing.as_deref()
    }
}

/// Parse one IRC line without its CRLF. Returns `None` for blank/garbage lines.
pub fn parse_line(line: &str) -> Option<Message> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return None;
    }
    let mut rest = line;
    let mut tags = BTreeMap::new();
    if let Some(after_at) = rest.strip_prefix('@') {
        let (tag_text, tail) = after_at.split_once(' ')?;
        tags = parse_tags(tag_text);
        rest = tail;
    }
    let mut prefix = None;
    if let Some(after_colon) = rest.strip_prefix(':') {
        let (value, tail) = after_colon.split_once(' ')?;
        prefix = Some(value.to_string());
        rest = tail;
    }
    let (head, trailing) = match rest.find(" :") {
        Some(index) => (&rest[..index], Some(rest[index + 2..].to_string())),
        None => (rest, None),
    };
    let mut words = head.split_whitespace();
    let command = words.next()?.to_ascii_uppercase();
    let params = words.map(str::to_string).collect();
    Some(Message {
        tags,
        prefix,
        command,
        params,
        trailing,
    })
}

pub fn parse_tags(text: &str) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    for item in text.split(';') {
        if item.is_empty() {
            continue;
        }
        let (key, value) = match item.split_once('=') {
            Some((key, value)) => (key, value),
            None => (item, ""),
        };
        tags.insert(key.to_string(), unescape_tag_value(value));
    }
    tags
}

fn unescape_tag_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

pub fn escape_tag_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            ';' => out.push_str("\\:"),
            ' ' => out.push_str("\\s"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Render an ordered tag set as the leading `@...` field.
pub fn tags_wire(tags: &[(String, String)]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let body: Vec<String> = tags
        .iter()
        .map(|(key, value)| {
            if value.is_empty() {
                key.clone()
            } else {
                format!("{key}={}", escape_tag_value(value))
            }
        })
        .collect();
    format!("@{} ", body.join(";"))
}

/// Source identity for an inbound line.
///
/// IRCv3 `msgid` wins when the server provides it. The current fleet ircd sends
/// no tags, so the fallback is `src:<sha256(target|nick|ts|text|ordinal)>`.
/// `ordinal` is the per-connection receive index; because the ircd replays the
/// same transcript tail on JOIN, a restart sees the same ordinals and replayed
/// duplicates collapse on the inbox primary key. When the fleet ircd tag patch
/// lands, the same column carries the server `msgid` unchanged.
pub fn source_key(target: &str, nick: &str, ts: &str, text: &str, ordinal: u64) -> String {
    format!(
        "src:{}",
        stable_id(&[target, nick, ts, text, &ordinal.to_string()])
    )
}

/// Source key used when an explicit IRCv3 `msgid` tag exists.
pub fn msgid_key(msgid: &str) -> String {
    format!("msgid:{msgid}")
}

/// Split text into UTF-8-safe byte chunks, never exceeding `max` bytes.
pub fn split_bytes(text: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let len = ch.len_utf8();
        if used + len > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += len;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Numbered chunks used when `draft/multiline` is not advertised.
pub fn split_numbered(text: &str, overhead: usize) -> Vec<String> {
    // The 512-byte limit includes the trailing CRLF.
    let max = MAX_LINE_BYTES.saturating_sub(2);
    if overhead + 8 >= max {
        return Vec::new();
    }
    let mut n = 1usize;
    for _ in 0..16 {
        let prefix = format!("[{n}/{n}] ");
        let available = max - overhead - prefix.len();
        if available == 0 {
            return Vec::new();
        }
        let chunks = split_bytes(text, available);
        if chunks.len() == n {
            return chunks;
        }
        n = chunks.len();
    }
    Vec::new()
}

/// Parse a `[i/n] body` numbered chunk. Used for inbox reassembly.
pub fn parse_numbered_chunk(body: &str) -> Option<(usize, usize, &str)> {
    let rest = body.strip_prefix('[')?;
    let (index, rest) = rest.split_once('/')?;
    let (total, rest) = rest.split_once("] ")?;
    let index: usize = index.parse().ok()?;
    let total: usize = total.parse().ok()?;
    (index >= 1 && total >= 1 && index <= total).then_some((index, total, rest))
}

/// True when the wire line (including CRLF) fits IRC's 512-byte limit.
pub fn line_fits(line: &str) -> bool {
    line.len() + 2 <= MAX_LINE_BYTES
}

/// Extract the `(bus:file:line)` pointer the fleet ircd appends to bus entries.
/// It is part of `text`, so it also makes the fallback source key stable across
/// replay-window shifts.
pub fn bus_pointer(text: &str) -> Option<&str> {
    let marker = "(bus:";
    let start = text.rfind(marker)? + marker.len();
    let end = text[start..].find(')')? + start;
    Some(&text[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_prefix_params_and_trailing() {
        let line = "@msgid=abc;+draft/reply=xyz :bus!bus@fleet PRIVMSG #orch :hello there";
        let parsed = parse_line(line).expect("parse");
        assert_eq!(parsed.command, "PRIVMSG");
        assert_eq!(parsed.prefix_nick(), Some("bus"));
        assert_eq!(parsed.params, vec!["#orch"]);
        assert_eq!(parsed.trailing.as_deref(), Some("hello there"));
        assert_eq!(parsed.tags.get("msgid").map(String::as_str), Some("abc"));
    }

    #[test]
    fn empty_cap_ls_is_not_an_error() {
        let parsed = parse_line(":pi-bus.local CAP * LS :").expect("parse");
        assert_eq!(parsed.cap_name(), Some("LS"));
        assert_eq!(parsed.cap_list(), Some(""));
    }

    #[test]
    fn cap_values_are_split_at_equals() {
        let parsed =
            parse_line(":srv CAP * LS :sasl=PLAIN message-tags draft/reply").expect("parse");
        let caps: Vec<&str> = parsed
            .cap_list()
            .unwrap()
            .split_whitespace()
            .map(|cap| cap.split_once('=').map_or(cap, |(name, _)| name))
            .collect();
        assert_eq!(caps, vec!["sasl", "message-tags", "draft/reply"]);
    }

    #[test]
    fn tag_escaping_roundtrips() {
        let raw = "a b;c\\d\n";
        let escaped = escape_tag_value(raw);
        assert!(!escaped.contains(' '));
        assert_eq!(unescape_tag_value(&escaped), raw);
    }

    #[test]
    fn source_key_is_stable_for_the_same_replay_position() {
        let a = source_key("#orch", "bus", "", "hello", 7);
        assert_eq!(a, source_key("#orch", "bus", "", "hello", 7));
        assert_ne!(a, source_key("#orch", "bus", "", "hello", 8));
        assert!(a.starts_with("src:"));
    }

    #[test]
    fn numbered_split_reassembles_with_bounded_lines() {
        let text = "x".repeat(1200);
        let chunks = split_numbered(&text, 20);
        assert!(chunks.len() > 1);
        let mut joined = String::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let line = format!("PRIVMSG #orch :[{}/{}] {}", index + 1, chunks.len(), chunk);
            assert!(line_fits(&line), "line too long: {}", line.len());
            joined.push_str(chunk);
        }
        assert_eq!(joined, text);
    }

    #[test]
    fn numbered_chunks_parse() {
        assert_eq!(parse_numbered_chunk("[2/3] hello"), Some((2, 3, "hello")));
        assert_eq!(parse_numbered_chunk("plain"), None);
    }

    #[test]
    fn bus_pointer_is_extracted() {
        let text = "[orch] from X [CHAT] hi (bus:global.jsonl:42)";
        assert_eq!(bus_pointer(text), Some("global.jsonl:42"));
    }
}
