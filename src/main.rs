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
    if let Command::Login {
        rakazo_url,
        issuer,
        client_id,
        output,
    } = &cli.command
    {
        return rakazo_beeper::login::login(rakazo_url, issuer, client_id, output).await;
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
