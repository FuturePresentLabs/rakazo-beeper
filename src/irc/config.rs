//! Client configuration for the Rakazo IRC frontend.
//!
//! The local rehearsal only needs loopback host/port, a nick, channels and an
//! inbox path. Certificate paths are carried for the later TLS/ZNC hop but are
//! deliberately unused here: this increment adds no TLS stack.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrcConfig {
    #[serde(default = "host_default")]
    pub host: String,
    #[serde(default = "port_default")]
    pub port: u16,
    #[serde(default = "nick_default")]
    pub nick: String,
    #[serde(default = "channels_default")]
    pub channels: Vec<String>,
    #[serde(default = "inbox_default")]
    pub inbox: PathBuf,
    /// Reserved for the TLS/ZNC principal hop; not read by the local client.
    pub cert: Option<PathBuf>,
    /// Reserved for the TLS/ZNC principal hop; not read by the local client.
    pub key: Option<PathBuf>,
    /// Seconds of quiet after which `connect --once` returns.
    #[serde(default = "idle_default")]
    pub idle_seconds: u64,
    /// Cap on reconnect backoff for the long-running client.
    #[serde(default = "backoff_default")]
    pub max_backoff_seconds: u64,
}

fn host_default() -> String {
    "127.0.0.1".into()
}
fn port_default() -> u16 {
    6667
}
fn nick_default() -> String {
    "rakazo".into()
}
fn channels_default() -> Vec<String> {
    vec!["#orch".into()]
}
fn inbox_default() -> PathBuf {
    PathBuf::from("irc-inbox.sqlite")
}
fn idle_default() -> u64 {
    2
}
fn backoff_default() -> u64 {
    30
}

impl Default for IrcConfig {
    fn default() -> Self {
        Self {
            host: host_default(),
            port: port_default(),
            nick: nick_default(),
            channels: channels_default(),
            inbox: inbox_default(),
            cert: None,
            key: None,
            idle_seconds: idle_default(),
            max_backoff_seconds: backoff_default(),
        }
    }
}

impl IrcConfig {
    /// Load `path` when it exists; otherwise use loopback defaults.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        let config = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read IRC config {}", path.display()))?;
            toml::from_str(&text).context("invalid IRC client configuration")?
        } else {
            Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.host.trim().is_empty(), "IRC host must not be empty");
        ensure!(self.port > 0, "IRC port must be positive");
        ensure!(!self.nick.trim().is_empty(), "IRC nick must not be empty");
        ensure!(
            !self.nick.contains([' ', ',', ':']),
            "IRC nick must not contain spaces, commas or colons"
        );
        ensure!(
            !self.channels.is_empty(),
            "configure at least one IRC channel"
        );
        for channel in &self.channels {
            ensure!(
                channel.starts_with('#') && !channel.contains(' '),
                "IRC channel {channel:?} must start with '#' and contain no spaces"
            );
        }
        ensure!(
            !self.inbox.as_os_str().is_empty(),
            "IRC inbox path must not be empty"
        );
        ensure!(self.idle_seconds >= 1, "idle_seconds must be positive");
        ensure!(
            self.max_backoff_seconds >= 1,
            "max_backoff_seconds must be positive"
        );
        Ok(())
    }

    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.idle_seconds)
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
