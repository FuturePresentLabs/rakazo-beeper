use crate::{
    config::{RegistrationBundle, Tenant},
    stable_id,
};
use anyhow::{Context, Result, ensure};
use reqwest::{
    Client, Method, StatusCode,
    header::{AUTHORIZATION, HeaderValue},
};
use serde_json::{Value, json};
use url::Url;

#[derive(Clone)]
pub struct Matrix {
    client: Client,
    base: Url,
    token: HeaderValue,
    pub domain: String,
    pub sender: String,
    pub prefix: String,
    namespaces: Vec<regex::Regex>,
}

impl Matrix {
    pub fn new(config: &Tenant, bundle: &RegistrationBundle) -> Result<Self> {
        let registration = &bundle.registration;
        let mut token = HeaderValue::from_str(&format!("Bearer {}", registration.as_token))?;
        token.set_sensitive(true);
        let sender = format!(
            "@{}:{}",
            registration.sender_localpart, bundle.homeserver_domain
        );
        let prefix = registration
            .sender_localpart
            .strip_suffix("bot")
            .context("Beeper sender_localpart must end in bot")?
            .to_string();
        let namespaces = registration
            .namespaces
            .users
            .iter()
            .filter(|ns| ns.exclusive)
            .map(|ns| regex::Regex::new(&format!("^(?:{})$", ns.regex)))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(!namespaces.is_empty(), "exclusive user namespace required");
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base: config.homeserver.clone(),
            token,
            domain: bundle.homeserver_domain.clone(),
            sender,
            prefix,
            namespaces,
        })
    }

    pub fn ghost(&self, tenant: &str, bot: &str) -> Result<String> {
        let user = format!(
            "@{}_{}:{}",
            self.prefix,
            stable_id(&[tenant, bot]),
            self.domain
        );
        ensure!(
            self.namespaces.iter().any(|ns| ns.is_match(&user)),
            "ghost is outside registered namespace"
        );
        Ok(user)
    }

    fn url(&self, path: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Matrix base URL"))?
            .pop_if_empty()
            .extend(["_matrix", "client", "v3"])
            .extend(path);
        Ok(url)
    }

    /// @implements url:https://spec.matrix.org/v1.16/application-service-api/ §Application service API -- user impersonation
    async fn request(
        &self,
        method: Method,
        path: &[&str],
        user: Option<&str>,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut url = self.url(path)?;
        if let Some(user) = user {
            url.query_pairs_mut().append_pair("user_id", user);
        }
        let mut request = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, self.token.clone());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Matrix transport failed: {}", e.without_url()))?;
        let status = response.status();
        let body: Value = response.json().await.context("invalid Matrix response")?;
        Ok((status, body))
    }
    async fn ok(
        &self,
        method: Method,
        path: &[&str],
        user: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let (status, body) = self.request(method, path, Some(user), body).await?;
        ensure!(status.is_success(), "Matrix request returned HTTP {status}");
        Ok(body)
    }
    pub async fn register(&self, user: &str, name: &str) -> Result<()> {
        let localpart = user
            .strip_prefix('@')
            .and_then(|s| s.split_once(':'))
            .context("invalid ghost")?
            .0;
        let (status,body) = self.request(Method::POST,&["register"],None,Some(json!({"type":"m.login.application_service","username":localpart,"inhibit_login":true}))).await?;
        ensure!(
            status.is_success()
                || (status == StatusCode::BAD_REQUEST && body["errcode"] == "M_USER_IN_USE"),
            "Matrix ghost registration returned HTTP {status}"
        );
        self.ok(
            Method::PUT,
            &["profile", user, "displayname"],
            user,
            Some(json!({"displayname":name})),
        )
        .await?;
        Ok(())
    }
    pub async fn room(
        &self,
        tenant: &str,
        bot: &str,
        name: &str,
        ghost: &str,
        owner: &str,
    ) -> Result<String> {
        // Hungryserv does not implement aliases. Recover a lost createRoom response
        // through the dedicated ghost's joined rooms and our unique bridge metadata.
        let key = format!("rakazo://{tenant}/{bot}");
        let joined = self.ok(Method::GET, &["joined_rooms"], ghost, None).await?;
        let rooms = joined["joined_rooms"]
            .as_array()
            .context("Matrix joined_rooms missing array")?;
        let mut matches = Vec::new();
        for room in rooms {
            let room = room.as_str().context("invalid joined room id")?;
            let state = self
                .ok(Method::GET, &["rooms", room, "state"], ghost, None)
                .await?;
            if state
                .as_array()
                .context("invalid room state")?
                .iter()
                .any(|event| {
                    event["type"] == "m.bridge"
                        && event["state_key"] == key
                        && event["content"]["creator"] == ghost
                        && event["content"]["channel"]["id"] == bot
                })
            {
                matches.push(room.to_string());
            }
        }
        ensure!(
            matches.len() <= 1,
            "multiple rooms match this agent; delivery paused"
        );
        if let Some(room) = matches.pop() {
            return Ok(room);
        }
        let body = self.ok(Method::POST,&["createRoom"],ghost,Some(json!({
            "name":format!("{name} · Rakazo"),
            "topic":"Rakazo agent conversation. This bridge currently requires an unencrypted room.",
            "visibility":"private", "preset":"private_chat", "invite":[owner],
            "creation_content":{"m.federate":false},
            "power_level_content_override":{
                "users":{ghost:100,owner:0},"users_default":0,"events_default":100,"state_default":100,
                "invite":100,"kick":100,"ban":100,"redact":100,
                "events":{"m.room.message":0,"m.room.encryption":100}
            },
            "initial_state":[
                {"type":"m.room.history_visibility","state_key":"","content":{"history_visibility":"joined"}},
                {"type":"m.bridge","state_key":key,"content":{
                    "bridgebot":ghost,"creator":ghost,"protocol":{"id":"rakazo","displayname":"Rakazo"},
                    "channel":{"id":bot,"displayname":name}
                }}
            ]
        }))).await?;
        Ok(body["room_id"]
            .as_str()
            .context("Matrix createRoom missing room_id")?
            .to_string())
    }

    /// @implements url:https://spec.matrix.org/v1.16/client-server-api/ §Content repository, §Profiles, §m.room.avatar
    pub async fn upload_avatar(&self, user: &str, avatar: crate::avatar::Avatar) -> Result<String> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Matrix base URL"))?
            .pop_if_empty()
            .extend(["_matrix", "media", "v3", "upload"]);
        url.query_pairs_mut()
            .append_pair("user_id", user)
            .append_pair("filename", "avatar");
        let response = self
            .client
            .post(url)
            .header(AUTHORIZATION, self.token.clone())
            .header(reqwest::header::CONTENT_TYPE, avatar.mime)
            .body(avatar.bytes)
            .send()
            .await
            .map_err(|error| {
                anyhow::anyhow!("Matrix avatar upload failed: {}", error.without_url())
            })?;
        ensure!(
            response.status().is_success(),
            "Matrix avatar upload returned HTTP {}",
            response.status()
        );
        let result: Value = response.json().await?;
        let uri = result["content_uri"]
            .as_str()
            .context("Matrix upload missing content_uri")?;
        let parsed = Url::parse(uri)?;
        ensure!(
            parsed.scheme() == "mxc"
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "Matrix upload returned invalid media URI"
        );
        Ok(uri.to_string())
    }

    pub async fn sync_profile(
        &self,
        user: &str,
        room: &str,
        name: &str,
        avatar: &str,
    ) -> Result<()> {
        self.ok(
            Method::PUT,
            &["profile", user, "displayname"],
            user,
            Some(json!({"displayname":name})),
        )
        .await?;
        self.ok(
            Method::PUT,
            &["profile", user, "avatar_url"],
            user,
            Some(json!({"avatar_url":avatar})),
        )
        .await?;
        self.ok(
            Method::PUT,
            &["rooms", room, "state", "m.room.avatar", ""],
            user,
            Some(json!({"url":avatar})),
        )
        .await?;
        self.ok(
            Method::PUT,
            &["rooms", room, "state", "m.room.name", ""],
            user,
            Some(json!({"name":format!("{name} · Rakazo")})),
        )
        .await?;
        Ok(())
    }

    /// Check authoritative state on every delivery, including after a restart.
    pub async fn check_room(&self, room: &str, ghost: &str, owner: &str) -> Result<()> {
        let state = self
            .ok(Method::GET, &["rooms", room, "state"], ghost, None)
            .await?;
        let events = state
            .as_array()
            .context("Matrix room state must be an array")?;
        ensure!(
            !events.iter().any(|e| e["type"] == "m.room.encryption"),
            "encrypted rooms are unsupported; delivery paused"
        );
        let mut ghost_joined = false;
        for event in events.iter().filter(|e| e["type"] == "m.room.member") {
            let user = event["state_key"]
                .as_str()
                .context("member missing state_key")?;
            let member = event["content"]["membership"].as_str().unwrap_or("");
            if user == ghost && member == "join" {
                ghost_joined = true;
            }
            if matches!(member, "join" | "invite") {
                ensure!(
                    user == owner || user == ghost,
                    "unexpected room member; delivery paused"
                );
            }
        }
        ensure!(ghost_joined, "ghost is no longer joined; delivery paused");
        Ok(())
    }

    pub async fn send(&self, room: &str, ghost: &str, transaction: &str, text: &str) -> Result<()> {
        ensure!(
            text.len() <= 48_000,
            "message exceeds Matrix text limit; delivery paused"
        );
        self.ok(
            Method::PUT,
            &["rooms", room, "send", "m.room.message", transaction],
            ghost,
            Some(json!({"msgtype":"m.text","body":text})),
        )
        .await?;
        Ok(())
    }
}
