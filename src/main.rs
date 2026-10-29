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
    /// Multi-agent AgentSmith roster bridge (Slice A). The roster command lives
    /// in the bridge config; a roster failure is fatal (no orch fallback).
    Bridge {
        #[arg(long = "bridge-config", default_value = "config/agentsmith.toml")]
        bridge_config: PathBuf,
        #[command(subcommand)]
        command: BridgeCommand,
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
    /// Read-only probe of one (or all) `[[agent]]` providers. Prints HTTP status
    /// and catalog ids only: never the token, never message text.
    AgentProbe {
        #[arg(long)]
        agent: Option<String>,
        /// Explicit dry run (same read-only behaviour as `agent-probe`).
        #[arg(long)]
        dry_run: bool,
    },
    /// Post one agent reply through the ohmOS agent-conversation API.
    AgentReply {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        text: String,
        /// Explicit stable idempotency key; defaults to a deterministic id so a
        /// retry of the same text is a replay, never a duplicate.
        #[arg(long)]
        request_id: Option<String>,
        /// Print endpoint status only; send nothing and print no text.
        #[arg(long)]
        dry_run: bool,
    },
    /// Poll one agent conversation and deliver new inbound rows via `pi-notify`.
    AgentServe {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        interval_seconds: Option<u64>,
        /// Poll once and exit (the degraded status is still printed explicitly).
        #[arg(long)]
        once: bool,
    },
}

