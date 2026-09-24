mod config;
mod discord;
mod github;
mod routing;
mod signature;
mod store;
mod worker;

use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};

use actix_web::{App, HttpServer, web::Data};
use anyhow::{Context as _, Result, ensure};
use clap::{Parser, Subcommand};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[derive(Parser)]
#[command(version, about = "Route signed GitHub release events to Discord")]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    /// Render a webhook payload without loading secrets or making requests.
    Preview {
        payload: PathBuf,
    },
    /// List permanent and exhausted delivery failures as JSON.
    Failed,
    /// Reset one failed delivery's retry budget.
    Retry {
        id: i64,
    },
}

#[actix_web::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_writer(std::io::stderr),
        )
        .init();
    let result = run_command().await;
    if let Err(error) = &result {
        tracing::error!(error = %format!("{error:#}"), "command failed");
    }
    result
}

async fn run_command() -> Result<()> {
    let args = Args::parse();
    if let Command::Preview { payload } = args.command {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(payload)?)?;
        let repository = value
            .pointer("/repository/full_name")
            .and_then(|s| s.as_str())
            .context("missing repository.full_name")?;
        let release =
            serde_json::from_value(value.get("release").cloned().context("missing release")?)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&discord::render(repository, &release))?
        );
        return Ok(());
    }
    let config = Arc::new(config::Config::load(&args.config)?);
    tracing::info!(
        bind = %config.bind,
        sources = config.sources.len(),
        destinations = config.destinations.len(),
        routes = config.routes.len(),
        "configuration loaded"
    );
    if let Some(parent) = std::path::Path::new(&config.database)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).context("cannot create database directory")?;
    }
    let store = store::Store::open(&config.database).await?;
    tracing::info!("database ready");
    match args.command {
        Command::Failed => println!("{}", serde_json::to_string_pretty(&store.failed().await?)?),
        Command::Retry { id } => {
            ensure!(store.retry(id).await?, "no failed delivery with ID {id}");
            tracing::info!(job_id = id, "failed delivery scheduled for retry");
        }
        Command::Serve => serve(config, store.clone()).await?,
        Command::Preview { .. } => unreachable!(),
    }
    store.0.close().await;
    Ok(())
}

async fn serve(config: Arc<config::Config>, store: store::Store) -> Result<()> {
    // Keep the lock file in place: removing it can let two processes lock different inodes.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(format!("{}.lock", config.database))
        .context("cannot open service lock file")?;
    lock.try_lock()
        .context("another server holds this database lock")?;
    tracing::info!("database lock acquired");

    let health = worker::WorkerHealth(Arc::new(AtomicBool::new(false)));
    let (stop, receiver) = tokio::sync::watch::channel(false);
    let app_config = config.clone();
    let app_store = store.clone();
    let app_health = health.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(Data::from(app_config.clone()))
            .app_data(Data::new(app_store.clone()))
            .app_data(Data::new(app_health.clone()))
            .configure(github::endpoints)
    })
    .workers(2)
    .disable_signals()
    .shutdown_timeout(35)
    .bind(&config.bind)?
    .run();
    tracing::info!(bind = %config.bind, "HTTP server starting");
    let handle = server.handle();
    let mut delivery = tokio::spawn(worker::run(store, config, receiver, health));
    let mut http = tokio::spawn(server);

    tokio::select! {
        signal = shutdown_signal() => {
            signal?;
            tracing::info!("shutdown requested");
            stop.send_replace(true);
            handle.stop(true).await;
            http.await??;
            delivery.await??;
        },
        result = &mut delivery => {
            stop.send_replace(true);
            handle.stop(true).await;
            http.await??;
            result??;
            tracing::error!("delivery worker stopped unexpectedly");
            anyhow::bail!("delivery worker stopped unexpectedly");
        },
        result = &mut http => {
            stop.send_replace(true);
            delivery.await??;
            result??;
            tracing::warn!("HTTP server stopped");
        },
    }
    tracing::info!("service stopped");
    Ok(())
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests;
