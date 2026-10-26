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
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

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
    /// IRC client for the fleet bus (local rehearsal; hidden until the SSO answer).
    #[command(hide = true)]
    Irc {
        /// IRC config file; defaults to ~/.config/rakazo/irc.toml.
        #[arg(long = "irc-config", default_value_os_t = default_irc_config())]
        irc_config: PathBuf,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        nick: Option<String>,
        /// Repeatable; defaults to #orch from the config.
        #[arg(long = "channel")]
        channels: Vec<String>,
        #[arg(long)]
        inbox: Option<PathBuf>,
        #[arg(long)]
        tls: bool,
        #[arg(long)]
        cafile: Option<PathBuf>,
        #[arg(long)]
        password_file: Option<PathBuf>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        no_verify_hostname: bool,
        #[command(subcommand)]
        command: IrcCommand,
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
enum IrcCommand {
    /// Connect, join configured channels, store inbound rows, and print only new ones.
    Connect {
        /// Exit after the configured quiet window instead of reconnecting forever.
        #[arg(long)]
        once: bool,
        /// Quiet window for --once, overriding idle_seconds.
        #[arg(long)]
        seconds: Option<u64>,
    },
    /// Send one message (chunked within the 512-byte line budget when needed).
    Send {
        #[arg(long, default_value = "#orch")]
        target: String,
        #[arg(long)]
        text: String,
    },
    /// Add a reaction, tagged when supported and plain-text otherwise.
    React {
        #[arg(long, default_value = "#orch")]
        target: String,
        #[arg(long)]
        msgid: String,
        #[arg(long)]
        emoji: String,
    },
    /// Reply to a message, tagged when supported and quote-line otherwise.
    Reply {
        #[arg(long, default_value = "#orch")]
        target: String,
        #[arg(long)]
        msgid: String,
        #[arg(long)]
        text: String,
    },
    /// Backfill the durable inbox from a fleet-bus IRC transcript.
    Ingest {
        #[arg(long)]
        transcript: PathBuf,
        #[arg(long, default_value = "#orch")]
        target: String,
        #[arg(long, default_value_t = 1)]
        limit: usize,
    },
    /// Print nick, user, channels and transport without any secret material.
    Whoami,
    /// Offline proof of capability handling, chunk budgets and source identity.
    Selftest,
}

/// The live IRC client's default config lives beside its password file.
fn default_irc_config() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/rakazo/irc.toml"),
        None => PathBuf::from("irc.toml"),
    }
}

