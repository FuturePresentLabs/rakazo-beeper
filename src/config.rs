use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use url::Url;

use crate::fleet::ack::AckPolicy;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database: PathBuf,
    pub encryption_key_env: String,
    #[serde(default = "listen_default")]
    pub listen: SocketAddr,
    #[serde(default = "poll_default")]
    pub poll_seconds: u64,
    pub tenants: Vec<Tenant>,
}
fn listen_default() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}
fn poll_default() -> u64 {
    5
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tenant {
    pub id: String,
    pub owner: String,
    pub registration: PathBuf,
    pub homeserver: Url,
    pub rakazo_url: Url,
    pub rakazo_token_env: String,
    pub rakazo_space_id: String,
    /// Explicitly opt individual bots into bridging. Empty means none.
    pub bot_ids: Vec<String>,
    /// Automatically keep the orchestrator fleet in sync. Explicit `bot_ids`
    /// are added to this set. Removed/archived bots stop being polled.
    #[serde(default)]
    pub auto_bots: bool,
    /// Additional active bots selected by case-insensitive exact display name.
    /// This is data, not policy: no bot name is hardcoded in bridge logic.
    #[serde(default)]
    pub auto_named_bots: Vec<String>,
    #[serde(default = "websocket_default")]
    pub websocket: bool,
    /// When the bridge may acknowledge an inbound message.
    ///
    /// `mid_thread_only` (the default) acknowledges only while the agent is
    /// already working on a prior task: an idle agent answers instead. The
    /// other values are `never` and `always`.
    #[serde(default)]
    pub ack_policy: AckPolicy,
    /// Seconds before an acknowledged message is due for its follow-up.
    #[serde(default = "ack_due_default")]
    pub ack_due_seconds: u64,
    /// Seconds after the due time before the broken promise is admitted to
    /// the human in the thread.
    #[serde(default = "ack_grace_default")]
    pub ack_grace_seconds: u64,
}
fn websocket_default() -> bool {
    true
}
fn ack_due_default() -> u64 {
    900
}
fn ack_grace_default() -> u64 {
    300
}

#[derive(Clone, Deserialize)]
pub struct Registration {
    pub id: String,
    pub as_token: String,
    pub hs_token: String,
    pub sender_localpart: String,
    pub namespaces: Namespaces,
}
#[derive(Clone, Deserialize)]
pub struct Namespaces {
    pub users: Vec<Namespace>,
}
#[derive(Clone, Deserialize)]
pub struct Namespace {
    pub regex: String,
    pub exclusive: bool,
}

#[derive(Deserialize)]
pub struct RegistrationBundle {
    pub registration: Registration,
    pub homeserver_url: Url,
    pub homeserver_domain: String,
    pub your_user_id: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Self = toml::from_str(&std::fs::read_to_string(path)?)
            .context("invalid bridge configuration")?;
        let root = path.parent().unwrap_or(Path::new("."));
        if config.database.is_relative() {
            config.database = root.join(&config.database);
        }
        ensure!(config.poll_seconds >= 1, "poll_seconds must be positive");
        ensure!(!config.tenants.is_empty(), "configure at least one tenant");
        let mut ids = HashSet::new();
        let mut registrations = HashSet::new();
        let mut tokens = HashSet::new();
        let mut app_tokens = HashSet::new();
        for tenant in &mut config.tenants {
            ensure!(valid_slug(&tenant.id), "tenant id must be a lowercase slug");
            ensure!(ids.insert(tenant.id.clone()), "duplicate tenant id");
            ensure!(
                tenant.owner.starts_with('@') && tenant.owner.contains(':'),
                "invalid Matrix owner"
            );
            ensure!(
                !tenant.rakazo_space_id.is_empty(),
                "explicit Rakazo space id required"
            );
            validate_url(&tenant.homeserver)?;
            validate_url(&tenant.rakazo_url)?;
            ensure!(
                tenant.bot_ids.iter().all(|id| !id.is_empty()),
                "empty bot id"
            );
            ensure!(
                tenant.bot_ids.iter().collect::<HashSet<_>>().len() == tenant.bot_ids.len(),
                "duplicate bot id"
            );
            if tenant.registration.is_relative() {
                tenant.registration = root.join(&tenant.registration);
            }
            let bundle = tenant.bundle()?;
            let reg = &bundle.registration;
            ensure!(
                reg.as_token.len() >= 20 && reg.hs_token.len() >= 20,
                "registration tokens too short"
            );
            ensure!(
                tokens.insert(reg.hs_token.clone()),
                "tenants must use separate registration tokens"
            );
            ensure!(
                app_tokens.insert(reg.as_token.clone()),
                "tenants must use separate appservice tokens"
            );
            ensure!(
                registrations.insert((tenant.homeserver.to_string(), reg.id.clone())),
                "duplicate appservice registration"
            );
            secret(&tenant.rakazo_token_env)?;
        }
        Ok(config)
    }
}