#[derive(Subcommand)]
enum BridgeCommand {
    /// Print the visible roster (orch + PMs + muse, workers only when visible).
    Roster {
        /// Emit one JSON object instead of tabs.
        #[arg(long)]
        json: bool,
    },
    /// Supervise every visible agent: load the roster each interval, reconcile,
    /// and deliver new user messages via `pi-notify`.
    #[command(alias = "run")]
    Serve {
        /// Run exactly one cycle and exit. Resident is the default; a long-running
        /// command must never look alive while it has already exited.
        #[arg(long)]
        once: bool,
        /// Seconds between cycles in resident mode (default 10).
        #[arg(long, value_name = "SECONDS", default_value_t = 10)]
        interval_s: u64,
    },
    /// Seed every owned agent's watermark to its conversation head without
    /// delivering. Run once before the first `serve` on an existing fleet.
    Seed {
        /// Limit to one agent (key, `pm:<vertical>`, or pi-notify target).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Issue one bot-bound service token per visible agent (owner session).
    /// Prints only the bot id and the token file path. Nothing without --apply.
    Provision {
        #[arg(long)]
        apply: bool,
        /// Limit to one agent (key, `pm:<vertical>`, or pi-notify target).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Append one assistant reply as exactly one agent, with that agent's token.
    Reply {
        /// Agent key (`fleet-pm-fpl-v1`) or delivery target (`pm-fpl`).
        #[arg(long)]
        agent: String,
        /// The answer. Mutually exclusive with --from-pane.
        #[arg(long, conflicts_with = "from_pane")]
        text: Option<String>,
        /// Read the worker's last assistant answer from its live tmux pane.
        #[arg(long, conflicts_with = "text")]
        from_pane: Option<String>,
        /// Thread the answer under this inbound message id.
        #[arg(long)]
        reply_to: Option<String>,
        /// Idempotency key; generated when omitted (deterministic for
        /// --from-pane, so a retry is a replay and never a second reply).
        #[arg(long)]
        nonce: Option<String>,
        /// Resolve and print the binding without sending anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Compare the roster with live agents and print per-agent state (no secrets).
    Status {
        /// Emit one JSON array instead of tabs.
        #[arg(long)]
        json: bool,
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
    /// Add one tapback (`threads.react`) to a message in an agent's conversation.
    React {
        /// Agent id, spawnKey, or roster name.
        bot: String,
        /// Rakazo message id to react to.
        message: String,
        /// One of the server's fixed reactions: 👍 👎 ❤️ 😂 🎉 😮.
        emoji: String,
        /// Remove the reaction. Rakazo exposes no removal RPC, so this fails
        /// explicitly rather than pretending locally.
        #[arg(long)]
        remove: bool,
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
    if let Command::Bridge {
        bridge_config,
        command,
    } = &cli.command
    {
        return bridge_command(bridge_config, command).await;
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
            let receipt = orch
                .say(&root.id, &Orch::spawn_prompt(entry), &nonce)
                .await?;
            println!(
                "spawning\tseq={}\tnonce={}\t{}",
                receipt.seq, nonce, entry.name
            );
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
            let receipt = orch.say(&id, text, &nonce).await?;
            match receipt.run() {
                Some(run) => println!("sent\tseq={}\trun={}\tnonce={}", receipt.seq, run, nonce),
                None => println!(
                    "stored seq={} (no run: relay-only) nonce={}",
                    receipt.seq, nonce
                ),
            }
        }
        OrchCommand::React {
            bot,
            message,
            emoji,
            remove,
        } => {
            anyhow::ensure!(
                !*remove,
                "Rakazo has no reaction-removal RPC: `threads/unreact` returns 404 and \
                 `threads.react` rejects an empty reaction. Remove the tapback in the Rakazo UI."
            );
            anyhow::ensure!(
                rakazo_beeper::rich::ALLOWED_REACTIONS.contains(&emoji.as_str()),
                "unsupported reaction {emoji}; Rakazo accepts {:?}",
                rakazo_beeper::rich::ALLOWED_REACTIONS
            );
            anyhow::ensure!(!message.trim().is_empty(), "message id must not be empty");
            let id = resolve_id(&orch, bot).await?;
            let nonce = random_nonce();
            orch.rakazo.react(&id, message, emoji, &nonce).await?;
            println!("sent\tok\tbot={id}\tmessage={message}\temoji={emoji}\tnonce={nonce}");
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
                PiNotify::default(),
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
                        "poll\tdelivered={}\tduplicates={}\tignored={}\treactions={}\treactions_dropped={}",
                        report.delivered,
                        report.duplicates,
                        report.ignored,
                        report.reactions,
                        report.reactions_dropped
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

/// Backoff after a failed cycle in resident mode: three intervals, capped at
/// five minutes, and never shorter than the configured interval.
fn cycle_backoff(interval: Duration) -> Duration {
    interval
        .saturating_mul(3)
        .min(Duration::from_secs(300))
        .max(interval)
}

/// Multi-agent AgentSmith roster bridge. A roster command failure is fatal for
/// one-shot subcommands: there is no "just orch" fallback. In resident mode
/// (`serve --interval-s N`) a failed roster command is a cycle error, not a
/// process exit, because transient `EAGAIN` from a loaded fork is recoverable.
async fn bridge_command(config_path: &Path, command: &BridgeCommand) -> Result<()> {
    use rakazo_beeper::orch::{
        Orch,
        agentsmith::{self, Agent, CommandRoster, RosterSource},
        bridge::{BridgeConfig, SessionIssuer, TokenStore},
        pane, reply, supervisor,
        supervisor::{PiNotifyDeliverer, Supervisor},
        watch::WatchState,
    };

    let config = BridgeConfig::load(config_path).with_context(|| {
        format!(
            "bridge config {} (copy config/agentsmith.example.toml and set roster_command + host)",
            config_path.display()
        )
    })?;
    let host = config.host()?;

    // Load the roster exactly the same way for every subcommand.
    let load = |config: &BridgeConfig, host: &str| -> Result<Vec<Agent>> {
        let source = CommandRoster::new(&config.agentsmith.roster_command)?;
        let roster = source.fetch()?;
        agentsmith::agents(&roster.rows, config.effective_hide_workers(&roster), host)
    };
    let token_present = |key: &str| TokenStore::new(&config.bridge.service_token_dir).exists(key);

    match command {
        BridgeCommand::Roster { json } => {
            let agents = load(&config, &host)?;
            if *json {
                let rows: Vec<_> = agents
                    .iter()
                    .map(|agent| {
                        serde_json::json!({
                            "key": agent.key,
                            "name": agent.name,
                            "role": agent.role.as_str(),
                            "host": agent.host,
                            "owned": agent.host == host,
                            "pane": agent.pane,
                            "worker": agent.delivery,
                            "offline": agent.offline,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "host": host,
                        "hide_workers": config.bridge.hide_workers,
                        "agents": rows,
                    })
                );
            } else {
                for agent in &agents {
                    println!(
                        "{}\t{}\thost={}\towned={}\tworker={}\tpane={}\toffline={}",
                        agent.key,
                        agent.role.as_str(),
                        agent.host,
                        agent.host == host,
                        agent.delivery,
                        agent.pane.as_deref().unwrap_or("-"),
                        agent.offline
                    );
                }
            }
        }
        BridgeCommand::Serve { once, interval_s } => {
            // Resident is the default: `serve` is a daemon, and a daemon that
            // exits 0 after one iteration is indistinguishable from success to
            // a restart wrapper. `--once` is the only way to ask for one cycle.
            // Validate before touching credentials: a bad interval is an
            // operator error, not an auth failure.
            anyhow::ensure!(*interval_s > 0, "--interval-s must be positive");
            let interval = Duration::from_secs(*interval_s);
            if interval < Duration::from_secs(2) {
                tracing::warn!(
                    ?interval,
                    "--interval-s below 2s; the roster command is expensive"
                );
                eprintln!("warning: --interval-s below 2s; the roster command is expensive");
            }
            let resident = !*once;
            let orch = Orch::from_env()?;
            let deliverer = PiNotifyDeliverer::configured(
                &config.bridge.notify_kind,
                config.bridge.notify_urgent,
            );
            let mut supervisor = Supervisor::new(
                orch.rakazo.clone(),
                deliverer,
                &config.bridge.state_dir,
                &host,
            )?
            .with_seen_reaction(config.bridge.seen_reaction.clone())
            .with_reply_hint_config(config_path);
            loop {
                // Re-read the roster every interval so added/removed agents are
                // reconciled. In resident mode a roster or cycle failure is a
                // cycle error: log it, back off, and keep supervising, because
                // `os error 35` (EAGAIN from fork under load) is transient.
                let mut failed = false;
                match load(&config, &host) {
                    Ok(agents) => match supervisor.cycle(&agents).await {
                        Ok(report) => println!(
                            "cycle\thost={}\tagents={}\towned={}\tforeign={}\tcreated={}\tarchived={}\tmissing={}\tdelivered={}\tdeferred={}\tduplicates={}\tignored={}\treactions={}\treactions_dropped={}\terrors={}",
                            host,
                            report.agents,
                            report.owned,
                            report.foreign.join(","),
                            report.created.join(","),
                            report.archived.join(","),
                            report.missing.join(","),
                            report.delivered,
                            report.deferred,
                            report.duplicates,
                            report.ignored,
                            report.reactions,
                            report.reactions_dropped,
                            report.errors.join(" | ")
                        ),
                        Err(error) if resident => {
                            failed = true;
                            eprintln!("cycle-error: {error:#}");
                        }
                        Err(error) => return Err(error),
                    },
                    Err(error) if resident => {
                        failed = true;
                        eprintln!("cycle-error roster: {error:#}");
                    }
                    Err(error) => {
                        return Err(error.context("roster command failed"));
                    }
                }
                if !resident {
                    eprintln!("one-shot: --once ran 1 cycle, exiting");
                    break;
                }
                let delay = if failed {
                    cycle_backoff(interval)
                } else {
                    interval
                };
                tokio::time::sleep(delay).await;
            }
        }
        BridgeCommand::Seed { agent } => {
            let orch = Orch::from_env()?;
            let agents = load(&config, &host)?;
            let bots = orch.bots().await?;
            for target in agents
                .iter()
                .filter(|target| target.host == host)
                .filter(|target| agent.as_deref().is_none_or(|only| target.matches(only)))
            {
                if target.role == agentsmith::AgentRole::Muse {
                    continue;
                }
                let Some(bot) = agentsmith::resolve_bot(&bots, &agents, target) else {
                    println!("{}\tbot=-\tseeded=false\treason=missing", target.key);
                    continue;
                };
                let watermark = supervisor::seed_watermark(
                    &orch.rakazo,
                    &config.bridge.state_dir,
                    &host,
                    target,
                    bot,
                )
                .await?;
                println!(
                    "{}\thost={}\tbot={}\tthread={}\tseeded=true\twatermark={watermark}",
                    target.key,
                    target.host,
                    bot.id,
                    bot.thread_id.as_deref().unwrap_or(&bot.id)
                );
            }
        }
        BridgeCommand::Provision { apply, agent } => {
            let orch = Orch::from_env()?;
            let agents = load(&config, &host)?;
            let bots = orch.bots().await?;
            let store = TokenStore::new(&config.bridge.service_token_dir);
            let issuer = SessionIssuer {
                rakazo: orch.rakazo.clone(),
            };
            let report = rakazo_beeper::orch::bridge::provision(
                &agents,
                &bots,
                &store,
                &issuer,
                *apply,
                agent.as_deref(),
            )
            .await?;
            for decision in &report.provisioned {
                // Never print token material; only the binding and the path.
                println!(
                    "{}\tbot={}\tpath={}\t{}",
                    decision.agent_key,
                    decision.bot_id,
                    decision.token_path.display(),
                    if decision.issued {
                        "issued"
                    } else if *apply {
                        "present"
                    } else {
                        "would-issue"
                    }
                );
            }
            for (key, reason) in &report.skipped {
                println!("{key}\tskipped={reason}");
            }
        }
        BridgeCommand::Reply {
            agent,
            text,
            from_pane,
            reply_to,
            nonce,
            dry_run,
        } => {
            let orch = Orch::from_env()?;
            let agents = load(&config, &host)?;
            let bots = orch.bots().await?;
            let target = agents
                .iter()
                .find(|candidate| candidate.matches(agent))
                .with_context(|| format!("no roster agent matches {agent}"))?;
            anyhow::ensure!(
                target.host == host,
                "{} belongs to host {}; this supervisor is {host}",
                target.key,
                target.host
            );
            let bot = agentsmith::resolve_bot(&bots, &agents, target)
                .with_context(|| format!("{} has no live Rakazo bot", target.key))?;
            // Exactly one answer source. `--from-pane` resolves a worker name
            // to its rosters pane, or uses the value as a raw tmux target.
            let (text, source) = match (text.as_deref(), from_pane.as_deref()) {
                (Some(text), None) => (text.to_string(), "text"),
                (None, Some(worker)) => {
                    let pane = agents
                        .iter()
                        .find(|candidate| candidate.matches(worker))
                        .and_then(|candidate| candidate.pane.clone())
                        .unwrap_or_else(|| worker.to_string());
                    (
                        pane::from_pane(&pane).with_context(|| {
                            format!("read last answer from pane {pane} for {}", target.key)
                        })?,
                        "pane",
                    )
                }
                (Some(_), Some(_)) => unreachable!("clap forbids both"),
                (None, None) => {
                    anyhow::bail!("reply needs --text \"<answer>\" or --from-pane <worker>")
                }
            };
            let nonce = nonce.clone().unwrap_or_else(|| match source {
                "pane" => {
                    reply::nonce_for(&[&target.key, reply_to.as_deref().unwrap_or(""), &text])
                }
                _ => reply::nonce(),
            });
            if *dry_run {
                println!(
                    "agent={}\thost={}\tbot={}\tthread={}\tsource={}\treply_to={}\tdry_run=true",
                    target.key,
                    target.host,
                    bot.id,
                    bot.thread_id.as_deref().unwrap_or(&bot.id),
                    source,
                    reply_to.as_deref().unwrap_or("")
                );
                return Ok(());
            }
            let store = TokenStore::new(&config.bridge.service_token_dir);
            let appender = rakazo_beeper::orch::bridge::RakazoAppender {
                base: orch.rakazo.base().clone(),
                space: orch.space.clone(),
            };
            let receipt = rakazo_beeper::orch::bridge::reply(
                &store,
                &appender,
                &agents,
                &bots,
                &target.key,
                &bot.id,
                &text,
                &nonce,
                reply_to.as_deref(),
            )
            .await?;
            println!(
                "agent={}\thost={}\tbot={}\tmessage={}\tseq={}\tnonce={}\treply_to={}",
                target.key,
                target.host,
                bot.id,
                receipt.message_id,
                receipt.seq,
                nonce,
                reply_to.as_deref().unwrap_or("")
            );
        }
        BridgeCommand::Status { json } => {
            let orch = Orch::from_env()?;
            let agents = load(&config, &host)?;
            let bots = orch.bots().await?;
            // Optional `[[agent]]` provider config. It is how a roster agent
            // such as Muse advertises its real transport and live Beeper state.
            let agent_configs = config
                .bridge
                .agent_config
                .as_ref()
                .map(|path| rakazo_beeper::ohm::OhmConfig::load(path))
                .transpose()?;
            let mut tracked = std::collections::BTreeMap::new();
            for agent in agents.iter().filter(|agent| agent.host == host) {
                let state_file =
                    supervisor::state_file(&config.bridge.state_dir, &host, &agent.key)?;
                let state = WatchState::load(Some(&state_file))?;
                tracked.insert(
                    agent.key.clone(),
                    supervisor::Tracked {
                        agent: agent.clone(),
                        state,
                        state_file,
                        bot_id: agentsmith::resolve_bot(&bots, &agents, agent)
                            .map(|bot| bot.id.clone()),
                        missing: agent.role != agentsmith::AgentRole::Muse
                            && agentsmith::resolve_bot(&bots, &agents, agent).is_none(),
                    },
                );
            }
            if *json {
                let rows: Vec<_> = agents
                    .iter()
                    .map(|agent| {
                        let tracked = tracked.get(&agent.key);
                        let state = tracked.map(|tracked| &tracked.state);
                        let bot = agentsmith::resolve_bot(&bots, &agents, agent);
                        let configured = agent_configs
                            .as_ref()
                            .and_then(|config| config.agents.iter().find(|a| a.key == agent.key));
                        let transport = configured
                            .map(|a| match a.transport {
                                rakazo_beeper::ohm::AgentTransportKind::Beeper => "beeper",
                                rakazo_beeper::ohm::AgentTransportKind::Ohmos => "ohmos",
                                rakazo_beeper::ohm::AgentTransportKind::Msgd => "msgd",
                            })
                            .unwrap_or("rakazo");
                        // One live GET /v1/info for a Beeper agent: a truthful
                        // status, not "the token file exists".
                        let beeper = configured
                            .filter(|a| {
                                a.transport == rakazo_beeper::ohm::AgentTransportKind::Beeper
                            })
                            .map(rakazo_beeper::ohm::beeper::status);
                        serde_json::json!({
                            "key": agent.key,
                            "role": agent.role.as_str(),
                            "host": agent.host,
                            "owned": agent.host == host,
                            "worker": agent.delivery,
                            "pane": agent.pane,
                            "offline": agent.offline,
                            "transport": transport,
                            "bot_id": bot.map(|bot| bot.id.clone()),
                            "thread": state.and_then(|state| state.thread_id.clone())
                                .or_else(|| bot.map(|bot| bot.thread_id.clone().unwrap_or_else(|| bot.id.clone()))),
                            "watermark": state.map(|state| state.last_seq).unwrap_or(0),
                            "last_delivered_id": state.and_then(|state| state.last_delivered_id.clone()),
                            "last_poll_ok": beeper.as_ref().map(|b| b.last_poll_ok)
                                .or_else(|| state.and_then(|state| state.last_poll_ok)),
                            "last_poll_at": state.and_then(|state| state.last_poll_at.clone()),
                            "last_error": beeper.as_ref().and_then(|b| b.last_error.clone())
                                .or_else(|| state.and_then(|state| state.last_error.clone())),
                            "missing": tracked.is_some_and(|tracked| tracked.missing),
                            "relay_only": bot.and_then(|bot| bot.relay_only),
                            "token_present": token_present(&agent.key),
                            "token_file": beeper.as_ref().and_then(|b| b.token_path.as_ref())
                                .map(|p| p.display().to_string()),
                            "token_fingerprint": beeper.as_ref().and_then(|b| b.token_fingerprint.clone()),
                            "delivery_backend": "pi-notify",
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                for row in supervisor::status_rows(
                    &agents,
                    &bots,
                    &tracked,
                    &host,
                    "pi-notify",
                    &token_present,
                ) {
                    println!("{row}");
                }
                if let Some(config) = &agent_configs {
                    for agent in &config.agents {
                        if agent.transport == rakazo_beeper::ohm::AgentTransportKind::Beeper {
                            println!(
                                "{}",
                                rakazo_beeper::ohm::beeper::status(agent).line(&agent.key)
                            );
                        }
                    }
                }
            }
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
                agents: Vec::new(),
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
        OhmCommand::AgentProbe { agent, dry_run: _ } => {
            let config = OhmConfig::load(path)?;
            agent_probe(&config, agent.as_deref())?;
        }
        OhmCommand::AgentReply {
            agent,
            text,
            request_id,
            dry_run,
        } => {
            let config = OhmConfig::load(path)?;
            agent_reply(&config, path, agent, text, request_id.as_deref(), *dry_run)?;
        }
        OhmCommand::AgentServe {
            agent,
            interval_seconds,
            once,
        } => {
            let config = OhmConfig::load(path)?;
            agent_serve(&config, path, agent.as_deref(), *interval_seconds, *once).await?;
        }
    }
    Ok(())
}

/// Load the owner-only bearer for one agent, or explain the one human command
/// that obtains it. The value is never printed.
fn agent_token(agent: &rakazo_beeper::ohm::AgentConfig) -> Result<rakazo_beeper::ohm::Token> {
    use rakazo_beeper::ohm::{AgentTransportKind, agent_conversations::TOKEN_ACQUISITION};
    let path = agent.token_path()?;
    let acquisition = match agent.transport {
        AgentTransportKind::Beeper => rakazo_beeper::ohm::beeper::TOKEN_ACQUISITION,
        _ => TOKEN_ACQUISITION,
    };
    rakazo_beeper::ohm::Token::load(&path).map_err(|error| {
        anyhow::anyhow!(
            "{error}\nobtain the token once with:\n  {acquisition}\nUNVERIFIED: no live request was made"
        )
    })
}

fn agent_probe(config: &rakazo_beeper::ohm::OhmConfig, selected: Option<&str>) -> Result<()> {
    use rakazo_beeper::ohm::{
        AgentConversationsClient, AgentTransportKind, BeeperClient, UreqAgentHttp, UreqBeeperHttp,
        beeper,
    };
    for agent in config.agents(selected)? {
        let Some(base) = agent.base_url.as_deref() else {
            println!(
                "{}\toffline\tno base_url configured; zero network calls",
                agent.key
            );
            continue;
        };
        if agent.transport == AgentTransportKind::Beeper {
            let path = agent.token_path()?;
            let token = match beeper::load_token(&path) {
                Ok(token) => token,
                Err(error) => {
                    println!(
                        "{}\ttoken={} (missing)\t{}",
                        agent.key,
                        path.display(),
                        error.code()
                    );
                    println!(
                        "{}\t{migration}",
                        agent.key,
                        migration = beeper::SH_WHATSAPP_MIGRATION
                    );
                    anyhow::bail!(
                        "agent {} probe: {error}\nobtain the token once with:\n  {}",
                        agent.key,
                        beeper::TOKEN_ACQUISITION
                    );
                }
            };
            // Path + last 4 only: never the value.
            println!("{}\t{}\ttransport=beeper", agent.key, token.describe(&path));
            let client = BeeperClient::new(Some(base), token, UreqBeeperHttp::new())
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let report = client.probe(
                agent
                    .chat_id
                    .as_deref()
                    .filter(|chat_id| !chat_id.trim().is_empty()),
            );
            for line in report.lines() {
                println!("{}\t{line}", agent.key);
            }
            if let Some(error) = report.errors.first() {
                println!(
                    "{}\t{migration}",
                    agent.key,
                    migration = beeper::SH_WHATSAPP_MIGRATION
                );
                anyhow::bail!("agent {} probe: {error}", agent.key);
            }
            continue;
        }
        let token = agent_token(agent)?;
        let client = AgentConversationsClient::new(Some(base), token, UreqAgentHttp::new())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let report = client.probe(agent.conversation_id.as_deref());
        for line in report.lines() {
            println!("{}\t{line}", agent.key);
        }
        if let Some(error) = report.errors.first() {
            // Non-zero exit: a 401/502 probe is never a silent success.
            anyhow::bail!("agent {} probe: {error}", agent.key);
        }
    }
    Ok(())
}

fn agent_reply(
    config: &rakazo_beeper::ohm::OhmConfig,
    config_path: &Path,
    key: &str,
    text: &str,
    request_id: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    use rakazo_beeper::ohm::{
        AgentBridge, AgentConversationsClient, AgentTransportKind, BeeperClient, BeeperTransport,
        ExpectedNativeIdentity, PiNotifyDelivery, UreqAgentHttp, UreqBeeperHttp, beeper,
        stable_request_id,
    };
    let agent = config
        .agents(Some(key))?
        .into_iter()
        .next()
        .expect("selected above");
    let Some(base) = agent.base_url.as_deref() else {
        anyhow::bail!(
            "agent {} is offline (no base_url); refusing to send and making no network call",
            agent.key
        );
    };
    let request_id = request_id
        .map(str::to_string)
        .unwrap_or_else(|| stable_request_id(&agent.key, text));

    if agent.transport == AgentTransportKind::Beeper {
        let chat_id = beeper::chat_id_for_send(agent, config_path)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let path = agent.token_path()?;
        let token = agent_token(agent)?;
        println!("{}\t{}", agent.key, token.describe(&path));
        let client = BeeperClient::new(Some(base), token, UreqBeeperHttp::new())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if dry_run {
            // Status only: no text, no token, no send.
            let report = client.probe(Some(&chat_id));
            for line in report.lines() {
                println!("{}\t{line}", agent.key);
            }
            println!("{}\tdry_run\tno message sent", agent.key);
            if let Some(error) = report.errors.first() {
                anyhow::bail!("agent {} dry run: {error}", agent.key);
            }
            return Ok(());
        }
        let state_file = beeper::default_state_file(&agent.key, agent.state_file.as_deref());
        let transport = BeeperTransport::new(client, chat_id.clone(), agent.limit);
        let mut bridge = AgentBridge::new(
            transport,
            PiNotifyDelivery::new(),
            agent.worker().to_string(),
            chat_id,
            Some(state_file),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let report = bridge
            .reply(&request_id, text)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let message_id = bridge
            .state()
            .sent
            .get(&request_id)
            .cloned()
            .unwrap_or_else(|| "none".to_string());
        println!(
            "{}\trequest_id={request_id}\tmessage_id={message_id}\tstatus={report:?}",
            agent.key
        );
        return Ok(());
    }

    let conversation = agent.conversation_id.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "agent {} has no conversation_id; read it from GET /agent-conversations",
            agent.key
        )
    })?;
    let token = agent_token(agent)?;
    let client = AgentConversationsClient::new(Some(base), token, UreqAgentHttp::new())
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    if dry_run {
        // Status only: no text, no token, no send.
        let report = client.probe(Some(conversation));
        for line in report.lines() {
            println!("{}\t{line}", agent.key);
        }
        println!("{}\tdry_run\tno message sent", agent.key);
        if let Some(error) = report.errors.first() {
            anyhow::bail!("agent {} dry run: {error}", agent.key);
        }
        return Ok(());
    }

    let identity = match (agent.platform, agent.remote_id.as_deref()) {
        (Some(platform), Some(remote_id)) if !remote_id.trim().is_empty() => {
            Some(ExpectedNativeIdentity {
                platform,
                remote_id,
            })
        }
        _ => None,
    };
    let receipt = client
        .send(conversation, &request_id, text, identity)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    println!(
        "{}\trequest_id={}\tstatus={}",
        agent.key, receipt.id, receipt.status
    );
    Ok(())
}

async fn agent_serve(
    config: &rakazo_beeper::ohm::OhmConfig,
    config_path: &Path,
    selected: Option<&str>,
    interval_seconds: Option<u64>,
    once: bool,
) -> Result<()> {
    use rakazo_beeper::ohm::{
        AgentBridge, AgentConversationsClient, AgentTransportKind, BeeperClient, BeeperTransport,
        OhmosTransport, PiNotifyDelivery, UreqAgentHttp, UreqBeeperHttp, beeper,
        default_state_file, identity,
    };
    let agents = config.agents(selected)?;
    anyhow::ensure!(
        agents.len() == 1,
        "agent serve needs exactly one --agent ({} agent entries configured)",
        agents.len()
    );
    let agent = agents[0];
    let Some(base) = agent.base_url.as_deref() else {
        println!(
            "{}\toffline\tno base_url configured; zero network calls",
            agent.key
        );
        return Ok(());
    };
    let interval = interval_seconds.unwrap_or_else(|| agent.poll_seconds());

    if agent.transport == AgentTransportKind::Beeper {
        let chat_id = beeper::chat_id_for_send(agent, config_path)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let path = agent.token_path()?;
        let token = agent_token(agent)?;
        // Startup line: path + last 4 only, never the secret. `status()` also
        // makes the one live GET /v1/info that proves the local API is enabled.
        println!("{}\t{}\ttransport=beeper", agent.key, token.describe(&path));
        let live = beeper::status(agent);
        println!("{}\t{}", agent.key, live.line(&agent.key));
        let client = BeeperClient::new(Some(base), token, UreqBeeperHttp::new())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let transport = BeeperTransport::new(client, chat_id.clone(), agent.limit);
        let state_file = beeper::default_state_file(&agent.key, agent.state_file.as_deref());
        let mut bridge = AgentBridge::new(
            transport,
            PiNotifyDelivery::new(),
            agent.worker().to_string(),
            chat_id,
            Some(state_file.clone()),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        println!(
            "{}\tserve\tworker={}\tstate={}\tinterval={}s",
            agent.key,
            agent.worker(),
            state_file.display(),
            interval
        );
        return run_agent_bridge(agent.key.as_str(), &mut bridge, interval, once).await;
    }

    let conversation = agent
        .conversation_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("agent {} has no conversation_id", agent.key))?;
    let token = agent_token(agent)?;
    let client = AgentConversationsClient::new(Some(base), token, UreqAgentHttp::new())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let transport = OhmosTransport::new(
        client,
        conversation.clone(),
        identity(agent.platform, agent.remote_id.as_deref()),
        agent.limit,
    );
    let state_file = default_state_file(&agent.key, agent.state_file.as_deref());
    let mut bridge = AgentBridge::new(
        transport,
        PiNotifyDelivery::new(),
        agent.worker().to_string(),
        conversation,
        Some(state_file.clone()),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    println!(
        "{}\tserve\tworker={}\tstate={}\tinterval={}s",
        agent.key,
        agent.worker(),
        state_file.display(),
        interval
    );
    run_agent_bridge(agent.key.as_str(), &mut bridge, interval, once).await
}

async fn run_agent_bridge<T: rakazo_beeper::ohm::AgentTransport>(
    key: &str,
    bridge: &mut rakazo_beeper::ohm::AgentBridge<T, rakazo_beeper::ohm::PiNotifyDelivery>,
    interval: u64,
    once: bool,
) -> Result<()> {
    loop {
        let mut failed = false;
        match bridge.poll_once() {
            Ok(report) => {
                if report.delivered > 0 || report.duplicates > 0 {
                    println!(
                        "{}\tdelivered={}\tduplicates={}\tsuppressed={}\tdropped={}",
                        key, report.delivered, report.duplicates, report.suppressed, report.dropped
                    );
                }
            }
            Err(error) if error.is_degraded() => {
                // Explicit degraded status: never a silent success.
                println!("{key}\tdegraded\t{error}");
                failed = true;
            }
            Err(error) => {
                println!("{}\t{}\t{}", key, error.code(), error);
                failed = true;
            }
        }
        if once {
            if failed {
                anyhow::bail!("agent {key} poll failed");
            }
            break;
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
    Ok(())
}

#[cfg(test)]
mod backoff_tests {
    use super::cycle_backoff;
    use std::time::Duration;

    #[test]
    fn backoff_is_three_intervals_capped_at_five_minutes() {
        assert_eq!(
            cycle_backoff(Duration::from_secs(10)),
            Duration::from_secs(30)
        );
        assert_eq!(
            cycle_backoff(Duration::from_secs(200)),
            Duration::from_secs(300)
        );
        assert_eq!(
            cycle_backoff(Duration::from_secs(1)),
            Duration::from_secs(3)
        );
        // Never shorter than the configured interval, even past the cap.
        assert_eq!(
            cycle_backoff(Duration::from_secs(900)),
            Duration::from_secs(900)
        );
    }
}

#[cfg(test)]
mod serve_cli_tests {
    use super::{BridgeCommand, Cli, Command};
    use clap::Parser;

    fn serve(args: &[&str]) -> (bool, u64) {
        let cli = Cli::try_parse_from(args).expect("serve CLI must parse");
        match cli.command {
            Command::Bridge {
                command: BridgeCommand::Serve { once, interval_s },
                ..
            } => (once, interval_s),
            _ => panic!("expected bridge serve"),
        }
    }

    #[test]
    fn serve_defaults_to_resident_at_ten_seconds() {
        let (once, interval_s) = serve(&["rakazo-beeper", "bridge", "serve"]);
        assert!(!once, "serve must be resident by default");
        assert_eq!(interval_s, 10);
    }

    #[test]
    fn once_is_the_only_way_to_ask_for_a_single_cycle() {
        let (once, _) = serve(&["rakazo-beeper", "bridge", "serve", "--once"]);
        assert!(once);
    }

    #[test]
    fn interval_is_explicit_and_never_zero() {
        let (_, interval_s) = serve(&["rakazo-beeper", "bridge", "serve", "--interval-s", "5"]);
        assert_eq!(interval_s, 5);
        assert!(
            Cli::try_parse_from(["rakazo-beeper", "bridge", "serve", "--interval-s", "0"]).is_ok()
        );
    }
}

#[cfg(test)]
mod orch_react_cli_tests {
    use super::{Cli, Command, OrchCommand};
    use clap::Parser;

    fn react(args: &[&str]) -> (String, String, String, bool) {
        let cli = Cli::try_parse_from(args).expect("orch react CLI must parse");
        match cli.command {
            Command::Orch {
                command:
                    OrchCommand::React {
                        bot,
                        message,
                        emoji,
                        remove,
                    },
            } => (bot, message, emoji, remove),
            _ => panic!("expected orch react"),
        }
    }

    #[test]
    fn orch_react_parses_bot_message_emoji_and_remove() {
        let (bot, message, emoji, remove) = react(&[
            "rakazo-beeper",
            "orch",
            "react",
            "fleet-orch-v1",
            "message-9",
            "👍",
        ]);
        assert_eq!(bot, "fleet-orch-v1");
        assert_eq!(message, "message-9");
        assert_eq!(emoji, "👍");
        assert!(!remove);

        let (_, _, _, remove) = react(&[
            "rakazo-beeper",
            "orch",
            "react",
            "fleet-orch-v1",
            "message-9",
            "👍",
            "--remove",
        ]);
        assert!(remove);
    }
}
