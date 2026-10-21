//! Offline M0 tests for fleet identity, routing, tapbacks and A2A.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rakazo_beeper::fleet::{
    AnswerLedger, Ask, BotKey, Dedupe, DedupeResult, Entry, InMemoryLedger, Message, RecordOutcome,
    Resolution, Signal, Table, TableError, Verb, tapback, worker_attribution,
};
use sha2::{Digest, Sha256};

fn fixture() -> Table {
    Table::new(vec![
        Entry::routed(BotKey::Orch, "conv-orch", "pane-orch", "orchestrator")
            .with_bot_id("fleet-orch-v1"),
        Entry::routed(
            BotKey::pm("rakazo"),
            "conv-rakazo",
            "pane-rakazo",
            "pm:rakazo",
        )
        .with_bot_id("fleet-pm-rakazo-v1")
        .with_parent(BotKey::Orch),
        Entry::new(
            BotKey::pm("transmog"),
            Some("conv-transmog".to_string()),
            Some("pane-transmog".to_string()),
            Some("pm:transmog".to_string()),
        )
        .with_bot_id("fleet-pm-transmog-v1")
        .with_parent(BotKey::Orch)
        .with_paused(true),
        Entry::worker("g2-m0-routing", "pm:rakazo"),
    ])
    .expect("fixture table is structurally valid")
}

// M0 item 1: conversation -> pane and pane -> conversation, with explicit
// unresolvable results rather than a silent orch fallback.
#[test]
fn route_resolves_orch_and_pm_without_implicit_orch_fallback() {
    let table = fixture();

    assert_eq!(
        table.resolve("conv-orch"),
        Resolution::Routed {
            bot: BotKey::Orch,
            pane: "pane-orch".to_string(),
        }
    );
    assert_eq!(
        table.resolve("conv-rakazo"),
        Resolution::Routed {
            bot: BotKey::pm("rakazo"),
            pane: "pane-rakazo".to_string(),
        }
    );
    assert_eq!(table.target("pane-orch"), Some(BotKey::Orch));
    assert_eq!(table.target("pane-rakazo"), Some(BotKey::pm("rakazo")));
    assert_eq!(table.target("pane-missing"), None);

    match table.resolve("conv-does-not-exist") {
        Resolution::Unrouted { reason } => {
            assert!(reason.contains("unknown conversation"), "{reason}");
            assert!(
                !reason.contains("orch"),
                "must not fall back to orch: {reason}"
            );
        }
        other => panic!("expected Unrouted, got {other:?}"),
    }
}

#[test]
fn route_paused_transmog_is_present_but_not_routable() {
    let table = fixture();
    assert_eq!(
        table.entry(&BotKey::pm("transmog")).map(Entry::is_paused),
        Some(true)
    );
    match table.resolve("conv-transmog") {
        Resolution::Unrouted { reason } => assert!(reason.contains("paused"), "{reason}"),
        other => panic!("paused transmog must be Unrouted, got {other:?}"),
    }
    assert_eq!(table.target("pane-transmog"), None);
}

#[test]
fn route_worker_has_no_conversation_or_bot_identity() {
    let entry = fixture()
        .entry(&BotKey::worker("g2-m0-routing"))
        .cloned()
        .expect("worker row exists");
    assert_eq!(entry.conversation_id(), None);
    assert_eq!(entry.bot_id(), None);
    assert_eq!(entry.pane_id(), None);
    assert_eq!(entry.notify_target(), Some("pm:rakazo"));

    let with_conversation = Entry::new(
        BotKey::worker("bad"),
        Some("conv".to_string()),
        None,
        Some("pm:rakazo".to_string()),
    );
    assert_eq!(
        Table::new(vec![with_conversation]).unwrap_err(),
        TableError::WorkerHasConversation(BotKey::worker("bad"))
    );

    let with_bot_id = Entry::worker("bad", "pm:rakazo").with_bot_id("bot-created-by-accident");
    assert_eq!(
        Table::new(vec![with_bot_id]).unwrap_err(),
        TableError::WorkerHasBotId(BotKey::worker("bad"))
    );
}

