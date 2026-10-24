//! Client configuration for the Rakazo IRC frontend.
//!
//! The local rehearsal only needs loopback host/port, a nick, channels and an
//! inbox path. The live hop adds TLS with a private CA and a ZNC password file;
//! both are validated here so a missing or world-readable secret fails closed
//! before any socket is opened.

use anyhow::{Context, Result, bail, ensure};
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
    /// Use TLS for the server hop. Off by default so loopback rehearsals stay plain.
    #[serde(default)]
    pub tls: bool,
    /// PEM bundle used to verify the server. Required when `tls = true`.
    pub cafile: Option<PathBuf>,
    /// File holding the ZNC password. Required when `tls = true`; never logged.
    pub password_file: Option<PathBuf>,
    #[serde(default = "nick_default")]
    pub nick: String,
    /// IRC `USER` name. Defaults to `nick` (ZNC uses the account name here).
    pub user: Option<String>,
    #[serde(default = "channels_default")]
    pub channels: Vec<String>,
    #[serde(default = "inbox_default")]
    pub inbox: PathBuf,
    /// Verify that the certificate names the configured host. ZNC's certificate
    /// is issued for its IP, so set this to `false` only when connecting by a
    /// name the certificate does not carry. Chain verification always happens.
    #[serde(default = "verify_hostname_default")]
    pub verify_hostname: bool,
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
fn verify_hostname_default() -> bool {
    true
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
            tls: false,
            cafile: None,
            password_file: None,
            nick: nick_default(),
            user: None,
            channels: channels_default(),
            inbox: inbox_default(),
            verify_hostname: verify_hostname_default(),
            idle_seconds: idle_default(),
            max_backoff_seconds: backoff_default(),
        }
    }
}

/// Expand a leading `~/` against `$HOME`. Keeps committed examples portable
/// without pulling in a directory-handling crate.
pub fn expand_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

impl IrcConfig {
    /// Load `path` when it exists; otherwise use loopback defaults.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read IRC config {}", path.display()))?;
            toml::from_str(&text).context("invalid IRC client configuration")?
        } else {
            Self::default()
        };
        config.resolve_paths();
        config.validate()?;
        Ok(config)
    }

    /// Expand `~/` in paths carried by the config. Secret files are expanded at
    /// use time as well, so a CLI override behaves the same as a file value.
    pub fn resolve_paths(&mut self) {
        self.inbox = expand_path(&self.inbox);
        if let Some(cafile) = self.cafile.take() {
            self.cafile = Some(expand_path(&cafile));
        }
        if let Some(password_file) = self.password_file.take() {
            self.password_file = Some(expand_path(&password_file));
        }
    }

    pub fn user_name(&self) -> &str {
        self.user.as_deref().unwrap_or(&self.nick)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.host.trim().is_empty(), "IRC host must not be empty");
        ensure!(self.port > 0, "IRC port must be positive");
        ensure!(!self.nick.trim().is_empty(), "IRC nick must not be empty");
        ensure!(
            !self.nick.contains([' ', ',', ':']),
            "IRC nick must not contain spaces, commas or colons"
        );
        let user = self.user_name();
        ensure!(!user.trim().is_empty(), "IRC user must not be empty");
        ensure!(
            !user.contains([' ', ',', ':']),
            "IRC user must not contain spaces, commas or colons"
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
        self.validate_secret_files()?;
        Ok(())
    }

    /// TLS is fail-closed: no trust anchor or no password file means no connect.
    /// Errors name paths and never include file contents.
    fn validate_secret_files(&self) -> Result<()> {
        if !self.tls {
            return Ok(());
        }
        let cafile = self
            .cafile
            .as_ref()
            .context("TLS is enabled but no cafile is configured")?;
        let cafile = expand_path(cafile);
        ensure!(
            cafile.is_file(),
            "TLS cafile {} does not exist or is not a file",
            cafile.display()
        );
        let password_file = self
            .password_file
            .as_ref()
            .context("TLS is enabled but no password_file is configured")?;
        let password_file = expand_path(password_file);
        ensure!(
            password_file.is_file(),
            "TLS password_file {} does not exist or is not a file",
            password_file.display()
        );
        let metadata = std::fs::metadata(&password_file)
            .with_context(|| format!("stat TLS password_file {}", password_file.display()))?;
        ensure!(
            metadata.len() > 0,
            "TLS password_file {} is empty",
            password_file.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                bail!(
                    "TLS password_file {} must be mode 0600 (found {:03o})",
                    password_file.display(),
                    mode
                );
            }
        }
        Ok(())
    }

    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.idle_seconds)
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Redacted identity line for `irc whoami`. Never includes a secret, and the
    /// password path is only a path.
    pub fn whoami(&self) -> String {
        format!(
            "nick={} user={} channels={} transport={} host={} inbox={}",
            self.nick,
            self.user_name(),
            self.channels.join(","),
            if self.tls { "tls" } else { "plain" },
            self.address(),
            self.inbox.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tls_config(cafile: Option<PathBuf>, password_file: Option<PathBuf>) -> IrcConfig {
        IrcConfig {
            tls: true,
            cafile,
            password_file,
            ..IrcConfig::default()
        }
    }

    fn secret_file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[test]
    fn tls_without_cafile_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let password = secret_file(dir.path(), "pass", "secret\n");
        let error = tls_config(None, Some(password))
            .validate()
            .expect_err("missing cafile must fail");
        assert!(format!("{error:#}").contains("cafile"));
    }

    #[test]
    fn tls_without_password_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let cafile = secret_file(dir.path(), "ca.pem", "unused\n");
        let error = tls_config(Some(cafile), None)
            .validate()
            .expect_err("missing password file must fail");
        assert!(format!("{error:#}").contains("password_file"));
    }

    #[test]
    fn tls_with_missing_files_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let error = tls_config(
            Some(dir.path().join("no-ca.pem")),
            Some(dir.path().join("no-pass")),
        )
        .validate()
        .expect_err("missing paths must fail");
        assert!(format!("{error:#}").contains("does not exist"));
    }

    #[test]
    fn world_readable_password_file_is_rejected() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let cafile = secret_file(dir.path(), "ca.pem", "unused\n");
            let password = secret_file(dir.path(), "pass", "secret\n");
            std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = tls_config(Some(cafile), Some(password))
                .validate()
                .expect_err("world-readable password must fail");
            assert!(format!("{error:#}").contains("0600"));
        }
    }

    #[test]
    fn whoami_never_prints_secret_material() {
        let dir = tempfile::tempdir().unwrap();
        let cafile = secret_file(dir.path(), "ca.pem", "unused\n");
        let password = secret_file(dir.path(), "pass", "hunter2\n");
        let config = tls_config(Some(cafile), Some(password.clone()));
        config.validate().unwrap();
        let line = config.whoami();
        assert!(line.contains("nick=rakazo"));
        assert!(line.contains("user=rakazo"));
        assert!(line.contains("#orch"));
        assert!(!line.contains("hunter2"));
        assert!(!line.contains(&password.display().to_string()));
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"));
    }
}
