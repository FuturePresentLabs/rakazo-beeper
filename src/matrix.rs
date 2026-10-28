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
                "users":{ghost:100,owner:0},"users_default":0,"events_default":0,"state_default":100,
                "invite":100,"kick":100,"ban":100,"redact":100,
                "events":{"m.room.message":0,"m.reaction":0,"m.sticker":0,"m.room.encryption":100}
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
        self.upload_media(user, "avatar", avatar.mime, avatar.bytes)
            .await
    }

    pub async fn upload_media(
        &self,
        user: &str,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<String> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Matrix base URL"))?
            .pop_if_empty()
            .extend(["_matrix", "media", "v3", "upload"]);
        url.query_pairs_mut()
            .append_pair("user_id", user)
            .append_pair("filename", name);
        let response = self
            .client
            .post(url)
            .header(AUTHORIZATION, self.token.clone())
            .header(reqwest::header::CONTENT_TYPE, mime)
            .body(bytes)
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

    pub async fn bridge_identity(&self, avatar: &str) -> Result<()> {
        self.register(&self.sender, "Rakazo").await?;
        self.ok(
            Method::PUT,
            &["profile", &self.sender, "avatar_url"],
            &self.sender,
            Some(json!({"avatar_url":avatar})),
        )
        .await?;
        Ok(())
    }

    /// @implements github:mautrix/go@v0.28.1 bridgev2/portal.go UpdateBridgeInfo -- protocol icon in both bridge state event formats
    pub async fn bridge_info(
        &self,
        binding: &crate::store::Binding,
        space_name: &str,
        name: &str,
        logo: &str,
        avatar: &str,
    ) -> Result<()> {
        let key = format!("rakazo://{}/{}", binding.tenant, binding.bot);
        let content = json!({"bridgebot":self.sender,"creator":binding.ghost,"protocol":{"id":"rakazo","displayname":space_name,"avatar_url":logo},"channel":{"id":binding.bot,"displayname":name,"avatar_url":avatar}});
        for event_type in ["m.bridge", "uk.half-shot.bridge"] {
            self.ok(
                Method::PUT,
                &["rooms", &binding.room, "state", event_type, &key],
                &binding.ghost,
                Some(content.clone()),
            )
            .await?;
        }
        Ok(())
    }
    pub async fn room_features(
        &self,
        binding: &crate::store::Binding,
        features: Value,
    ) -> Result<()> {
        let key = format!("rakazo://{}/{}", binding.tenant, binding.bot);
        self.ok(
            Method::PUT,
            &[
                "rooms",
                &binding.room,
                "state",
                "com.beeper.room_features",
                &key,
            ],
            &binding.ghost,
            Some(features),
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
        self.send_event(
            room,
            ghost,
            transaction,
            "m.room.message",
            json!({"msgtype":"m.text","body":text}),
        )
        .await?;
        Ok(())
    }

    pub async fn send_event(
        &self,
        room: &str,
        user: &str,
        transaction: &str,
        kind: &str,
        content: Value,
    ) -> Result<String> {
        let response = self
            .ok(
                Method::PUT,
                &["rooms", room, "send", kind, transaction],
                user,
                Some(content),
            )
            .await?;
        Ok(response["event_id"]
            .as_str()
            .context("Matrix send missing event_id")?
            .to_string())
    }

    pub async fn event(&self, room: &str, user: &str, id: &str) -> Result<Value> {
        self.ok(Method::GET, &["rooms", room, "event", id], user, None)
            .await
    }

    pub async fn redact(&self, room: &str, user: &str, id: &str, transaction: &str) -> Result<()> {
        let (status, _) = self
            .request(
                Method::PUT,
                &["rooms", room, "redact", id, transaction],
                Some(user),
                Some(json!({})),
            )
            .await?;
        ensure!(
            status.is_success() || status == StatusCode::NOT_FOUND,
            "Matrix request returned HTTP {status}"
        );
        Ok(())
    }

    /// Permit owner timeline events using the default checked by Beeper iOS,
    /// preserving membership, state and moderation restrictions in old rooms.
    pub async fn rich_permissions(&self, binding: &crate::store::Binding) -> Result<()> {
        let path = [
            "rooms",
            binding.room.as_str(),
            "state",
            "m.room.power_levels",
            "",
        ];
        let mut levels = self.ok(Method::GET, &path, &binding.ghost, None).await?;
        let events = levels
            .get_mut("events")
            .and_then(Value::as_object_mut)
            .context("managed room power levels missing event restrictions")?;
        let mut changed = false;
        for kind in ["m.reaction", "m.sticker"] {
            if events.get(kind) != Some(&json!(0)) {
                events.insert(kind.into(), json!(0));
                changed = true;
            }
        }
        // Beeper iOS treats events_default as the messaging permission even when
        // m.room.message has a lower explicit override. State uses state_default.
        if levels["events_default"] != 0 {
            levels["events_default"] = json!(0);
            changed = true;
        }
        if changed {
            self.ok(Method::PUT, &path, &binding.ghost, Some(levels))
                .await?;
        }
        Ok(())
    }

    pub async fn room_messages(&self, room: &str, user: &str, from: Option<&str>) -> Result<Value> {
        let mut url = self.url(&["rooms", room, "messages"])?;
        url.query_pairs_mut()
            .append_pair("user_id", user)
            .append_pair("dir", "b")
            .append_pair("limit", "1000");
        if let Some(from) = from {
            url.query_pairs_mut().append_pair("from", from);
        }
        let response = self
            .client
            .get(url)
            .header(AUTHORIZATION, self.token.clone())
            .send()
            .await
            .map_err(|error| {
                anyhow::anyhow!("Matrix history lookup failed: {}", error.without_url())
            })?;
        ensure!(
            response.status().is_success(),
            "Matrix history lookup returned HTTP {}",
            response.status()
        );
        Ok(response.json().await?)
    }

    /// @implements url:https://spec.matrix.org/v1.16/client-server-api/#get_matrixclientv1mediadownloadservernamemediaid -- authenticated download, bounded bytes and credential-free CDN redirects
    pub async fn download_media(
        &self,
        user: &str,
        content: &Value,
        limit: usize,
    ) -> Result<crate::media::Media> {
        use futures_util::StreamExt;
        let encrypted = content.get("file");
        let uri = encrypted
            .map(|file| &file["url"])
            .unwrap_or(&content["url"])
            .as_str()
            .ok_or(crate::media::Rejected(
                "Attachment is missing its Matrix media URI.",
            ))?;
        let uri = Url::parse(uri).map_err(|_| crate::media::Rejected("Invalid attachment URI."))?;
        ensure!(
            uri.scheme() == "mxc"
                && uri.username().is_empty()
                && uri.password().is_none()
                && uri.query().is_none()
                && uri.fragment().is_none(),
            crate::media::Rejected("Attachment must use a Matrix media URI.")
        );
        let server = uri.host_str().ok_or(crate::media::Rejected(
            "Attachment is missing its Matrix media server.",
        ))?;
        let server = if let Some(port) = uri.port() {
            format!("{server}:{port}")
        } else {
            server.to_string()
        };
        let id = uri
            .path()
            .strip_prefix('/')
            .filter(|id| !id.is_empty() && !id.contains('/'))
            .ok_or(crate::media::Rejected("Invalid Matrix media identifier."))?;
        let name = crate::media::filename(
            content["filename"]
                .as_str()
                .or(content["body"].as_str())
                .unwrap_or("attachment"),
        )?;
        let mime = crate::media::mime(
            content["info"]["mimetype"]
                .as_str()
                .unwrap_or("application/octet-stream"),
        )?;
        if let Some(size) = content["info"]["size"].as_u64() {
            ensure!(
                size <= limit as u64,
                crate::media::Rejected("Attachment exceeds the bridge's 10 MiB limit.")
            );
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .extend(["_matrix", "client", "v1", "media", "download", &server, id]);
        url.query_pairs_mut()
            .append_pair("user_id", user)
            .append_pair("allow_redirect", "false");
        let mut response = self
            .client
            .get(url)
            .header(AUTHORIZATION, self.token.clone())
            .send()
            .await
            .map_err(|error| {
                anyhow::anyhow!("Matrix media download failed: {}", error.without_url())
            })?;
        // Hungryserv serves authenticated media through expiring R2 URLs. Follow
        // only this provider's signed CDN or the configured homeserver origin,
        // with no appservice credentials/cookies on the redirected request.
        for _ in 0..3 {
            if !response.status().is_redirection() {
                break;
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(crate::media::Rejected(
                    "Media redirect is missing its destination.",
                ))?;
            let location = response
                .url()
                .join(location)
                .map_err(|_| crate::media::Rejected("Invalid media redirect."))?;
            ensure!(
                self.media_redirect_allowed(&location),
                crate::media::Rejected("Media redirected to an untrusted destination.")
            );
            response = self.client.get(location).send().await.map_err(|error| {
                anyhow::anyhow!("Media CDN download failed: {}", error.without_url())
            })?;
        }
        ensure!(
            !response.status().is_redirection(),
            crate::media::Rejected("Too many media redirects.")
        );
        if matches!(response.status().as_u16(), 400 | 404 | 410 | 413 | 415) {
            return Err(crate::media::Rejected(
                "Attachment is unavailable or exceeds the media limit.",
            )
            .into());
        }
        ensure!(
            response.status().is_success(),
            "Matrix media download returned HTTP {}",
            response.status()
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= limit as u64),
            crate::media::Rejected("Attachment exceeds the bridge's 10 MiB limit.")
        );
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("Matrix media stream failed"))?;
            ensure!(
                chunk.len() <= limit.saturating_sub(bytes.len()),
                crate::media::Rejected("Attachment exceeds the bridge's 10 MiB limit.")
            );
            bytes.extend_from_slice(&chunk);
        }
        if let Some(file) = encrypted {
            crate::media::decrypt(&mut bytes, file)
                .map_err(|_| crate::media::Rejected("Encrypted attachment failed verification."))?;
        }
        ensure!(
            !bytes.is_empty(),
            crate::media::Rejected("Empty attachments cannot be sent to Rakazo.")
        );
        ensure!(
            content["info"]["size"]
                .as_u64()
                .is_none_or(|size| size == bytes.len() as u64),
            crate::media::Rejected("Attachment download size did not match its metadata.")
        );
        Ok(crate::media::Media { name, mime, bytes })
    }

    fn media_redirect_allowed(&self, destination: &Url) -> bool {
        if !destination.username().is_empty()
            || destination.password().is_some()
            || destination.fragment().is_some()
        {
            return false;
        }
        if destination.origin() == self.base.origin() {
            return true;
        }
        let beeper = self
            .base
            .host_str()
            .is_some_and(|host| host == "matrix.beeper.com");
        beeper
            && destination.scheme() == "https"
            && destination.port_or_known_default() == Some(443)
            && destination
                .host_str()
                .is_some_and(|host| host.ends_with(".r2.cloudflarestorage.com"))
    }

    /// @implements github:mautrix/go@v0.28.1 requests.go ReqBeeperBatchSend, bridgev2/portalbackfill.go -- timestamped, silent, deterministic backfill
    pub async fn backfill(&self, room: &str, events: Vec<Value>) -> Result<()> {
        self.batch_send(room, events, false).await
    }

    pub async fn batch_send(&self, room: &str, events: Vec<Value>, forward: bool) -> Result<()> {
        let expected: Vec<_> = events
            .iter()
            .map(|event| event["event_id"].clone())
            .collect();
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Matrix base URL"))?
            .pop_if_empty()
            .extend([
                "_matrix",
                "client",
                "unstable",
                "com.beeper.backfill",
                "rooms",
                room,
                "batch_send",
            ]);
        url.query_pairs_mut().append_pair("user_id", &self.sender);
        let response = self.client.post(url).header(AUTHORIZATION,self.token.clone())
            .json(&json!({"forward":forward,"forward_if_no_messages":true,"send_notification":forward,"events":events}))
            .send().await.map_err(|error| anyhow::anyhow!("Matrix history transport failed: {}", error.without_url()))?;
        ensure!(
            response.status().is_success(),
            "Matrix history import returned HTTP {}",
            response.status()
        );
        let body: Value = response.json().await?;
        ensure!(
            body["event_ids"] == json!(expected),
            "Matrix history import returned unexpected event IDs; cursor retained"
        );
        Ok(())
    }
}
