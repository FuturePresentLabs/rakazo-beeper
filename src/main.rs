use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, Subcommand};
use rakazo_beeper::{
    config::{Config, secret},
    service::Runtime,
    store::Store,
    transport,
};
use rand::RngCore;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, default_value = "config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run bridge workers and the health/application-service HTTP server.
    Run,
    /// Validate configuration, secrets, registration bundles, and Rakazo access. No rooms or messages created.
    Check,
    /// List accessible Rakazo agents for one configured tenant.
    Bots {
        #[arg(long)]
        tenant: String,
    },
    /// Generate a random database encryption key. Save to your secret manager.
    GenerateKey,
    /// Pair through FPL device approval and save a new Rakazo session privately.
    /// Fleet orchestrator and PM entries in Rakazo (no bridge config required).
    Orch {
        #[command(subcommand)]
        command: OrchCommand,
    },
    Login {
        #[arg(long, default_value = "https://agents.fpl.dev")]
        rakazo_url: url::Url,
        #[arg(long, default_value = "https://sso.fpl.dev")]
        issuer: url::Url,
        #[arg(long, default_value = "rakazo-beeper")]
        client_id: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// ohmOS messaging client (offline by default; hidden while the phone hop is unproven).
    #[command(hide = true)]
    Ohm {
        #[arg(long, default_value = "ohm.toml")]
        ohm_config: PathBuf,
        #[command(subcommand)]
        command: OhmCommand,
    },
}

#[derive(Subcommand)]
enum OhmCommand {
    /// Health + inbox against a configured base URL. Prints counts only.
    Probe,
    /// Exercise the fake transport and pane bridge. No network.
    Selftest,
    /// Poll the daemon and inject routed conversations. Requires base URL + allow_send.
    Serve {
        /// Explicit orchestrator pane target, used when a route has no pane.
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 5)]
        interval_seconds: u64,
    },
}

