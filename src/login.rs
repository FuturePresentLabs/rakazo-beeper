//! Continue Rakazo's normal OIDC flow using a narrowly delegated device grant.
//! The issuer authenticates its own token; only Rakazo issues Rakazo sessions.
use anyhow::{Context, Result, ensure};
use reqwest::{Client, Response, header, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Write, path::Path, time::Duration};
use url::Url;

#[derive(Deserialize)]
struct Discovery {
    issuer: Url,
    device_authorization_endpoint: Url,
    token_endpoint: Url,
}
#[derive(Deserialize)]
struct Device {
    device_code: String,
    user_code: String,
    verification_uri: Url,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}
fn default_interval() -> u64 {
    5
}
fn same_origin(a: &Url, b: &Url) -> bool {
    a.origin() == b.origin() && b.username().is_empty() && b.password().is_none()
}
fn parameter(url: &Url, name: &str) -> Result<String> {
    let values: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .collect();
    ensure!(
        values.len() == 1 && !values[0].is_empty(),
        "missing or duplicate OIDC {name}"
    );
    Ok(values[0].clone())
}
fn authorization_target(rakazo: &Url, issuer: &Url, auth: &Url) -> Result<String> {
    ensure!(
        same_origin(issuer, auth) && auth.path() == "/authorize",
        "unexpected issuer authorization endpoint"
    );
    let target = parameter(auth, "client_id")?;
    ensure!(
        target.len() <= 256
            && target
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
        "invalid OIDC client identity"
    );
    ensure!(
        Url::parse(&parameter(auth, "redirect_uri")?)? == rakazo.join("/api/auth/callback/oidc")?,
        "unexpected Rakazo OIDC callback"
    );
    ensure!(
        parameter(auth, "scope")?
            .split_whitespace()
            .all(|s| matches!(s, "openid" | "email" | "profile")),
        "unsupported OIDC permissions"
    );
    parameter(auth, "state")?;
    parameter(auth, "nonce")?;
    ensure!(
        parameter(auth, "code_challenge_method")? == "S256",
        "Rakazo login must use S256 PKCE"
    );
    parameter(auth, "code_challenge")?;
    Ok(target)
}
async fn body(response: Response) -> Result<Value> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(reqwest::Error::without_url)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "authentication response too large"
    );
    ensure!(
        status.is_success(),
        "authentication endpoint returned HTTP {status}"
    );
    serde_json::from_slice(&bytes).context("invalid authentication response")
}
fn session_cookie(response: &Response) -> Result<String> {
    let mut tokens = Vec::new();
    for value in response.headers().get_all(header::SET_COOKIE) {
        let first = value.to_str()?.split(';').next().unwrap_or_default();
        let Some((name, value)) = first.split_once('=') else {
            continue;
        };
        if matches!(
            name,
            "better-auth.session_token" | "__Secure-better-auth.session_token"
        ) {
            tokens.push(
                percent_encoding::percent_decode_str(value)
                    .decode_utf8()?
                    .into_owned(),
            );
        }
    }
    ensure!(
        tokens.len() == 1 && !tokens[0].is_empty(),
        "Rakazo did not issue one unambiguous session"
    );
    Ok(tokens.remove(0))
}
fn save_session(output: &Path, token: &str) -> Result<()> {
    use std::fs::{self, OpenOptions};
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Ok(existing) = fs::symlink_metadata(output) {
        ensure!(
            existing.is_file() && !existing.file_type().is_symlink(),
            "session output must be a regular file"
        );
    }
    let temporary = parent.join(format!(".rakazo-session-{:032x}", rand::random::<u128>()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        writeln!(file, "{token}")?;
        file.sync_all()?;
        fs::rename(&temporary, output)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
async fn start_login(rk: &Client, rakazo: &Url) -> Result<Url> {
    let start = body(
        rk.post(rakazo.join("/api/auth/sign-in/social")?)
            .header(header::ORIGIN, rakazo.origin().ascii_serialization())
            .json(&json!({"provider":"oidc","callbackURL":rakazo.as_str(),"disableRedirect":true}))
            .send()
            .await
            .map_err(reqwest::Error::without_url)?,
    )
    .await?;
    Url::parse(
        start["url"]
            .as_str()
            .context("Rakazo returned no authorization URL")?,
    )
    .context("invalid authorization URL")
}

pub async fn login(rakazo: &Url, issuer: &Url, client_id: &str, output: &Path) -> Result<()> {
    crate::config::validate_url(rakazo)?;
    crate::config::validate_url(issuer)?;
    for origin in [rakazo, issuer] {
        ensure!(
            origin.path() == "/" && origin.query().is_none() && origin.fragment().is_none(),
            "login requires exact origin URLs"
        );
    }
    let client = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    // This jar contains only the newly started login, never browser sessions.
    // Separate clients prevent forwarding Rakazo cookies to the issuer.
    let rk = Client::builder()
        .cookie_store(true)
        .redirect(Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let auth = start_login(&rk, rakazo).await?;
    let target = authorization_target(rakazo, issuer, &auth)?;
    let discovery: Discovery = serde_json::from_value(
        body(
            client
                .get(issuer.join("/.well-known/openid-configuration")?)
                .send()
                .await
                .map_err(reqwest::Error::without_url)?,
        )
        .await?,
    )
    .context("issuer does not advertise device authorization")?;
    ensure!(
        discovery.issuer == *issuer
            && same_origin(issuer, &discovery.device_authorization_endpoint)
            && same_origin(issuer, &discovery.token_endpoint),
        "issuer discovery endpoints mismatch"
    );
    let scope = format!("openid email profile oidc:authorize:{target}");
    let device: Device = serde_json::from_value(
        body(
            client
                .post(discovery.device_authorization_endpoint)
                .form(&[("client_id", client_id), ("scope", &scope)])
                .send()
                .await
                .map_err(reqwest::Error::without_url)?,
        )
        .await?,
    )?;
    ensure!(
        same_origin(issuer, &device.verification_uri)
            && (1..=600).contains(&device.expires_in)
            && (1..=60).contains(&device.interval),
        "invalid device authorization parameters"
    );
    println!(
        "Open {} and enter {} to connect this bridge to Rakazo.",
        device.verification_uri, device.user_code
    );
    std::io::stdout().flush()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval;
    // @implements rfc:8628 §3.5 -- minimum polling interval, slow_down and expiry
    let access = loop {
        ensure!(
            tokio::time::Instant::now() + Duration::from_secs(interval) < deadline,
            "device approval expired; run login again"
        );
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = client
            .post(discovery.token_endpoint.clone())
            .form(&[
                ("client_id", client_id),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &device.device_code),
            ])
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(reqwest::Error::without_url)?;
        ensure!(bytes.len() <= 1024 * 1024, "token response too large");
        let response: Value = serde_json::from_slice(&bytes)?;
        if status.is_success() {
            ensure!(
                response["token_type"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("bearer")),
                "unsupported device token type"
            );
            break response["access_token"]
                .as_str()
                .context("device response has no access token")?
                .to_owned();
        }
        match response["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") => anyhow::bail!("device connection denied"),
            Some("expired_token") => anyhow::bail!("device approval expired; run login again"),
            _ => anyhow::bail!("device token endpoint returned HTTP {status}"),
        }
    };
    // Rakazo's state cookie may expire before the device approval does. Start
    // the actual PKCE flow after approval, bound to the same approved client.
    let auth = start_login(&rk, rakazo).await?;
    ensure!(
        authorization_target(rakazo, issuer, &auth)? == target,
        "Rakazo changed OIDC clients during pairing"
    );
    let authorized = client
        .get(auth.clone())
        .bearer_auth(access)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    ensure!(
        authorized.status().is_redirection(),
        "issuer did not authorize the delegated Rakazo login"
    );
    let callback = Url::parse(
        authorized
            .headers()
            .get(header::LOCATION)
            .context("missing OIDC callback")?
            .to_str()?,
    )?;
    ensure!(
        same_origin(rakazo, &callback)
            && callback.path() == "/api/auth/callback/oidc"
            && parameter(&callback, "state")? == parameter(&auth, "state")?
            && !parameter(&callback, "code")?.is_empty()
            && !callback.query_pairs().any(|(k, _)| k == "error"),
        "unexpected OIDC callback or state"
    );
    let completed = rk
        .get(callback)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    ensure!(
        completed.status().is_redirection(),
        "Rakazo did not complete sign-in"
    );
    let session = session_cookie(&completed)?;
    let verified = body(
        rk.get(rakazo.join("/api/auth/get-session")?)
            .bearer_auth(&session)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?,
    )
    .await?;
    ensure!(
        verified["user"]["id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "Rakazo rejected its session"
    );
    save_session(output, &session)?;
    println!("Rakazo session saved. Restart the bridge to use it.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Form, State},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn auth(rakazo: &Url, issuer: &Url) -> Url {
        let mut url = issuer.join("/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("client_id", "agents-client"),
            (
                "redirect_uri",
                rakazo.join("/api/auth/callback/oidc").unwrap().as_str(),
            ),
            ("scope", "openid email profile"),
            ("state", "opaque-state"),
            ("nonce", "opaque-nonce"),
            ("code_challenge_method", "S256"),
            ("code_challenge", "opaque-challenge"),
        ]);
        url
    }
    #[test]
    fn authorization_is_fenced_before_requesting_device_permission() {
        let rk = Url::parse("https://agents.example.test").unwrap();
        let issuer = Url::parse("https://sso.example.test").unwrap();
        let valid = auth(&rk, &issuer);
        assert_eq!(
            authorization_target(&rk, &issuer, &valid).unwrap(),
            "agents-client"
        );
        let mut attacker = valid.clone();
        attacker.set_host(Some("attacker.example.test")).unwrap();
        assert!(authorization_target(&rk, &issuer, &attacker).is_err());
        for (name, value) in [
            ("client_id", "other-client"),
            ("redirect_uri", "https://attacker.example.test/callback"),
            ("scope", "mcp:write"),
            ("state", "other"),
        ] {
            let mut duplicate = valid.clone();
            duplicate.query_pairs_mut().append_pair(name, value);
            assert!(authorization_target(&rk, &issuer, &duplicate).is_err());
        }
        let mut no_pkce = valid.clone();
        let pairs: Vec<_> = no_pkce
            .query_pairs()
            .filter(|(k, _)| k != "code_challenge_method")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        no_pkce.set_query(None);
        no_pkce
            .query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("code_challenge_method", "plain");
        assert!(authorization_target(&rk, &issuer, &no_pkce).is_err());
    }
    #[test]
    fn sessions_are_written_privately_and_replaced_only_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("token");
        save_session(&output, "first").unwrap();
        save_session(&output, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "second\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let alias = dir.path().join("alias");
            symlink(&output, &alias).unwrap();
            assert!(save_session(&alias, "wrong").is_err());
            assert_eq!(std::fs::read_to_string(&output).unwrap(), "second\n");
        }
    }
    #[derive(Clone)]
    struct Fixture {
        rk: Url,
        issuer: Url,
        bad_state: bool,
        denied: bool,
        polls: Arc<AtomicUsize>,
    }
    async fn fixture(bad_state: bool, denied: bool) -> (Fixture, Vec<tokio::task::JoinHandle<()>>) {
        let rkl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let il = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let f = Fixture {
            rk: Url::parse(&format!("http://{}/", rkl.local_addr().unwrap())).unwrap(),
            issuer: Url::parse(&format!("http://{}/", il.local_addr().unwrap())).unwrap(),
            bad_state,
            denied,
            polls: Arc::new(AtomicUsize::new(0)),
        };
        let rk = Router::new()
            .route(
                "/api/auth/sign-in/social",
                post(
                    |State(f): State<Fixture>, Json(body): Json<Value>| async move {
                        assert_eq!(body["provider"], "oidc");
                        (
                            [(
                                header::SET_COOKIE,
                                if f.polls.load(Ordering::SeqCst) == 0 {
                                    // The discovery login's cookie expires while
                                    // the user approves. Only a fresh login works.
                                    "better-auth.oauth_state=expired-login-state; Path=/"
                                } else {
                                    "better-auth.oauth_state=private-login-state; Path=/"
                                },
                            )],
                            Json(json!({"url":auth(&f.rk,&f.issuer)})),
                        )
                    },
                ),
            )
            .route(
                "/api/auth/callback/oidc",
                get(|headers: HeaderMap| async move {
                    assert!(
                        headers[header::COOKIE]
                            .to_str()
                            .unwrap()
                            .contains("better-auth.oauth_state=private-login-state")
                    );
                    assert!(!headers.contains_key(header::AUTHORIZATION));
                    (
                        StatusCode::FOUND,
                        [
                            (
                                header::SET_COOKIE,
                                "better-auth.session_token=new%2Fsession%3D; Path=/",
                            ),
                            (header::LOCATION, "/"),
                        ],
                    )
                }),
            )
            .route(
                "/api/auth/get-session",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Bearer new/session=");
                    Json(json!({"user":{"id":"owner"}}))
                }),
            )
            .with_state(f.clone());
        let issuer=Router::new().route("/.well-known/openid-configuration",get(|State(f):State<Fixture>|async move { Json(json!({"issuer":f.issuer,"device_authorization_endpoint":f.issuer.join("/device_authorization").unwrap(),"token_endpoint":f.issuer.join("/token").unwrap()})) }))
            .route("/device_authorization",post(|State(f):State<Fixture>,headers:HeaderMap,Form(body):Form<HashMap<String,String>>|async move {
                assert!(!headers.contains_key(header::COOKIE));
                assert_eq!(body["scope"],"openid email profile oidc:authorize:agents-client");
                Json(json!({"device_code":"private-device-code","user_code":"BCDFG-HJKLM","verification_uri":f.issuer.join("/device").unwrap(),"expires_in":60,"interval":1}))
            })).route("/token",post(|State(f):State<Fixture>,headers:HeaderMap,Form(body):Form<HashMap<String,String>>|async move {
                assert!(!headers.contains_key(header::COOKIE)); assert_eq!(body["device_code"],"private-device-code");
                if f.denied {return (StatusCode::BAD_REQUEST,Json(json!({"error":"access_denied"}))).into_response()}
                if f.polls.fetch_add(1,Ordering::SeqCst)==0 {return (StatusCode::BAD_REQUEST,Json(json!({"error":"authorization_pending"}))).into_response()}
                Json(json!({"access_token":"issuer-only-token","token_type":"Bearer"})).into_response()
            })).route("/authorize",get(|State(f):State<Fixture>,headers:HeaderMap|async move {
                assert!(!headers.contains_key(header::COOKIE)); assert_eq!(headers[header::AUTHORIZATION],"Bearer issuer-only-token");
                let mut callback=f.rk.join("/api/auth/callback/oidc").unwrap(); callback.query_pairs_mut().append_pair("code","code-for-rakazo").append_pair("state",if f.bad_state {"wrong"} else {"opaque-state"});
                (StatusCode::SEE_OTHER,[(header::LOCATION,callback.to_string())])
            })).with_state(f.clone());
        let tasks = vec![
            tokio::spawn(async move { axum::serve(rkl, rk).await.unwrap() }),
            tokio::spawn(async move { axum::serve(il, issuer).await.unwrap() }),
        ];
        (f, tasks)
    }
    #[tokio::test]
    async fn device_approval_completes_normal_rakazo_login_without_credential_crossover() {
        let (f, tasks) = fixture(false, false).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        login(&f.rk, &f.issuer, "rakazo-beeper", &path)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new/session=\n");
        assert_eq!(f.polls.load(Ordering::SeqCst), 2);
        for task in tasks {
            task.abort()
        }
    }
    #[tokio::test]
    async fn state_mismatch_never_replaces_an_existing_credential() {
        let (f, tasks) = fixture(true, false).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        save_session(&path, "existing").unwrap();
        assert!(
            login(&f.rk, &f.issuer, "rakazo-beeper", &path)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "existing\n");
        for task in tasks {
            task.abort()
        }
    }
    #[tokio::test]
    async fn denied_pairing_never_creates_a_credential() {
        let (f, tasks) = fixture(false, true).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        assert!(
            login(&f.rk, &f.issuer, "rakazo-beeper", &path)
                .await
                .is_err()
        );
        assert!(!path.exists());
        for task in tasks {
            task.abort()
        }
    }
}