#[test]
fn route_structural_rules_and_durable_pane_refresh() {
    let missing_parent = Entry::routed(BotKey::pm("fpl"), "conv-fpl", "pane-fpl", "pm:fpl");
    assert_eq!(
        Table::new(vec![missing_parent]).unwrap_err(),
        TableError::PmWithoutOrchParent(BotKey::pm("fpl"))
    );

    let mut table = fixture();
    table
        .with_pane_updated(&BotKey::Orch, "pane-refreshed")
        .expect("known key updates");
    assert_eq!(
        table.target("pane-orch"),
        None,
        "old pane id must be removed"
    );
    assert_eq!(table.target("pane-refreshed"), Some(BotKey::Orch));
    assert_eq!(
        table.resolve("conv-orch"),
        Resolution::Routed {
            bot: BotKey::Orch,
            pane: "pane-refreshed".to_string(),
        }
    );

    assert_eq!(
        table
            .with_pane_updated(&BotKey::node("ghost"), "pane-ghost")
            .unwrap_err(),
        TableError::UnknownKey(BotKey::node("ghost"))
    );
}

// M0 item 4: T2 workers render as an attribution in the PM conversation and
// never become a bot or a conversation of their own.
#[test]
fn route_t2_worker_renders_as_attribution_only() {
    let table = fixture();
    let worker = BotKey::worker("g2-m0-routing");
    assert!(table.contains_key(&worker));
    assert_eq!(worker.as_key(), "worker:g2-m0-routing");
    assert_eq!(
        worker_attribution("g2-m0-routing", "DONE g2-m0-routing"),
        "worker:g2-m0-routing: DONE g2-m0-routing"
    );
    assert_eq!(table.entry(&worker).unwrap().conversation_id(), None);
    assert_eq!(table.entry(&worker).unwrap().bot_id(), None);
}

// M0 item 2: tapback state machine.
#[test]
fn tapback_ask_layout_and_signal_table() {
    let ask = Ask {
        id: "esc-1".to_string(),
        question: "Ship the M0 routing proof?".to_string(),
        options: vec!["yes".to_string(), "wait".to_string(), "no".to_string()],
        recommend: 1,
    };
    assert_eq!(
        ask.render(),
        "ASK esc-1 — Ship the M0 routing proof?\n1) yes\n2) wait\n3) no\nrecommend: 1"
    );

    assert_eq!(tapback::parse("👍"), Some(Signal::Yes));
    assert_eq!(tapback::parse("✅"), Some(Signal::Yes));
    assert_eq!(tapback::parse("👎"), Some(Signal::No));
    assert_eq!(tapback::parse("❌"), Some(Signal::No));
    assert_eq!(tapback::parse("1️⃣"), Some(Signal::Option(1)));
    assert_eq!(tapback::parse("2"), Some(Signal::Option(2)));
    assert_eq!(tapback::parse("❓"), Some(Signal::MoreInfo));
    assert_eq!(tapback::parse("?"), Some(Signal::MoreInfo));
    assert_eq!(tapback::parse("🕐"), Some(Signal::Later));
    assert_eq!(tapback::parse("later"), Some(Signal::Later));
    assert_eq!(tapback::parse("not-a-signal"), None);
}

#[test]
fn tapback_recommendation_and_option_selection() {
    let ask = Ask {
        id: "esc-2".to_string(),
        question: "Pick one".to_string(),
        options: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        recommend: 2,
    };
    let mut ledger = InMemoryLedger::with_asks([ask]);

    assert_eq!(
        ledger.record("esc-2", "msg-1", Signal::Yes),
        RecordOutcome::Recorded(Verb::Yes)
    );
    assert_eq!(ledger.answer("esc-2"), Some(Verb::Yes));

    let mut ledger = InMemoryLedger::with_asks([Ask {
        id: "esc-3".to_string(),
        question: "Pick one".to_string(),
        options: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        recommend: 2,
    }]);
    assert_eq!(
        ledger.record("esc-3", "msg-1", Signal::Option(2)),
        RecordOutcome::Recorded(Verb::Option(2))
    );
    assert_eq!(ledger.answer("esc-3"), Some(Verb::Option(2)));
}

#[test]
fn tapback_duplicate_delivery_records_once_and_second_answer_is_refused() {
    let ask = Ask {
        id: "esc-4".to_string(),
        question: "Pick one".to_string(),
        options: vec!["a".to_string(), "b".to_string()],
        recommend: 1,
    };
    let mut ledger = InMemoryLedger::with_asks([ask]);

    assert_eq!(
        ledger.record("esc-4", "msg-1", Signal::Yes),
        RecordOutcome::Recorded(Verb::Yes)
    );
    // Same webhook redelivery: no second answer.
    assert_eq!(
        ledger.record("esc-4", "msg-1", Signal::Yes),
        RecordOutcome::AlreadyAnswered(Verb::Yes)
    );
    // A different message id on an answered ASK is also refused.
    assert_eq!(
        ledger.record("esc-4", "msg-2", Signal::No),
        RecordOutcome::AlreadyAnswered(Verb::Yes)
    );
    assert_eq!(ledger.answer("esc-4"), Some(Verb::Yes));
}