#[derive(Subcommand)]
enum OrchCommand {
    /// Confirm the session, its owner, and the resolved space.
    Whoami,
    /// List agents visible to this session, marking roster entries.
    List,
    /// Compare the roster with live agents. Reports only unless --apply.
    Ensure {
        /// Create missing entries (idempotent per entry's spawnKey).
        #[arg(long)]
        apply: bool,
        /// Only entries whose key or name contains this text.
        #[arg(long)]
        only: Vec<String>,
    },
    /// Ask the orchestrator to spawn one PM sub-chat (starts one model run).
    ///
    /// Children can only be nested by the orchestrator's own `spawn_bot` tool.
    Spawn {
        /// Roster key, e.g. fleet-pm-marbles-v1.
        key: String,
    },
    /// Read one agent's conversation (read-only).
    Show {
        /// Agent id, spawnKey, or roster name.
        bot: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Send one message into an agent's conversation.
    Say {
        bot: String,
        text: String,
        /// Durable idempotency key; generated when omitted.
        #[arg(long)]
        nonce: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error:#}");
        std::process::exit(
            if error
                .downcast_ref::<transport::ConnectionReplaced>()
                .is_some()
            {
                78
            } else {
                1
            },
        );
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rakazo_beeper=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if matches!(cli.command, Command::GenerateKey) {
        let mut key = [0; 32];
        rand::thread_rng().fill_bytes(&mut key);
        println!("{}", STANDARD.encode(key));
        return Ok(());
    }
    if let Command::Orch { command } = &cli.command {
        return orch_command(command).await;
    }
    if let Command::Login {
        rakazo_url,
        issuer,
        client_id,
        output,
    } = &cli.command
    {
        return rakazo_beeper::login::login(rakazo_url, issuer, client_id, output).await;
    }
    if let Command::Ohm {
        ohm_config,
        command,
    } = &cli.command
    {
        return ohm_command(ohm_config, command).await;
    }
    let config = Config::load(&cli.config)?;
    // Read-only commands do not acquire the database lock or create database state.
    match &cli.command {
        Command::Check => {
            let key = STANDARD
                .decode(secret(&config.encryption_key_env)?)
                .context("invalid encryption key")?;
            anyhow::ensure!(key.len() == 32, "database key must decode to 32 bytes");
            for tenant in &config.tenants {
                rakazo_beeper::matrix::Matrix::new(tenant, &tenant.bundle()?)?;
                let bots = rakazo_beeper::rakazo::Rakazo::new(tenant)?.bots().await?;
                anyhow::ensure!(
                    tenant
                        .bot_ids
                        .iter()
                        .all(|id| bots.iter().any(|b| &b.id == id)),
                    "configured bot inaccessible"
                );
                println!("{}: configuration and Rakazo access verified", tenant.id);
            }
            return Ok(());
        }
        Command::Bots { tenant } => {
            let tenant = config
                .tenants
                .iter()
                .find(|t| &t.id == tenant)
                .context("unknown tenant")?;
            for bot in rakazo_beeper::rakazo::Rakazo::new(tenant)?.bots().await? {
                println!("{}\t{}", bot.id, bot.name);
            }
            return Ok(());
        }
        _ => {}
    }
    // Identity line for `run`: which build, which config, where it listens.
    // No secrets, tenants, owner IDs or message text.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        revision = %build_revision(),
        config = %cli.config.display(),
        listen = %config.listen,
        "rakazo-beeper starting"
    );
    let store = Arc::new(Store::open(
        &config.database,
        &secret(&config.encryption_key_env)?,
    )?);
    let runtime = Arc::new(Runtime::new(&config, store)?);
    let mut tasks = tokio::task::JoinSet::<Result<()>>::new();
    for tenant in &runtime.tenants {
        let worker = tenant.clone();
        let interval = config.poll_seconds;
        tasks.spawn(async move {
            loop {
                if let Err(error) = worker.tick().await {
                    tracing::warn!(tenant=%worker.config.id,error=%error,"bridge sync failed; retrying");
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        });
        if tenant.config.websocket {
            tasks.spawn(transport::websocket_loop(tenant.clone()));
        }
    }
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(listen=%config.listen,"bridge HTTP server listening");
    let app = transport::router(runtime);
    tasks.spawn(async move {
        axum::serve(listener, app)
            .await
            .context("HTTP server failed")
    });
    tokio::select! {
        signal = shutdown() => signal?,
        result = tasks.join_next() => {
            result.context("all bridge tasks stopped")??.context("bridge task stopped")?;
            anyhow::bail!("bridge task unexpectedly exited");
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    tracing::info!("bridge stopped; queued messages remain durable");
    Ok(())
}

/// Build revision stamped into the image/deployment; `unknown` when unset.
///
/// The value comes from the environment (no git call in the startup path).
fn build_revision() -> String {
    std::env::var("RAKAZO_BUILD_REVISION")
        .map(|value| value.trim().to_owned())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

const ORCH_HELP: &str =
    "set RAKAZO_ORCH_TOKEN_FILE (owner-only file) or RAKAZO_ORCH_TOKEN, plus RAKAZO_ORCH_SPACE_ID";

async fn orch_command(command: &OrchCommand) -> Result<()> {
    use rakazo_beeper::orch::{Orch, roster};

    let orch = Orch::from_env().with_context(|| ORCH_HELP.to_string())?;
    match command {
        OrchCommand::Whoami => {
            let (user, space) = orch.whoami().await?;
            println!("user\t{user}");
            println!("space\t{space}");
            anyhow::ensure!(
                space == orch.space,
                "session resolves to space {space}, not the configured {}",
                orch.space
            );
        }
        OrchCommand::List => {
            let bots = orch.bots().await?;
            let mut roster_ids = std::collections::HashSet::new();
            for entry in roster() {
                match orch.resolve(&bots, &entry) {
                    Some(bot) => {
                        roster_ids.insert(bot.id.clone());
                        println!(
                            "{}\t{}\t{}\tparent={}",
                            bot.id,
                            entry.name,
                            entry.key,
                            bot.parent_bot_id.as_deref().unwrap_or("-")
                        );
                    }
                    None => println!("-\t{}\t{}\tmissing", entry.name, entry.key),
                }
            }
            for bot in bots.iter().filter(|bot| !roster_ids.contains(&bot.id)) {
                println!("{}\t{}\t(not a fleet entry)", bot.id, bot.name);
            }
        }
        OrchCommand::Ensure { apply, only } => {
            let entries: Vec<_> = roster()
                .into_iter()
                .filter(|entry| {
                    only.is_empty()
                        || only.iter().any(|needle| {
                            entry.key.contains(needle.as_str())
                                || entry.name.contains(needle.as_str())
                        })
                })
                .collect();
            anyhow::ensure!(!entries.is_empty(), "no roster entry matched --only");
            let bots = orch.bots().await?;
            for entry in entries {
                match orch.resolve(&bots, &entry) {
                    Some(bot) => println!("present\t{}\t{}\t{}", bot.id, entry.name, entry.key),
                    None if *apply && entry.kind == rakazo_beeper::orch::Kind::Root => {
                        let bot = orch.create(&entry).await?;
                        println!("created\t{}\t{}\t{}", bot.id, entry.name, entry.key);
                    }
                    None if entry.kind == rakazo_beeper::orch::Kind::Child => println!(
                        "missing\t-\t{}\t{}\t(child: run `orch spawn {}`)",
                        entry.name, entry.key, entry.key
                    ),
                    None => println!("missing\t-\t{}\t{}", entry.name, entry.key),
                }
            }
            if !apply {
                println!("dry run: no entries created (pass --apply to create)");
            }
        }
        OrchCommand::Spawn { key } => {
            let entry = roster()
                .into_iter()
                .find(|entry| entry.key == key.as_str() || entry.name == key.as_str())
                .with_context(|| format!("no roster entry matches {key}"))?;
            anyhow::ensure!(
                entry.kind == rakazo_beeper::orch::Kind::Child,
                "{} is a root entry; `orch ensure --apply` creates it",
                entry.name
            );
            let bots = orch.bots().await?;
            let root = orch
                .resolve(&bots, &roster()[0])
                .context("orchestrator entry missing; run `orch ensure --apply` first")?;
            if let Some(child) = orch.resolve(&bots, &entry) {
                println!("present\t{}\t{}\t{}", child.id, entry.name, entry.key);
                return Ok(());
            }
            let nonce = format!("spawn-{}-{}", entry.key, random_nonce());
            let seq = orch
                .say(&root.id, &Orch::spawn_prompt(&entry), &nonce)
                .await?;
            println!("spawning\tseq={seq}\tnonce={nonce}\t{}", entry.name);
        }
        OrchCommand::Show { bot, limit } => {
            let id = resolve_id(&orch, bot).await?;
            for (seq, role, text) in orch.recent(&id, *limit).await? {
                println!("#{seq}\t{role}\t{}", text.replace('\n', " ⏎ "));
            }
        }
        OrchCommand::Say { bot, text, nonce } => {
            let id = resolve_id(&orch, bot).await?;
            let nonce = nonce.clone().unwrap_or_else(random_nonce);
            let seq = orch.say(&id, text, &nonce).await?;
            println!("sent\tseq={seq}\tnonce={nonce}");
        }
    }
    Ok(())
}

/// Accept an agent id, its spawnKey, or a roster name.
async fn resolve_id(orch: &rakazo_beeper::orch::Orch, needle: &str) -> Result<String> {
    use rakazo_beeper::orch::roster;

    let bots = orch.bots().await?;
    let matched: Vec<_> = bots
        .iter()
        .filter(|bot| {
            bot.id == needle
                || bot.spawn_key.as_deref() == Some(needle)
                || roster()
                    .iter()
                    .any(|entry| entry.key == needle && bot.spawn_key.as_deref() == Some(entry.key))
                || bot.name == needle
        })
        .collect();
    match matched.as_slice() {
        [bot] => Ok(bot.id.clone()),
        [] => anyhow::bail!("no agent matches {needle}"),
        many => anyhow::bail!("ambiguous agent name {needle}: {} matches", many.len()),
    }
}

fn random_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::Engine::encode(&STANDARD, bytes)
}

/// Hidden ohmOS commands. `probe` is read-only; `serve` refuses to start without
/// a configured base URL *and* explicit send opt-in.
async fn ohm_command(path: &PathBuf, command: &OhmCommand) -> Result<()> {
    use rakazo_beeper::ohm::{
        Bridge, FakeNotifier, FakePaneIo, FakeTransport, MsgdClient, OhmConfig, Platform, Route,
        inbound,
    };

    match command {
        OhmCommand::Probe => {
            let config = OhmConfig::load(path)?;
            let base = config
                .base_url
                .as_deref()
                .context("ohm config has no base_url; no production value is built in")?;
            let client = MsgdClient::new(base)?;
            client.health()?;
            let inbox = client.inbox()?;
            println!("health\tok");
            println!("inbox\t{}\tchats", inbox.len());
            println!(
                "send\t{}",
                if client.send_allowed() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }
        OhmCommand::Selftest => {
            let config = OhmConfig {
                base_url: None,
                allow_send: false,
                state_file: None,
                orchestrator_targets: vec!["%0".into()],
                routes: vec![Route {
                    chat_id: 7,
                    roster_key: "fleet-orch-v1".into(),
                    pane: Some("%selftest".into()),
                    platform: Platform::WhatsApp,
                    remote_id: "selftest-remote".into(),
                    recipient: None,
                }],
            };
            let msg = inbound(7, Platform::WhatsApp, "selftest-remote", 41, "selftest");
            let transport = FakeTransport::new(vec![vec![msg.clone()], vec![msg]]);
            let mut bridge = Bridge::new(
                &config,
                None,
                transport,
                FakePaneIo::default(),
                FakeNotifier::default(),
            )?;
            let first = bridge.poll_once()?;
            let second = bridge.poll_once()?;
            anyhow::ensure!(
                first.injected == 1 && second.injected == 0 && second.duplicates == 1,
                "ohm selftest dedupe failed"
            );
            println!(
                "selftest\tok\tinjected={}\tduplicates={}",
                first.injected, second.duplicates
            );
        }
        OhmCommand::Serve {
            target,
            interval_seconds,
        } => {
            let config = OhmConfig::load(path)?;
            let Some(base) = config.base_url.as_deref() else {
                println!("ohm serve disabled: no base_url configured");
                return Ok(());
            };
            if !config.allow_send {
                println!("ohm serve disabled: allow_send is false");
                return Ok(());
            }
            anyhow::ensure!(*interval_seconds >= 1, "interval must be positive");
            let client = MsgdClient::new(base)?.with_send_allowed(true);
            let transport = rakazo_beeper::ohm::HttpTransport::new(client);
            let mut bridge = Bridge::new(
                &config,
                target.clone(),
                transport,
                rakazo_beeper::ohm::TmuxPaneIo::new(config.orchestrator_targets.clone()),
                rakazo_beeper::ohm::PiNotifyNotifier,
            )?;
            println!("ohm serve: polling (send enabled; replies stay queued)");
            loop {
                if let Err(error) = bridge.poll_once() {
                    tracing::warn!(error = %error, "ohm poll failed; retrying");
                }
                tokio::time::sleep(Duration::from_secs(*interval_seconds)).await;
            }
        }
    }
    Ok(())
}
