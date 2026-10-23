//! Blocking client for the ohmOS `ohm-msgd` HTTP daemon.
//!
//! The daemon's contract is `crates/ohmos-msg-host/src/main.rs` (routes) and
//! `crates/ohmos-msg-host/src/wire.rs` (bodies) at ohmOS rev `479e7f0`. We keep
//! the HTTP layer behind [`HttpExchange`] so the client is testable with a fake
//! daemon and no network.

use super::transport::SendOutcome;
use super::wire::{Chat, Msg, SendRequest, SendResponse};
use anyhow::{Context, Result, bail, ensure};
use std::{io::BufRead, time::Duration};
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(10);

/// The two HTTP shapes the daemon needs: JSON request/response and an SSE line
/// stream. A fake implementation is the offline daemon in the tests.
pub trait HttpExchange {
    fn get_json(&self, url: &str) -> Result<String, String>;
    fn post_json(&self, url: &str, body: &str) -> Result<String, String>;
    fn get_stream(&self, url: &str) -> Result<Box<dyn BufRead + Send>, String>;
}

/// Production `ureq` exchange: 10 s timeout, zero redirects.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(TIMEOUT)
                .redirects(0)
                .build(),
        }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpExchange for UreqHttp {
    fn get_json(&self, url: &str) -> Result<String, String> {
        self.agent
            .get(url)
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())
    }

    fn post_json(&self, url: &str, body: &str) -> Result<String, String> {
        self.agent
            .post(url)
            .set("content-type", "application/json")
            .send_string(body)
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())
    }

    fn get_stream(&self, url: &str) -> Result<Box<dyn BufRead + Send>, String> {
        let reader = self
            .agent
            .get(url)
            .call()
            .map_err(|e| e.to_string())?
            .into_reader();
        Ok(Box::new(std::io::BufReader::new(reader)))
    }
}

/// A client for one daemon instance. `allow_send` defaults to `false`; a queued
/// reply is reported as [`SendOutcome::Queued`], never as delivered.
pub struct MsgdClient<H = UreqHttp> {
    base: String,
    http: H,
    allow_send: bool,
}

impl MsgdClient<UreqHttp> {
    pub fn new(base_url: &str) -> Result<Self> {
        Self::validate_base(base_url)?;
        Ok(Self {
            base: base_url.trim_end_matches('/').to_string(),
            http: UreqHttp::new(),
            allow_send: false,
        })
    }
}

impl<H: HttpExchange> MsgdClient<H> {
    pub fn validate_base(base_url: &str) -> Result<()> {
        let url = Url::parse(base_url).context("invalid ohm daemon base URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "ohm daemon base URL must be http or https"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "ohm daemon base URL must not contain credentials, query, or fragment"
        );
        ensure!(
            url.path() == "/" || url.path().is_empty(),
            "ohm daemon base URL must not contain a path"
        );
        Ok(())
    }

    pub fn with_http(base_url: &str, http: H) -> Result<Self> {
        Self::validate_base(base_url)?;
        Ok(Self {
            base: base_url.trim_end_matches('/').to_string(),
            http,
            allow_send: false,
        })
    }

    /// Explicit opt-in to `POST /send`.
    pub fn with_send_allowed(mut self, allowed: bool) -> Self {
        self.allow_send = allowed;
        self
    }

    pub fn send_allowed(&self) -> bool {
        self.allow_send
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// `GET /health`: the daemon answers plain `ok`.
    pub fn health(&self) -> Result<()> {
        let body = self
            .http
            .get_json(&self.url("/health"))
            .map_err(anyhow::Error::msg)
            .context("ohm daemon health request failed")?;
        ensure!(body.trim() == "ok", "ohm daemon health is not ok");
        Ok(())
    }

    /// `GET /inbox`, as `Vec<Chat>` (most-recently-active first).
    pub fn inbox(&self) -> Result<Vec<Chat>> {
        let body = self
            .http
            .get_json(&self.url("/inbox"))
            .map_err(anyhow::Error::msg)
            .context("ohm daemon inbox request failed")?;
        serde_json::from_str(&body).context("invalid ohm daemon inbox JSON")
    }

    /// `GET /thread/:id`, oldest first.
    pub fn thread(&self, id: i64) -> Result<Vec<Msg>> {
        let body = self
            .http
            .get_json(&self.url(&format!("/thread/{id}")))
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("ohm daemon thread {id} request failed"))?;
        serde_json::from_str(&body).with_context(|| format!("invalid ohm daemon thread {id} JSON"))
    }