#[test]
fn tapback_defer_is_not_terminal_and_invalid_option_is_rejected() {
    let ask = Ask {
        id: "esc-5".to_string(),
        question: "Pick one".to_string(),
        options: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        recommend: 1,
    };
    let mut ledger = InMemoryLedger::with_asks([ask]);

    assert_eq!(
        ledger.record("esc-5", "msg-1", Signal::Later),
        RecordOutcome::Recorded(Verb::Later)
    );
    assert_eq!(ledger.answer("esc-5"), None, "defer stays open");
    assert_eq!(
        ledger.record("esc-5", "msg-1", Signal::Later),
        RecordOutcome::AlreadyAnswered(Verb::Later),
        "same message is idempotent even when deferred"
    );
    assert_eq!(
        ledger.record("esc-5", "msg-2", Signal::Option(4)),
        RecordOutcome::Rejected("no option 4".to_string())
    );
    assert_eq!(ledger.answer("esc-5"), None, "rejection records no answer");
    assert_eq!(
        ledger.record("esc-5", "msg-3", Signal::Option(2)),
        RecordOutcome::Recorded(Verb::Option(2))
    );
    assert_eq!(ledger.answer("esc-5"), Some(Verb::Option(2)));
}

// M0 item 3: A2A schema, validation, dedupe and no echo.
#[test]
fn a2a_message_serde_round_trip_keeps_ref_field() {
    let message = Message {
        id: "a2a-1".to_string(),
        from: "pm:fpl".to_string(),
        to: "pm:rakazo".to_string(),
        kind: "need".to_string(),
        r#ref: "esc-9".to_string(),
        ts: "2026-10-21T12:00:00Z".to_string(),
        body: "need a routing decision".to_string(),
        options: vec!["yes".to_string(), "no".to_string()],
        reply_to: None,
    };

    let json = serde_json::to_string(&message).unwrap();
    assert!(json.contains("\"ref\":\"esc-9\""), "{json}");
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(message, back);

    let minimal: Message = serde_json::from_str(
        r#"{"id":"a2a-2","from":"pm:fpl","to":"pm:rakazo","kind":"answer","ref":"esc-9","ts":"2026-10-21T12:01:00Z","body":"ok"}"#,
    )
    .unwrap();
    assert!(minimal.options.is_empty());
    assert_eq!(minimal.reply_to, None);
}

#[test]
fn a2a_validation_rejects_unknown_and_empty_fields() {
    let table = fixture();
    let mut message = Message {
        id: "a2a-1".to_string(),
        from: "pm:fpl".to_string(),
        to: "pm:rakazo".to_string(),
        kind: "need".to_string(),
        r#ref: "esc-9".to_string(),
        ts: "2026-10-21T12:00:00Z".to_string(),
        body: "hello".to_string(),
        options: vec![],
        reply_to: None,
    };

    assert!(
        message.validate(&table).is_err(),
        "unknown from is rejected"
    );
    message.from = "pm:rakazo".to_string();
    message.to = "pm:ghost".to_string();
    assert!(message.validate(&table).is_err(), "unknown to is rejected");

    message.to = "pm:rakazo".to_string();
    message.id.clear();
    assert!(message.validate(&table).is_err(), "empty id is rejected");
    message.id = "a2a-1".to_string();
    message.body.clear();
    assert!(message.validate(&table).is_err(), "empty body is rejected");
}

#[test]
fn a2a_dedupe_suppresses_second_copy_and_never_echoes() {
    let mut dedupe = Dedupe::new();
    let message = Message {
        id: "a2a-1".to_string(),
        from: "pm:rakazo".to_string(),
        to: "pm:rakazo".to_string(),
        kind: "need".to_string(),
        r#ref: "esc-9".to_string(),
        ts: "2026-10-21T12:00:00Z".to_string(),
        body: "same body".to_string(),
        options: vec![],
        reply_to: None,
    };
    assert_eq!(dedupe.accept(&message), DedupeResult::Echo);

    let mut message = message;
    message.to = "orch".to_string();
    assert_eq!(dedupe.accept(&message), DedupeResult::New);
    assert_eq!(dedupe.accept(&message), DedupeResult::Duplicate);

    let mut redelivered = message.clone();
    redelivered.id = "a2a-rewritten-id".to_string();
    assert_eq!(
        dedupe.accept(&redelivered),
        DedupeResult::Duplicate,
        "message id is not part of the content key"
    );

    let mut different = message.clone();
    different.body = "a different body".to_string();
    assert_eq!(dedupe.accept(&different), DedupeResult::New);

    assert_eq!(message.render(), "[from pm:rakazo] same body");
}