impl Tenant {
    pub fn bundle(&self) -> Result<RegistrationBundle> {
        let bundle: RegistrationBundle = serde_json::from_str(
            &std::fs::read_to_string(&self.registration)
                .context("read registration JSON bundle")?,
        )
        .context("registration must be the full bbctl register --json response")?;
        ensure!(
            bundle.homeserver_url == self.homeserver,
            "homeserver differs from registration bundle"
        );
        ensure!(
            bundle.your_user_id == self.owner,
            "owner differs from registration bundle"
        );
        ensure!(
            !bundle.homeserver_domain.is_empty(),
            "registration domain missing"
        );
        Ok(bundle)
    }
}

pub fn secret(name: &str) -> Result<String> {
    let file_name = format!("{name}_FILE");
    let direct = std::env::var(name).ok();
    let file = std::env::var(&file_name).ok();
    let value = match (direct, file) {
        (Some(value), None) => value,
        (None, Some(path)) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
                    "secret file referenced by {file_name} must be accessible only to its owner"
                );
            }
            std::fs::read_to_string(path)
                .with_context(|| format!("read secret file referenced by {file_name}"))?
                .trim_end()
                .to_string()
        }
        (Some(_), Some(_)) => bail!("set only one of {name} or {file_name}"),
        (None, None) => bail!("set {name} or {file_name}"),
    };
    ensure!(
        !value.trim().is_empty(),
        "empty credential environment variable {name}"
    );
    Ok(value)
}

pub fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn validate_url(url: &Url) -> Result<()> {
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "URL must not contain credentials, query, or fragment"
    );
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        bail!("upstream URLs require HTTPS except on loopback");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Tenant;
    use crate::fleet::ack::AckPolicy;

    #[test]
    fn tenant_accepts_configurable_named_bots() {
        let tenant = toml::from_str::<Tenant>(
            r#"
id = "personal"
owner = "@owner:example.com"
registration = "/tmp/registration.json"
homeserver = "https://matrix.example.com"
rakazo_url = "https://rakazo.example.com"
rakazo_token_env = "RAKAZO_TOKEN"
rakazo_space_id = "space-a"
bot_ids = []
auto_bots = true
auto_named_bots = ["Muse"]
"#,
        );
        assert!(tenant.is_ok());
    }

    const ACK_TENANT: &str = r#"
id = "personal"
owner = "@owner:example.com"
registration = "/tmp/registration.json"
homeserver = "https://matrix.example.com"
rakazo_url = "https://rakazo.example.com"
rakazo_token_env = "RAKAZO_TOKEN"
rakazo_space_id = "space-a"
bot_ids = []
"#;

    #[test]
    fn the_acknowledgement_policy_defaults_to_mid_thread_only() {
        let tenant: Tenant = toml::from_str(ACK_TENANT).unwrap();
        assert_eq!(tenant.ack_policy, AckPolicy::MidThreadOnly);
        assert_eq!(tenant.ack_due_seconds, 900);
        assert_eq!(tenant.ack_grace_seconds, 300);
    }

    #[test]
    fn the_acknowledgement_policy_is_explicit_when_it_is_set() {
        let tenant: Tenant = toml::from_str(&format!(
            "{ACK_TENANT}ack_policy = \"never\"\nack_due_seconds = 60\nack_grace_seconds = 30\n"
        ))
        .unwrap();
        assert_eq!(tenant.ack_policy, AckPolicy::Never);
        assert_eq!(tenant.ack_due_seconds, 60);
        assert_eq!(tenant.ack_grace_seconds, 30);
    }

    #[test]
    fn an_unknown_acknowledgement_policy_is_a_configuration_error() {
        let error =
            match toml::from_str::<Tenant>(&format!("{ACK_TENANT}ack_policy = \"sometimes\"\n")) {
                Ok(_) => panic!("an unknown policy must not parse"),
                Err(error) => error,
            };
        assert!(error.to_string().contains("sometimes"), "{error}");
    }
}