    /// `POST /send`. Refused unless `allow_send` was explicitly enabled.
    ///
    /// The daemon wire has no idempotency field, so the durable dedupe key lives
    /// in [`super::bridge::Bridge`], not in this body.
    pub fn send(
        &self,
        platform: super::wire::Platform,
        chat_remote_id: &str,
        recipient: &str,
        text: &str,
    ) -> Result<SendOutcome> {
        if !self.allow_send {
            bail!("ohm send is disabled (allow_send = false)");
        }
        ensure!(!text.trim().is_empty(), "refusing to send empty text");
        ensure!(
            !chat_remote_id.is_empty(),
            "chat_remote_id must not be empty"
        );
        ensure!(!recipient.is_empty(), "recipient must not be empty");
        let body = serde_json::to_string(&SendRequest {
            platform,
            chat_remote_id: chat_remote_id.to_string(),
            recipient: recipient.to_string(),
            text: text.to_string(),
        })?;
        let reply = self
            .http
            .post_json(&self.url("/send"), &body)
            .map_err(anyhow::Error::msg)
            .context("ohm daemon send request failed")?;
        let response: SendResponse =
            serde_json::from_str(&reply).context("invalid ohm daemon send JSON")?;
        if !response.queued {
            bail!("ohm daemon did not queue the reply");
        }
        // `queued` means accepted into an outbox. It is NOT delivery proof.
        Ok(SendOutcome::Queued)
    }

    /// `GET /events`: an SSE line reader (the daemon emits inbox JSON as data).
    pub fn events(&self) -> Result<Box<dyn BufRead + Send>> {
        self.http
            .get_stream(&self.url("/events"))
            .map_err(anyhow::Error::msg)
            .context("ohm daemon events request failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ohm::wire::Platform;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct FakeHttp {
        requests: Arc<Mutex<Vec<(String, String)>>>,
        replies: Arc<Mutex<VecDeque<(String, String)>>>,
    }

    impl FakeHttp {
        fn scripted(replies: &[(&str, &str)]) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                replies: Arc::new(Mutex::new(
                    replies
                        .iter()
                        .map(|(k, v)| ((*k).into(), (*v).into()))
                        .collect(),
                )),
            }
        }
        fn take(&self, path: &str, body: &str) -> Result<String, String> {
            self.requests
                .lock()
                .unwrap()
                .push((path.to_string(), body.to_string()));
            let mut replies = self.replies.lock().unwrap();
            let (want, reply) = replies.pop_front().expect("fake daemon script exhausted");
            assert!(path.ends_with(&want), "wanted {want}, got {path}");
            Ok(reply)
        }
    }

    impl HttpExchange for FakeHttp {
        fn get_json(&self, url: &str) -> Result<String, String> {
            let path = Url::parse(url).unwrap().path().to_string();
            self.take(&path, "")
        }
        fn post_json(&self, url: &str, body: &str) -> Result<String, String> {
            let path = Url::parse(url).unwrap().path().to_string();
            self.take(&path, body)
        }
        fn get_stream(&self, url: &str) -> Result<Box<dyn BufRead + Send>, String> {
            let path = Url::parse(url).unwrap().path().to_string();
            let body = self.take(&path, "")?;
            Ok(Box::new(std::io::Cursor::new(body.into_bytes())))
        }
    }

    fn client(http: FakeHttp) -> MsgdClient<FakeHttp> {
        MsgdClient::with_http("http://127.0.0.1:8140", http).unwrap()
    }

    #[test]
    fn fake_daemon_inbox_thread_and_send() {
        let chat = r#"[{"id":7,"platform":"WhatsApp","remote_id":"r","title":"T","contact_id":null,"last_ts":1,"last_text":"hi","unread":1}]"#;
        let thread = r#"[{"id":41,"from_me":false,"sender":"A","timestamp":2,"text":"hello","media_ref":null}]"#;
        let http = FakeHttp::scripted(&[
            ("/health", "ok"),
            ("/inbox", chat),
            ("/thread/7", thread),
            ("/send", r#"{"queued":true}"#),
            ("/events", "data: []\n\n"),
        ]);
        let client = client(http.clone()).with_send_allowed(true);
        client.health().unwrap();
        let inbox = client.inbox().unwrap();
        assert_eq!(inbox[0].id, 7);
        let msgs = client.thread(7).unwrap();
        assert_eq!(msgs[0].text.as_deref(), Some("hello"));
        assert_eq!(
            client.send(Platform::WhatsApp, "r", "r", "reply").unwrap(),
            SendOutcome::Queued
        );
        assert!(client.events().unwrap().lines().next().is_some());
        let sent = http.requests.lock().unwrap().clone();
        let body = &sent.iter().find(|(p, _)| p == "/send").unwrap().1;
        assert!(body.contains("\"chat_remote_id\":\"r\""), "{body}");
    }

    #[test]
    fn send_is_refused_when_flag_is_off() {
        let http = FakeHttp::scripted(&[]);
        let client = client(http.clone());
        let err = client
            .send(Platform::Signal, "r", "r", "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("disabled"), "{err}");
        assert!(http.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn queued_stays_queued_and_not_queued_is_an_error() {
        let http = FakeHttp::scripted(&[("/send", r#"{"queued":true}"#)]);
        let out = client(http).with_send_allowed(true);
        assert_eq!(
            out.send(Platform::Sms, "r", "r", "x").unwrap(),
            SendOutcome::Queued
        );
        let http = FakeHttp::scripted(&[("/send", r#"{"queued":false}"#)]);
        let out = client(http).with_send_allowed(true);
        assert!(out.send(Platform::Sms, "r", "r", "x").is_err());
    }
}
