use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jproxy_rs::app::{self, AppState};
use jproxy_rs::config::Config;
use jproxy_rs::import;
use jproxy_rs::rename;
use jproxy_rs::sync;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("import") {
        return run_import(&args[2..]).await;
    }
    if let Some(subcommand) = args.get(1).map(String::as_str)
        && matches!(
            subcommand,
            "admin" | "config" | "sync" | "doctor" | "version"
        )
    {
        return jproxy_rs::cli::run(&args[1..]).await;
    }

    let config = Config::from_env()?;
    info!("starting jproxy-rs {}", env!("CARGO_PKG_VERSION"));

    let state = AppState::new(config).await?;
    state.warn_if_default_admin_credentials().await;
    let snapshot = state.snapshot();
    info!("{}", snapshot.startup_summary());
    sync::spawn(state.clone());
    rename::spawn(state.clone());

    let listen_addr = snapshot.listen_addr;
    let listener = TcpListener::bind(listen_addr).await?;
    info!(%listen_addr, "listening");
    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    info!("shutdown complete");
    Ok(())
}

/// `jproxy-rs import --from <source.db> --to <target.db>`
///
/// Copies the non-secret reference tables (titles, rules, TMDB titles) from an
/// existing jproxy database into a target database. Never reads or writes
/// `system_config`/`system_user`.
async fn run_import(args: &[String]) -> Result<()> {
    let mut source: Option<PathBuf> = None;
    let mut target: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--from" => {
                source = Some(PathBuf::from(
                    args.get(index + 1).context("--from requires a path")?,
                ));
                index += 2;
            }
            "--to" => {
                target = Some(PathBuf::from(
                    args.get(index + 1).context("--to requires a path")?,
                ));
                index += 2;
            }
            other => bail!("unexpected argument: {other}"),
        }
    }
    let source = source.context("import requires --from <source.db>")?;
    let target = target.context("import requires --to <target.db>")?;

    let report = import::import_reference_data(&source, &target).await?;
    for (table, count) in &report.copied {
        info!(table, count, "imported reference table");
    }
    println!("imported {} rows into {}", report.total(), target.display());
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("jproxy_rs=info,tower_http=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
