//! Read one agent's last answer straight out of its live pi pane.
//!
//! This is the one-command reply hook: `bridge reply --from-pane <worker>`
//! captures the pane with `tmux capture-pane -p -t <pane>` and posts the last
//! assistant block, so answering never needs a copy/paste and never posts pi's
//! TUI chrome (status footer, box rules, tool-call lines) into a human chat.
//!
//! Parsing is deliberately conservative: an unrecognizable pane is a refusal,
//! not a guess. The caller gets a typed [`PaneError`] so a hook can tell "pane
//! is gone" from "pane has no answer yet".

use anyhow::Result;
use std::{fmt, process::Command};

/// Why a pane could not become a reply. Every variant refuses to post anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneError {
    /// `tmux capture-pane` failed: session/pane is gone.
    Missing(String, String),
    /// The pane captured as nothing at all.
    Empty(String),
    /// The pane held only pi TUI chrome, no assistant prose.
    NoAnswer(String),
    /// The candidate answer is a tool call or a bare file path.
    NotAnAnswer(String),
}

impl PaneError {
    /// Stable machine-readable code, safe to branch on in a hook.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Missing(..) => "pane-missing",
            Self::Empty(_) => "pane-empty",
            Self::NoAnswer(_) => "pane-no-answer",
            Self::NotAnAnswer(_) => "pane-not-an-answer",
        }
    }
}

impl fmt::Display for PaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(pane, why) => write!(f, "pane-missing: {pane}: {why}"),
            Self::Empty(pane) => write!(f, "pane-empty: {pane} captured no text"),
            Self::NoAnswer(pane) => {
                write!(f, "pane-no-answer: {pane} has no assistant block yet")
            }
            Self::NotAnAnswer(why) => write!(f, "pane-not-an-answer: {why}"),
        }
    }
}

impl std::error::Error for PaneError {}

/// Capture a pane and return its last assistant block.
pub fn from_pane(pane: &str) -> Result<String> {
    last_assistant(pane, &capture(pane)?)
}

/// Capture the visible pane exactly as tmux renders it.
pub fn capture(pane: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", pane])
        .output()
        .map_err(|error| PaneError::Missing(pane.to_string(), error.to_string()))?;
    if !output.status.success() {
        return Err(PaneError::Missing(
            pane.to_string(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )
        .into());
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.trim().is_empty() {
        return Err(PaneError::Empty(pane.to_string()).into());
    }
    Ok(text)
}

/// Extract the last assistant block from a captured pi pane.
pub fn last_assistant(pane: &str, captured: &str) -> Result<String> {
    let stripped = strip_ansi(captured);
    let lines: Vec<&str> = stripped
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    // Walk up from the bottom: skip trailing blank/chrome, then take the
    // contiguous prose run, stopping at the first piece of tool/UI chrome.
    let mut end = lines.len();
    while end > 0 && (lines[end - 1].trim().is_empty() || is_chrome(lines[end - 1])) {
        end -= 1;
    }
    if end == 0 {
        return Err(PaneError::NoAnswer(pane.to_string()).into());
    }
    let mut start = end;
    while start > 0 {
        let line = lines[start - 1];
        if is_chrome(line) {
            break;
        }
        start -= 1;
    }

    let mut answer: Vec<&str> = lines[start..end].to_vec();
    while answer.first().is_some_and(|line| line.trim().is_empty()) {
        answer.remove(0);
    }
    while answer.last().is_some_and(|line| line.trim().is_empty()) {
        answer.pop();
    }
    // pi indents assistant prose by one space; drop exactly that, not the
    // user's own indentation.
    let text = answer
        .iter()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim_end().to_string();

    if text.trim().is_empty() {
        return Err(PaneError::NoAnswer(pane.to_string()).into());
    }
    if answer.iter().any(|line| is_tool_call(line)) {
        return Err(PaneError::NotAnAnswer(format!(
            "{pane}: last block is a tool call, not an answer"
        ))
        .into());
    }
    if is_bare_path(&text) {
        return Err(PaneError::NotAnAnswer(format!(
            "{pane}: last block is a bare file path, not an answer"
        ))
        .into());
    }
    Ok(text)
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // CSI ... final byte in @..~
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC ... terminated by BEL or ST
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) | None => {}
        }
    }
    out
}

