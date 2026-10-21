//! ASK rendering and the tapback answer state machine.
//!
//! A tapback is an answer to one ASK message. The ledger, not the transcript,
//! is the authority: duplicate webhook deliveries and restarts must record the
//! first answer exactly once. Deferrals are visible but deliberately do not
//! close the ASK.

use std::collections::HashMap;
use std::fmt;

/// One message with explicit options and a recommended choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub recommend: usize,
}

impl Ask {
    pub fn new(
        id: impl Into<String>,
        question: impl Into<String>,
        options: Vec<String>,
        recommend: usize,
    ) -> Self {
        Self {
            id: id.into(),
            question: question.into(),
            options,
            recommend,
        }
    }

    /// The ASK layout. The brief's `Ask` has no separate rationale field, so
    /// the plain form stops at the recommendation.
    pub fn render(&self) -> String {
        self.render_with_reason(None)
    }

    /// The design's `recommend: 2 — <why>` form, when the caller has a reason.
    pub fn render_with_reason(&self, reason: Option<&str>) -> String {
        let mut out = format!("ASK {} — {}\n", self.id, self.question);
        for (index, option) in self.options.iter().enumerate() {
            out.push_str(&format!("{}) {}\n", index + 1, option));
        }
        out.push_str(&format!("recommend: {}", self.recommend));
        if let Some(reason) = reason {
            out.push_str(" — ");
            out.push_str(reason);
        }
        out
    }
}

/// A normalized human tapback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Signal {
    Yes,
    No,
    Option(usize),
    MoreInfo,
    Later,
}

/// Parse a tapback reaction or its text equivalent.
///
/// Accepted aliases: 👍/✅, 👎/❌, 1️⃣/"1"…, ❓/"?", 🕐/"later".
pub fn parse(signal_or_text: &str) -> Option<Signal> {
    let text = signal_or_text.trim();
    match text {
        "👍" | "✅" | "yes" | "YES" => return Some(Signal::Yes),
        "👎" | "❌" | "no" | "NO" => return Some(Signal::No),
        "❓" | "?" => return Some(Signal::MoreInfo),
        "🕐" | "later" | "LATER" => return Some(Signal::Later),
        _ => {}
    }

    // Keycap emoji are the digit followed by VS16 and/or U+20E3.
    let digits = text.trim_end_matches(|c| c == '\u{fe0f}' || c == '\u{20e3}');
    let mut chars = digits.chars();
    let digit = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let option = digit.to_digit(10)? as usize;
    (option > 0).then_some(Signal::Option(option))
}

/// The verb written to the ledger/history for a parsed signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verb {
    Yes,
    No,
    Option(usize),
    MoreInfo,
    Later,
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yes => f.write_str("answer:yes"),
            Self::No => f.write_str("answer:no"),
            Self::Option(option) => write!(f, "answer:{option}"),
            Self::MoreInfo => f.write_str("defer:more-info"),
            Self::Later => f.write_str("defer"),
        }
    }
}

impl Verb {
    pub fn as_str(&self) -> String {
        self.to_string()
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::MoreInfo | Self::Later)
    }
}

impl From<Signal> for Verb {
    fn from(signal: Signal) -> Self {
        match signal {
            Signal::Yes => Self::Yes,
            Signal::No => Self::No,
            Signal::Option(option) => Self::Option(option),
            Signal::MoreInfo => Self::MoreInfo,
            Signal::Later => Self::Later,
        }
    }
}

/// The result of recording one tapback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    /// First delivery of this answer.
    Recorded(Verb),
    /// The ASK (or this message) was already answered; the first answer wins.
    AlreadyAnswered(Verb),
    /// The signal was malformed for this ASK; nothing was recorded.
    Rejected(String),
}

/// Durable answer storage. The in-memory implementation below is the M0 test
/// double; production wiring will back the same operations with the ledger.
pub trait AnswerLedger {
    /// Register an ASK so option indexes can be validated.
    fn register(&mut self, ask: &Ask);

    fn record(&mut self, ask_id: &str, message_id: &str, signal: Signal) -> RecordOutcome;

    fn answer(&self, ask_id: &str) -> Option<Verb>;
}

/// In-memory ledger for tests and the offline M0 proof.
#[derive(Clone, Debug, Default)]
pub struct InMemoryLedger {
    asks: HashMap<String, Ask>,
    /// `(ask_id, message_id)` -> first recorded verb, including deferrals.
    seen: HashMap<(String, String), Verb>,
    /// Terminal answer per ask. A terminal answer never changes.
    answers: HashMap<String, Verb>,
}

impl InMemoryLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_asks(asks: impl IntoIterator<Item = Ask>) -> Self {
        let mut ledger = Self::new();
        for ask in asks {
            ledger.register(&ask);
        }
        ledger
    }

    pub fn recorded(&self, ask_id: &str) -> Option<Verb> {
        self.answers.get(ask_id).copied()
    }
}

impl AnswerLedger for InMemoryLedger {
    fn register(&mut self, ask: &Ask) {
        self.asks.insert(ask.id.clone(), ask.clone());
    }

    fn record(&mut self, ask_id: &str, message_id: &str, signal: Signal) -> RecordOutcome {
        let verb = Verb::from(signal);
        if let Verb::Option(option) = verb
            && let Some(ask) = self.asks.get(ask_id)
            && (option == 0 || option > ask.options.len())
        {
            return RecordOutcome::Rejected(format!("no option {option}"));
        }

        if let Some(existing) = self.answers.get(ask_id) {
            return RecordOutcome::AlreadyAnswered(*existing);
        }

        let key = (ask_id.to_string(), message_id.to_string());
        if let Some(existing) = self.seen.get(&key) {
            return RecordOutcome::AlreadyAnswered(*existing);
        }

        self.seen.insert(key, verb);
        if verb.is_terminal() {
            self.answers.insert(ask_id.to_string(), verb);
        }
        RecordOutcome::Recorded(verb)
    }

    fn answer(&self, ask_id: &str) -> Option<Verb> {
        self.answers.get(ask_id).copied()
    }
}