#[derive(Subcommand)]
enum OrchCommand {
    /// Confirm the session, its owner, and the resolved space.
    Whoami,
    /// List agents visible to this session, marking roster entries.
    List {
        /// Emit one JSON array instead of tabs.
        #[arg(long)]
        json: bool,
    },
    /// List the derived roster (orchestrator + every configured vertical)
    /// without a Rakazo session. Live state comes from `pi-pm list --tsv`.
    Roster {
        /// Emit one JSON array instead of tabs.
        #[arg(long)]
        json: bool,
    },
    /// Show the conversation -> pane routing table derived from the roster.
    Route {
        /// Emit one JSON array instead of tabs.
        #[arg(long)]
        json: bool,
    },
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
    /// Relay new user messages from the allowlisted conversation to the
    /// orchestrator notification queue (inbound only; no assistant append).
    Watch {
        /// Poll once and exit (the default).
        #[arg(long)]
        once: bool,
        /// Poll forever at this interval. The live loop requires an explicit value.
        #[arg(long, value_name = "SECONDS")]
        interval_s: Option<u64>,
        /// Dedupe/watermark file. Without it, state lives only for this process.
        #[arg(long)]
        state_file: Option<std::path::PathBuf>,
        /// `pi-notify --worker` target.
        #[arg(long, default_value = "fleet-orch-v1")]
        worker_name: String,
    },
    /// Reply through a linked messaging attachment only. There is no
    /// assistant-append RPC, so a normal Agent conversation reports
    /// `unsupported: no assistant-append RPC` and sends nothing.
    Reply {
        /// Text to post on the attachment surface.
        text: String,
        /// Print the would-be surface without sending anything.
        #[arg(long)]
        dry_run: bool,
        /// UUID idempotency key; generated when omitted.
        #[arg(long)]
        nonce: Option<String>,
    },
    /// Machine-readable health/watermark state (no secrets). Exits non-zero if
    /// the app health probe or the bearer token is not healthy.
    Status {
        /// Dedupe/watermark file used by `watch`.
        #[arg(long)]
        state_file: Option<std::path::PathBuf>,
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

/// The bridge has a global `--config`; inside the `irc` subcommand the same
/// spelling means the IRC config. Rewrite only arguments that appear after the
/// `irc` subcommand so `--config bridge.toml irc ...` keeps its meaning.
fn rewrite_irc_config_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut args: Vec<String> = args.into_iter().collect();
    let mut in_irc = false;
    for arg in args.iter_mut().skip(1) {
        if arg == "irc" {
            in_irc = true;
            continue;
        }
        if !in_irc {
            continue;
        }
        if arg == "--config" {
            *arg = "--irc-config".into();
        } else if let Some(value) = arg.strip_prefix("--config=") {
            *arg = format!("--irc-config={value}");
        }
    }
    args
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rakazo_beeper=info".into()),
        )
        .init();
    let cli = Cli::parse_from(rewrite_irc_config_args(std::env::args()));
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
    if let Command::Irc {
        irc_config,
        host,
        port,
        nick,
        channels,
        inbox,
        tls,
        cafile,
        password_file,
        user,
        no_verify_hostname,
        command,
    } = &cli.command
    {
        return irc_command(
            irc_config,
            host,
            *port,
            nick,
            channels,
            inbox,
            *tls,
            cafile,
            password_file,
            user,
            *no_verify_hostname,
            command,
        );
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

const ORCH_HELP: &str = "set RAKAZO_ORCH_TOKEN_FILE (owner-only file) or RAKAZO_ORCH_TOKEN, RAKAZO_ORCH_SPACE_ID, \
     and for watch/reply/status RAKAZO_ORCH_BOT_ID + RAKAZO_ORCH_THREAD_ID";

/// Print the derived roster. `bots` is `None` for the offline `orch roster`
/// view; when present, each entry is matched to its live Rakazo agent.
fn print_roster(
    entries: &[rakazo_beeper::orch::Entry],
    bots: Option<&[rakazo_beeper::rakazo::Bot]>,
    json: bool,
) -> Result<()> {
    use rakazo_beeper::orch;

    let rows: Vec<_> = entries
        .iter()
        .map(|entry| {
            let bot = bots.and_then(|bots| orch::resolve(bots, entry));
            let live = entry.live.as_ref();
            (
                entry,
                bot,
                live.map(|live| {
                    format!(
                        "{}:{}{}",
                        live.pane.as_deref().unwrap_or("<no-pane>"),
                        live.status,
                        if live.phase.is_empty() {
                            String::new()
                        } else {
                            format!(":{}", live.phase)
                        }
                    )
                }),
            )
        })
        .collect();

    if json {
        let value: Vec<_> = rows
            .iter()
            .map(|(entry, bot, live)| {
                serde_json::json!({
                    "key": entry.key,
                    "name": entry.name,
                    "title": entry.title,
                    "kind": entry.kind.as_str(),
                    "paused": entry.paused,
                    "bot_id": bot.map(|bot| bot.id.as_str()),
                    "parent_bot_id": bot.and_then(|bot| bot.parent_bot_id.as_deref()),
                    "live": entry.live.as_ref().map(|state| serde_json::json!({
                        "session": state.session,
                        "pane": state.pane,
                        "status": state.status,
                        "phase": state.phase,
                        "workers": state.workers,
                    })),
                    "pane": live,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    for (entry, bot, live) in rows {
        let id = bot.map(|bot| bot.id.as_str()).unwrap_or("-");
        let state = match (bot, entry.paused) {
            (_, true) => "paused",
            (Some(_), false) => "present",
            (None, false) => "missing",
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\tlive={}",
            id,
            entry.name,
            entry.key,
            entry.kind.as_str(),
            state,
            live.as_deref().unwrap_or("offline"),
        );
    }
    Ok(())
}

async fn orch_command(command: &OrchCommand) -> Result<()> {
    use rakazo_beeper::orch::{Entry, Kind, Orch, roster};

    // The roster and its route table are local: they need no Rakazo session, so
    // an offline host can still see every configured vertical.
    if let OrchCommand::Roster { json } = command {
        print_roster(&roster::load()?, None, *json)?;
        return Ok(());
    }

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
        OrchCommand::Roster { .. } => unreachable!("handled before the session is required"),
        OrchCommand::List { json } => {
            let bots = orch.bots().await?;
            let entries = roster::load()?;
            if *json {
                print_roster(&entries, Some(&bots), true)?;
            } else {
                print_roster(&entries, Some(&bots), false)?;
                let roster_ids: std::collections::HashSet<_> = entries
                    .iter()
                    .filter_map(|entry| orch.resolve(&bots, entry).map(|bot| bot.id.clone()))
                    .collect();
                for bot in bots.iter().filter(|bot| !roster_ids.contains(&bot.id)) {
                    println!("{}\t{}\t(not a fleet entry)", bot.id, bot.name);
                }
            }
        }
        OrchCommand::Route { json } => {
            let bots = orch.bots().await?;
            let entries = roster::load()?;
            let table = orch.route_table(&bots, &entries)?;
            if *json {
                let rows: Vec<_> = table
                    .entries()
                    .iter()
                    .map(|row| {
                        serde_json::json!({
                            "key": row.bot_key().as_key(),
                            "conversation": row.conversation_id(),
                            "pane": row.pane_id(),
                            "paused": row.is_paused(),
                            "parent": row.parent().map(|parent| parent.as_key()),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for row in table.entries() {
                    println!(
                        "{}\t{}\t{}\tpaused={}\tparent={}",
                        row.bot_key(),
                        row.conversation_id().unwrap_or("-"),
                        row.pane_id().unwrap_or("<unrouted>"),
                        row.is_paused(),
                        row.parent()
                            .map(|parent| parent.as_key())
                            .unwrap_or_else(|| "-".into()),
                    );
                }
            }
        }
        OrchCommand::Ensure { apply, only } => {
            let entries: Vec<_> = roster::load()?
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
                if entry.paused {
                    println!("paused\t-\t{}\t{}", entry.name, entry.key);
                    continue;
                }
                match orch.resolve(&bots, &entry) {
                    Some(bot) => println!("present\t{}\t{}\t{}", bot.id, entry.name, entry.key),
                    None if *apply && entry.kind == Kind::Root => {
                        let bot = orch.create(&entry).await?;
                        println!("created\t{}\t{}\t{}", bot.id, entry.name, entry.key);
                    }
                    None if entry.kind == Kind::Child => println!(
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
            let entries = roster::load()?;
            let entry: &Entry = entries
                .iter()
                .find(|entry| entry.key == key.as_str() || entry.name == key.as_str())
                .with_context(|| format!("no roster entry matches {key}"))?;
            anyhow::ensure!(
                entry.kind == Kind::Child,
                "{} is a root entry; `orch ensure --apply` creates it",
                entry.name
            );
            anyhow::ensure!(
                !entry.paused,
                "{} is paused in verticals.toml; resume the vertical before spawning it",
                entry.name
            );
            let bots = orch.bots().await?;
            let root = entries
                .iter()
                .find(|entry| entry.kind == Kind::Root)
                .context("roster is missing its orchestrator entry")
                .and_then(|entry| {
                    orch.resolve(&bots, entry)
                        .context("orchestrator entry missing; run `orch ensure --apply` first")
                })?;
            if let Some(child) = orch.resolve(&bots, entry) {
                println!("present\t{}\t{}\t{}", child.id, entry.name, entry.key);
                return Ok(());
            }
            let nonce = format!("spawn-{}-{}", entry.key, random_nonce());
            let seq = orch
                .say(&root.id, &Orch::spawn_prompt(entry), &nonce)
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
        OrchCommand::Watch {
            once,
            interval_s,
            state_file,
            worker_name,
        } => {
            use rakazo_beeper::orch::watch::{PiNotify, Watch};

            let allowlist = allowlist(&orch)?;
            let state_file = state_file
                .clone()
                .or_else(|| std::env::var_os("RAKAZO_ORCH_STATE_FILE").map(Into::into));
            let mut watch = Watch::new(
                orch.rakazo.clone(),
                PiNotify,
                worker_name,
                state_file,
                allowlist.clone(),
            )?;
            let bound = watch.bind().await?;
            let interval = match interval_s {
                Some(seconds) if !once => Some(std::time::Duration::from_secs(*seconds)),
                _ => None,
            };
            anyhow::ensure!(
                interval.is_none_or(|i| !i.is_zero()),
                "interval must be positive"
            );
            println!(
                "watching\t{}\t{}\tref=rakazo:{}",
                bound.id, bound.name, allowlist.thread_id
            );
            loop {
                match watch.poll_once().await {
                    Ok(report) => println!(
                        "poll\tdelivered={}\tduplicates={}\tignored={}",
                        report.delivered, report.duplicates, report.ignored
                    ),
                    Err(error) => {
                        tracing::warn!(error = %error, "watch poll failed; watermark unchanged");
                        eprintln!("watch poll failed: {error:#}");
                    }
                }
                match interval {
                    Some(interval) => tokio::time::sleep(interval).await,
                    None => break,
                }
            }
        }
        OrchCommand::Reply {
            text,
            dry_run,
            nonce,
        } => {
            use rakazo_beeper::orch::reply::{self, Plan};

            let allowlist = allowlist(&orch)?;
            let bots = orch.bots().await?;
            let bot = bots
                .iter()
                .find(|bot| bot.id == allowlist.bot_id)
                .with_context(|| {
                    format!(
                        "allowlisted bot {} is not visible to this session",
                        allowlist.bot_id
                    )
                })?;
            allowlist.check(bot)?;
            let nonce = nonce.clone().unwrap_or_else(reply::nonce);
            if orch.assistant.is_some() {
                if *dry_run {
                    println!("surface=assistant\tbot={}\tdry_run=true", bot.id);
                } else {
                    let receipt = orch.reply_assistant(&bot.id, text, &nonce, None).await?;
                    println!(
                        "surface=assistant\tbot={}\tmessage={}\tseq={}\tnonce={}",
                        bot.id, receipt.message_id, receipt.seq, nonce
                    );
                }
                return Ok(());
            }
            let plan = if *dry_run {
                reply::plan(&orch.rakazo, bot).await?
            } else {
                reply::reply(&orch.rakazo, bot, text, &nonce).await?
            };
            match plan {
                Plan::Attachment {
                    attachment_id,
                    label,
                } => {
                    if *dry_run {
                        println!(
                            "surface=attachment\tattachment={attachment_id}\tlabel={label}\tdry_run=true"
                        );
                    } else {
                        println!(
                            "surface=attachment\tattachment={attachment_id}\tlabel={label}\tnonce={nonce}"
                        );
                    }
                }
                Plan::Unsupported(reason) => anyhow::bail!("{reason}"),
            }
        }
        OrchCommand::Status { state_file } => {
            use rakazo_beeper::orch::watch::{WatchState, now_rfc3339};

            let allowlist = allowlist(&orch)?;
            let state_file = state_file
                .clone()
                .or_else(|| std::env::var_os("RAKAZO_ORCH_STATE_FILE").map(Into::into));
            let state = WatchState::load(state_file.as_deref())?;
            let conversation = state
                .thread_id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| allowlist.thread_id.clone());
            let bot_id = if state.bot_id.is_empty() {
                allowlist.bot_id.clone()
            } else {
                state.bot_id.clone()
            };

            let at = now_rfc3339();
            let healthz = match orch.rakazo.healthz().await {
                Ok(code) if code == 200 => serde_json::json!({
                    "status": "ok", "code": code, "at": at
                }),
                Ok(code) => serde_json::json!({
                    "status": "unhealthy", "code": code, "at": at
                }),
                Err(error) => serde_json::json!({
                    "status": "error", "error": format!("{error:#}"), "at": at
                }),
            };
            let token = match orch.whoami().await {
                Ok((_user, space)) if space == allowlist.space_id && space == orch.space => {
                    serde_json::json!({"status": "valid", "at": at})
                }
                Ok((_user, space)) => serde_json::json!({
                    "status": "wrong-space", "space": space, "at": at
                }),
                Err(error)
                    if error
                        .downcast_ref::<rakazo_beeper::rakazo::Unauthorized>()
                        .is_some() =>
                {
                    serde_json::json!({"status": "invalid", "code": 401, "at": at})
                }
                Err(error) => serde_json::json!({
                    "status": "error", "error": format!("{error:#}"), "at": at
                }),
            };
            let status = serde_json::json!({
                "bot_id": bot_id,
                "conversation_id": conversation,
                "thread_id": allowlist.thread_id,
                "space_id": allowlist.space_id,
                "watermark": state.last_seq,
                "last_seq": state.last_seq,
                "delivered": state.delivered_total,
                "duplicates": state.duplicates_total,
                "last_delivered_id": state.last_delivered_id,
                "last_poll_ok": state.last_poll_ok,
                "last_poll_at": state.last_poll_at,
                "last_error": state.last_error,
                // E-847 waiver: the poller uses the human session token read-only
                // until the service token lands; revoke at cutover.
                "auth_mode": "session-waiver (expires on service-token rollout)",
                "upstream": {"healthz": healthz, "token": token},
            });
            println!("{}", serde_json::to_string(&status)?);
            let healthy = status["upstream"]["healthz"]["status"] == "ok"
                && status["upstream"]["token"]["status"] == "valid";
            anyhow::ensure!(healthy, "orch status degraded");
        }
    }
    Ok(())
}

/// Read the allowlist and fail unless it names the same space as the session.
fn allowlist(orch: &rakazo_beeper::orch::Orch) -> Result<rakazo_beeper::orch::Allowlist> {
    let allowlist = rakazo_beeper::orch::Allowlist::from_env()?;
    anyhow::ensure!(
        allowlist.space_id == orch.space,
        "allowlist space {} differs from RAKAZO_ORCH_SPACE_ID {}",
        allowlist.space_id,
        orch.space
    );
    Ok(allowlist)
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
                || roster().unwrap_or_default().iter().any(|entry| {
                    entry.key == needle && bot.spawn_key.as_deref() == Some(entry.key.as_str())
                })
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

/// Hidden IRC commands. The live hop is TLS to ZNC with a private CA; secrets
/// come from files and are never printed.
#[allow(clippy::too_many_arguments)]
fn irc_command(
    path: &Path,
    host: &Option<String>,
    port: Option<u16>,
    nick: &Option<String>,
    channels: &[String],
    inbox: &Option<PathBuf>,
    tls: bool,
    cafile: &Option<PathBuf>,
    password_file: &Option<PathBuf>,
    user: &Option<String>,
    no_verify_hostname: bool,
    command: &IrcCommand,
) -> Result<()> {
    use rakazo_beeper::irc::{IrcConfig, run};

    let mut config = IrcConfig::load_or_default(path)?;
    if let Some(host) = host {
        config.host = host.clone();
    }
    if let Some(port) = port {
        config.port = port;
    }
    if let Some(nick) = nick {
        config.nick = nick.clone();
    }
    if !channels.is_empty() {
        config.channels = channels.to_vec();
    }
    if let Some(inbox) = inbox {
        config.inbox = inbox.clone();
    }
    if tls {
        config.tls = true;
    }
    if let Some(cafile) = cafile {
        config.cafile = Some(cafile.clone());
    }
    if let Some(password_file) = password_file {
        config.password_file = Some(password_file.clone());
    }
    if let Some(user) = user {
        config.user = Some(user.clone());
    }
    if no_verify_hostname {
        config.verify_hostname = false;
    }
    config.resolve_paths();
    config.validate()?;

    match command {
        IrcCommand::Connect { once, seconds } => run::connect(&config, *once, *seconds),
        IrcCommand::Send { target, text } => run::send(&config, target, text),
        IrcCommand::React {
            target,
            msgid,
            emoji,
        } => run::react(&config, target, msgid, emoji),
        IrcCommand::Reply {
            target,
            msgid,
            text,
        } => run::reply(&config, target, msgid, text),
        IrcCommand::Ingest {
            transcript,
            target,
            limit,
        } => run::ingest(&config, transcript, target, *limit),
        IrcCommand::Whoami => {
            println!("{}", config.whoami());
            Ok(())
        }
        IrcCommand::Selftest => run::selftest(),
    }
}

/// Hidden ohmOS commands. `probe` is read-only; `serve` refuses to start without
/// a configured base URL *and* explicit send opt-in.
async fn ohm_command(path: &Path, command: &OhmCommand) -> Result<()> {
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