/// pi TUI chrome: status footer, box rules, tool-call echoes, prompt box.
fn is_chrome(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Horizontal rules made only of box/line characters.
    if trimmed.chars().all(|c| "─━═╌╍┄┅▁▔-".contains(c)) {
        return true;
    }
    // Box borders and the prompt/input frame.
    if trimmed.starts_with(['╭', '╰', '├', '┌', '└', '│', '┃']) {
        return true;
    }
    // pi status footer: "~/.pi/agent (branch) • worker", token counters.
    if trimmed.contains("• ")
        && (trimmed.starts_with('~')
            || trimmed.starts_with('/')
            || trimmed.contains('↑')
            || trimmed.contains('↓'))
    {
        return true;
    }
    if trimmed.contains('↑') && trimmed.contains('↓') && trimmed.contains("CH") {
        return true;
    }
    if trimmed.contains("(freetier)") && trimmed.contains('•') {
        return true;
    }
    // Tool invocation and output chrome.
    if trimmed.starts_with("$ ") || trimmed.starts_with("❯ ") {
        return true;
    }
    if trimmed.starts_with("... (") && trimmed.contains("ctrl+o") {
        return true;
    }
    if trimmed.starts_with("Took ") && trimmed.ends_with('s') {
        return true;
    }
    if trimmed.starts_with("COST ") || trimmed.starts_with("bus: posted") {
        return true;
    }
    if trimmed.starts_with("● ") && trimmed.ends_with('…') {
        return true;
    }
    false
}

fn is_tool_call(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("$ ") || trimmed.starts_with("❯ ")
}

/// A single token that is only a path, never prose.
fn is_bare_path(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.split_whitespace().count() != 1 {
        return false;
    }
    trimmed.contains('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("~/")
        || trimmed.starts_with("src/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_PANE: &str = "\
   Running tests now


 $ cargo test (timeout 180s)

 ... (2 earlier lines, ctrl+o to expand)
 test result: ok. 12 passed; 0 failed

 Took 4.2s


 Anti-echo is fixed and the policy paragraph is honest.

 The old 95% phrasing is withdrawn; the landed floor is absolute.

───────────────────────────────────────────────────────────────
~/.pi/agent (master) • pm-viceroy
↑521k ↓154k R38M CH99.7% $0.285 27.4%/1.0M (auto)   (freetier) chain • low";

    #[test]
    fn takes_the_last_assistant_block_and_drops_chrome() {
        let answer = last_assistant("%40", AGENT_PANE).unwrap();
        assert!(answer.starts_with("Anti-echo is fixed"));
        assert!(answer.contains("The old 95% phrasing is withdrawn"));
        assert!(!answer.contains("cargo test"), "{answer}");
        assert!(!answer.contains("Took 4.2s"), "{answer}");
        assert!(!answer.contains("↑521k"), "{answer}");
        assert!(!answer.contains('─'), "{answer}");
    }

    #[test]
    fn an_empty_pane_is_refused() {
        let error = last_assistant("%40", "   \n\n").unwrap_err();
        assert_eq!(
            error.downcast_ref::<PaneError>().map(PaneError::kind),
            Some("pane-no-answer")
        );
    }

    #[test]
    fn a_pane_with_only_tool_chrome_is_refused() {
        let error = last_assistant(
            "%40",
            " $ cargo test\n ... (3 earlier lines, ctrl+o to expand)\n Took 2.0s\n",
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<PaneError>().map(PaneError::kind),
            Some("pane-no-answer")
        );
    }

    #[test]
    fn a_bare_path_is_not_an_answer() {
        let error = last_assistant("%40", " src/orch/bridge.rs\n").unwrap_err();
        assert_eq!(
            error.downcast_ref::<PaneError>().map(PaneError::kind),
            Some("pane-not-an-answer")
        );
    }

    #[test]
    fn ansi_escapes_do_not_leak_into_the_reply() {
        let captured = "\u{1b}[32m Anti-echo is fixed.\u{1b}[0m\n";
        assert_eq!(
            last_assistant("%40", captured).unwrap(),
            "Anti-echo is fixed."
        );
    }
}