#[test]
fn a2a_dedupe_window_expires() {
    let mut dedupe = Dedupe::with_window(Duration::from_millis(1));
    let message = Message {
        id: "a2a-window".to_string(),
        from: "pm:rakazo".to_string(),
        to: "orch".to_string(),
        kind: "answer".to_string(),
        r#ref: "esc-9".to_string(),
        ts: "2026-10-21T12:00:00Z".to_string(),
        body: "window".to_string(),
        options: vec![],
        reply_to: None,
    };
    assert_eq!(dedupe.accept(&message), DedupeResult::New);
    let deadline = Instant::now() + Duration::from_millis(5);
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
    assert_eq!(dedupe.accept(&message), DedupeResult::New);
}

// M0 item 5: selftest isolation. The test owns temp roots and asserts the real
// ledgers are untouched; if the real env vars are not set, say so and skip
// that comparison rather than pretending it ran.
#[test]
fn selftest_isolation_keeps_real_ledgers_byte_identical() {
    let vars = ["PI_PM_ROOT", "PI_ESCALATE_LEDGER"];
    let before: Vec<(String, Option<PathBuf>, Option<String>)> = vars
        .iter()
        .map(|name| {
            let path = std::env::var_os(name).map(PathBuf::from);
            let hash = path.as_deref().and_then(snapshot);
            ((*name).to_string(), path, hash)
        })
        .collect();
    let old: Vec<(String, Option<OsString>)> = vars
        .iter()
        .map(|name| ((*name).to_string(), std::env::var_os(name)))
        .collect();

    let temp = tempfile::tempdir().expect("tempdir");
    let isolated_pm = temp.path().join("pm");
    let isolated_ledger = temp.path().join("escalate-ledger.jsonl");
    fs::create_dir_all(&isolated_pm).unwrap();

    // SAFETY: this test is the only one in the suite that reads or writes these
    // variables, and each one is restored before the test returns.
    unsafe {
        std::env::set_var("PI_PM_ROOT", &isolated_pm);
        std::env::set_var("PI_ESCALATE_LEDGER", &isolated_ledger);
    }

    let mut ledger = InMemoryLedger::with_asks([Ask {
        id: "selftest-ask".to_string(),
        question: "isolated?".to_string(),
        options: vec!["yes".to_string()],
        recommend: 1,
    }]);
    assert_eq!(
        ledger.record("selftest-ask", "selftest-message", Signal::Yes),
        RecordOutcome::Recorded(Verb::Yes)
    );
    // A test double writes only under its temp root.
    fs::write(isolated_pm.join("state.json"), b"{}").unwrap();
    fs::write(&isolated_ledger, b"answer:yes\n").unwrap();

    for (name, path, before_hash) in &before {
        let after_hash = path.as_deref().and_then(snapshot);
        assert_eq!(
            before_hash, &after_hash,
            "{name} changed during an isolated selftest ({path:?})"
        );
    }

    let skipped = before.iter().all(|(_, _, hash)| hash.is_none());
    if skipped {
        eprintln!(
            "selftest-isolation: PI_PM_ROOT/PI_ESCALATE_LEDGER are unset; \
             real-path byte comparison skipped (isolated temp roots were still exercised)"
        );
    }

    // SAFETY: restore the process environment exactly.
    unsafe {
        for (name, value) in old {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn snapshot(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let mut hash = Sha256::new();
    if path.is_dir() {
        walk(path, path, &mut hash).ok()?;
    } else {
        let bytes = fs::read(path).ok()?;
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(&bytes);
    }
    Some(hex::encode(hash.finalize()))
}

fn walk(root: &Path, dir: &Path, hash: &mut Sha256) -> std::io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(root, &path, hash)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            hash.update(relative.to_string_lossy().as_bytes());
            let bytes = fs::read(&path)?;
            hash.update((bytes.len() as u64).to_be_bytes());
            hash.update(&bytes);
        }
    }
    Ok(())
}
