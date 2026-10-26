//! Loopback end-to-end proof for the ohmOS messaging bridge.
//!
//! The **real** `Bridge` and the **real** `MsgdClient` drive a tiny HTTP fixture
//! built on `std::net::TcpListener`. Every call stays on `127.0.0.1`; no test
//! dependency and no live daemon is involved.
//!
//! Proven here: one delivery per inbound message, no re-delivery on a second
//! poll, exactly one `POST /send` per reply, `queued:false` is a failure, and
//! dedupe/idempotency survive a bridge restart through the persisted ledger.

use rakazo_beeper::ohm::{
    Bridge, Chat, HttpTransport, Msg, MsgdClient, OhmConfig, OrchestratorNotifier, PaneIo,
    Platform, ReplyReport, Route, SendRequest,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::TempDir;

/// Test-local pane sink. Kept outside the bridge so the integration test can
/// inspect it after the real bridge consumed it.
#[derive(Clone, Default)]
struct RecordingPane {
    injections: Arc<Mutex<Vec<(String, String)>>>,
}

impl RecordingPane {
    fn injected(&self) -> Vec<(String, String)> {
        self.injections.lock().unwrap().clone()
    }
}

impl PaneIo for RecordingPane {
    fn inject(&self, pane: &str, text: &str) -> anyhow::Result<()> {
        self.injections
            .lock()
            .unwrap()
            .push((pane.to_string(), text.to_string()));
        Ok(())
    }
}

/// Test-local notifier sink: the only orchestrator delivery path.
#[derive(Clone, Default)]
struct RecordingNotifier {
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl RecordingNotifier {
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl OrchestratorNotifier for RecordingNotifier {
    fn notify(&self, worker: &str, text: &str) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((worker.to_string(), text.to_string()));
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

#[derive(Default)]
struct FixtureState {
    chats: Vec<Chat>,
    threads: HashMap<i64, Vec<Msg>>,
    requests: Vec<Recorded>,
    queued: bool,
}

/// Minimal blocking HTTP/1.1 fixture. One request per connection, always
/// `Connection: close`, so the `ureq` client can never hang on a reused socket.
struct Fixture {
    base_url: String,
    state: Arc<Mutex<FixtureState>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Fixture {
    fn start(chats: Vec<Chat>, threads: Vec<(i64, Vec<Msg>)>, queued: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        listener
            .set_nonblocking(true)
            .expect("nonblocking fixture listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(FixtureState {
            chats,
            threads: threads.into_iter().collect(),
            requests: Vec::new(),
            queued,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let join = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // BSD may inherit O_NONBLOCK from the listener; the
                        // per-connection reader must block for request bytes.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        serve(stream, &thread_state);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url,
            state,
            stop,
            join: Some(join),
        }
    }

    fn set_queued(&self, queued: bool) {
        self.state.lock().unwrap().queued = queued;
    }

    fn add_message(&self, chat_id: i64, message: Msg) {
        self.state
            .lock()
            .unwrap()
            .threads
            .entry(chat_id)
            .or_default()
            .push(message);
    }

    fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    fn posts(&self, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|request| request.method == "POST" && request.path == path)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn serve(mut stream: TcpStream, state: &Arc<Mutex<FixtureState>>) {
    let Some((method, path, body)) = read_request(&mut stream) else {
        return;
    };
    let (status, content_type, response) = {
        let mut state = state.lock().unwrap();
        state.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            body: body.clone(),
        });
        match (method.as_str(), path.as_str()) {
            ("GET", "/health") => (200, "text/plain", "ok".to_string()),
            ("GET", "/inbox") => (
                200,
                "application/json",
                serde_json::to_string(&state.chats).unwrap(),
            ),
            ("GET", "/events") => (
                200,
                "text/event-stream",
                "data: {\"inbox\":[]}\n\n".to_string(),
            ),
            ("GET", path) if path.starts_with("/thread/") => {
                let id: i64 = path
                    .trim_start_matches("/thread/")
                    .parse()
                    .expect("numeric thread id");
                let thread = state.threads.get(&id).cloned().unwrap_or_default();
                (
                    200,
                    "application/json",
                    serde_json::to_string(&thread).unwrap(),
                )
            }
            ("POST", "/send") => {
                // The fixture accepts the documented wire body and only the
                // `queued` flag in the reply carries success.
                let _: SendRequest = serde_json::from_str(&body).expect("accepted send body");
                (
                    200,
                    "application/json",
                    serde_json::json!({ "queued": state.queued }).to_string(),
                )
            }
            _ => (404, "text/plain", "not found".to_string()),
        }
    };
    let reply = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
        assert!(buffer.len() < 64 * 1024, "oversized fixture request");
    };
    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body =
        String::from_utf8_lossy(&buffer[body_start..body_start + content_length]).to_string();
    Some((method, path, body))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn chat() -> Chat {
    Chat {
        id: 7,
        platform: Platform::WhatsApp,
        remote_id: "remote-1".into(),
        title: "Phone fixture".into(),
        contact_id: None,
        last_ts: 41,
        last_text: Some("hello".into()),
        unread: 1,
    }
}

fn message(id: i64, text: &str) -> Msg {
    Msg {
        id,
        from_me: false,
        sender: "phone".into(),
        timestamp: id,
        text: Some(text.into()),
        media_ref: None,
    }
}

fn config(state_file: Option<PathBuf>, pane: &str) -> OhmConfig {
    OhmConfig {
        base_url: None,
        agents: Vec::new(),
        allow_send: true,
        state_file,
        orchestrator_targets: vec!["%0".into()],
        routes: vec![Route {
            chat_id: 7,
            roster_key: "fleet-orch-v1".into(),
            pane: Some(pane.into()),
            platform: Platform::WhatsApp,
            remote_id: "remote-1".into(),
            recipient: None,
        }],
    }
}

fn client(fixture: &Fixture) -> MsgdClient {
    MsgdClient::new(&fixture.base_url)
        .expect("valid loopback base URL")
        .with_send_allowed(true)
}

// The fixture is intentionally tiny; serialize these in-process to keep the
// one-request-per-connection harness deterministic under cargo's default threads.
static LOOPBACK_LOCK: Mutex<()> = Mutex::new(());

fn sent_bodies(fixture: &Fixture) -> Vec<SendRequest> {
    fixture
        .posts("/send")
        .into_iter()
        .map(|request| serde_json::from_str(&request.body).expect("send body"))
        .collect()
}

#[test]
fn loopback_worker_delivers_once_and_posts_one_reply() {
    let _loopback = LOOPBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::start(vec![chat()], vec![(7, vec![message(41, "hello")])], true);
    let panes = RecordingPane::default();
    let notifier = RecordingNotifier::default();
    let probe = MsgdClient::new(&fixture.base_url).unwrap();
    probe.health().expect("/health is ok on the fixture");
    let mut events = probe.events().expect("/events stream opens");
    let mut event_line = String::new();
    events.read_line(&mut event_line).unwrap();
    assert!(event_line.starts_with("data: "), "{event_line:?}");

    let mut bridge = Bridge::new(
        &config(None, "%1"),
        None,
        HttpTransport::new(client(&fixture)),
        panes.clone(),
        notifier.clone(),
    )
    .unwrap();

    let first = bridge.poll_once().unwrap();
    assert_eq!(first.injected, 1);
    assert_eq!(first.duplicates, 0);
    let injections = panes.injected();
    assert_eq!(injections.len(), 1);
    assert_eq!(injections[0].0, "%1");
    assert!(injections[0].1.contains("hello"), "{:?}", injections[0].1);

    let second = bridge.poll_once().unwrap();
    assert_eq!(second.injected, 0);
    assert_eq!(second.duplicates, 1);
    assert_eq!(panes.injected().len(), 1, "no re-delivery");

    assert_eq!(bridge.reply(7, 41, "answer").unwrap(), ReplyReport::Queued);
    assert_eq!(
        bridge.reply(7, 41, "answer").unwrap(),
        ReplyReport::AlreadySent
    );

    let sends = fixture.posts("/send");
    assert_eq!(sends.len(), 1, "exactly one POST /send per reply");
    let body = sent_bodies(&fixture).remove(0);
    assert_eq!(body.platform, Platform::WhatsApp);
    assert_eq!(body.chat_remote_id, "remote-1");
    assert_eq!(body.recipient, "remote-1");
    assert_eq!(body.text, "answer");
    assert!(
        bridge.state().replies.contains("ohm-reply-7-41"),
        "a queued reply is recorded in the local ledger"
    );

    // The fixture really served the documented routes the bridge used.
    let paths: Vec<_> = fixture
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert!(paths.contains(&"/health".to_string()));
    assert!(paths.contains(&"/inbox".to_string()));
    assert!(paths.contains(&"/thread/7".to_string()));
    assert!(paths.contains(&"/events".to_string()), "{paths:?}");
}

#[test]
fn loopback_queued_false_is_a_failure_not_a_delivery() {
    let _loopback = LOOPBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::start(vec![chat()], vec![(7, vec![message(41, "hello")])], false);
    let panes = RecordingPane::default();
    let notifier = RecordingNotifier::default();
    let mut bridge = Bridge::new(
        &config(None, "%1"),
        None,
        HttpTransport::new(client(&fixture)),
        panes.clone(),
        notifier.clone(),
    )
    .unwrap();
    bridge.poll_once().unwrap();

    let error = bridge.reply(7, 41, "answer").unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("did not queue"), "{error}");
    assert_eq!(fixture.posts("/send").len(), 1);
    assert!(
        bridge.state().replies.is_empty(),
        "a non-queued send is not recorded as sent"
    );

    // A later queued attempt is the first recorded reply; still only one send
    // has ever succeeded.
    fixture.set_queued(true);
    assert_eq!(bridge.reply(7, 41, "answer").unwrap(), ReplyReport::Queued);
    assert_eq!(fixture.posts("/send").len(), 2);
    assert!(bridge.state().replies.contains("ohm-reply-7-41"));
}

#[test]
fn loopback_restart_has_zero_redelivery_and_zero_duplicate_send() {
    let _loopback = LOOPBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let state_file = dir.path().join("ohm-state.json");
    let fixture = Fixture::start(vec![chat()], vec![(7, vec![message(41, "first")])], true);
    let panes1 = RecordingPane::default();
    let notifier1 = RecordingNotifier::default();

    {
        let mut bridge = Bridge::new(
            &config(Some(state_file.clone()), "%1"),
            None,
            HttpTransport::new(client(&fixture)),
            panes1.clone(),
            notifier1.clone(),
        )
        .unwrap();
        assert_eq!(bridge.poll_once().unwrap().injected, 1);
        assert_eq!(
            bridge.reply(7, 41, "answer-41").unwrap(),
            ReplyReport::Queued
        );
        assert_eq!(panes1.injected().len(), 1);
    }
    assert!(state_file.exists(), "the ledger was persisted");
    let panes2 = RecordingPane::default();
    let notifier2 = RecordingNotifier::default();

    let mut restarted = Bridge::new(
        &config(Some(state_file.clone()), "%1"),
        None,
        HttpTransport::new(client(&fixture)),
        panes2.clone(),
        notifier2.clone(),
    )
    .unwrap();

    let replayed = restarted.poll_once().unwrap();
    assert_eq!(replayed.injected, 0, "zero re-delivery after restart");
    assert_eq!(replayed.duplicates, 1);
    assert!(panes2.injected().is_empty());
    assert!(notifier2.calls().is_empty());
    assert_eq!(
        restarted.reply(7, 41, "answer-41").unwrap(),
        ReplyReport::AlreadySent
    );
    assert_eq!(
        fixture.posts("/send").len(),
        1,
        "zero duplicate POST /send after restart"
    );

    // A brand-new message is still delivered exactly once.
    fixture.add_message(7, message(42, "second"));
    let fresh = restarted.poll_once().unwrap();
    assert_eq!(fresh.injected, 1);
    assert_eq!(fresh.duplicates, 1, "old message stays deduped");
    let injections = panes2.injected();
    assert_eq!(injections.len(), 1);
    assert!(injections[0].1.contains("second"));
    assert_eq!(
        restarted.reply(7, 42, "answer-42").unwrap(),
        ReplyReport::Queued
    );
    assert_eq!(
        restarted.reply(7, 42, "answer-42").unwrap(),
        ReplyReport::AlreadySent
    );
    assert_eq!(fixture.posts("/send").len(), 2);
    let bodies = sent_bodies(&fixture);
    assert_eq!(bodies[0].text, "answer-41");
    assert_eq!(bodies[1].text, "answer-42");

    // The persisted schema is exactly the small JSON ledger the bridge reads.
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(state["seen"], serde_json::json!([41, 42]));
    assert_eq!(
        state["replies"],
        serde_json::json!(["ohm-reply-7-41", "ohm-reply-7-42"])
    );
}

#[test]
fn loopback_orchestrator_route_notifies_and_never_pastes_across_restart() {
    let _loopback = LOOPBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().unwrap();
    let state_file = dir.path().join("ohm-state.json");
    let fixture = Fixture::start(vec![chat()], vec![(7, vec![message(41, "first")])], true);
    let panes1 = RecordingPane::default();
    let notifier1 = RecordingNotifier::default();

    {
        let mut bridge = Bridge::new(
            &config(Some(state_file.clone()), "%0"),
            None,
            HttpTransport::new(client(&fixture)),
            panes1.clone(),
            notifier1.clone(),
        )
        .unwrap();
        assert_eq!(bridge.poll_once().unwrap().injected, 1);
        assert!(
            panes1.injected().is_empty(),
            "orchestrator route must never reach a pane"
        );
        let calls = notifier1.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "fleet-orch-v1");
        assert!(calls[0].1.contains("first"));
        assert_eq!(
            bridge.reply(7, 41, "answer-41").unwrap(),
            ReplyReport::Queued
        );
        assert_eq!(fixture.posts("/send").len(), 1);
    }

    let panes2 = RecordingPane::default();
    let notifier2 = RecordingNotifier::default();
    let mut restarted = Bridge::new(
        &config(Some(state_file), "%0"),
        None,
        HttpTransport::new(client(&fixture)),
        panes2.clone(),
        notifier2.clone(),
    )
    .unwrap();

    let replayed = restarted.poll_once().unwrap();
    assert_eq!(replayed.injected, 0, "zero re-delivery after restart");
    assert_eq!(replayed.duplicates, 1);
    assert!(panes2.injected().is_empty());
    assert!(
        notifier2.calls().is_empty(),
        "restart must not re-notify the orchestrator"
    );
    assert_eq!(
        restarted.reply(7, 41, "answer-41").unwrap(),
        ReplyReport::AlreadySent
    );
    assert_eq!(fixture.posts("/send").len(), 1, "zero duplicate send");

    fixture.add_message(7, message(42, "second for orchestrator"));
    let fresh = restarted.poll_once().unwrap();
    assert_eq!(fresh.injected, 1);
    assert!(
        panes2.injected().is_empty(),
        "after restart the orchestrator route is still notifier-only"
    );
    let calls = notifier2.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "fleet-orch-v1");
    assert!(calls[0].1.contains("second for orchestrator"));
    assert_eq!(
        restarted.reply(7, 42, "answer-42").unwrap(),
        ReplyReport::Queued
    );
    assert_eq!(fixture.posts("/send").len(), 2);
    assert_eq!(sent_bodies(&fixture)[1].text, "answer-42");
}
