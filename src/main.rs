use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jproxy_rs::app::{self, AppState};
use jproxy_rs::config::Config;
use jproxy_rs::control::{self, ControlPeer};
use jproxy_rs::import;
use jproxy_rs::rename;
use jproxy_rs::sync;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("import") {
        return run_import(&args[2..]).await;
    }
    if let Some(subcommand) = cli_subcommand(&args)
        && matches!(
            subcommand,
            "admin"
                | "config"
                | "sync"
                | "doctor"
                | "version"
                | "status"
                | "healthcheck"
                | "rules"
                | "titles"
                | "examples"
                | "tmdb"
                | "cache"
                | "rename"
                | "login"
                | "jobs"
        )
    {
        match jproxy_rs::cli::run(&args[1..]).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(jproxy_rs::cli::exit_code(&error));
            }
        }
    }

    let config = Config::from_env()?;
    info!("starting jproxy-rs {}", env!("CARGO_PKG_VERSION"));

    let state = AppState::new(config.clone()).await?;
    state.warn_if_default_admin_credentials().await;
    let snapshot = state.snapshot();
    info!("{}", snapshot.startup_summary());
    sync::spawn(state.clone());
    rename::spawn(state.clone());

    let listen_addr = snapshot.listen_addr;
    let listener = TcpListener::bind(listen_addr).await?;
    info!(%listen_addr, "listening");

    let mut prepared = control::prepare(&config).await?;
    info!(
        socket = %prepared.info.socket_path.display(),
        "control channel listening"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let tcp_shutdown = shutdown_rx.clone();
    let tcp_state = state.clone();
    let tcp_server = tokio::spawn(async move {
        axum::serve(listener, app::router(tcp_state))
            .with_graceful_shutdown(wait_for_shutdown(tcp_shutdown))
            .await
    });

    let uds_shutdown = shutdown_rx.clone();
    let uds_state = state.clone();
    let uds_info = prepared.info.clone();
    let uds_listener = prepared.take_listener()?;
    let uds_server = tokio::spawn(async move {
        axum::serve(
            uds_listener,
            control::control_router(uds_state, uds_info)
                .into_make_service_with_connect_info::<ControlPeer>(),
        )
        .with_graceful_shutdown(wait_for_shutdown(uds_shutdown))
        .await
    });

    shutdown_signal().await;
    let _ = shutdown_tx.send(true);
    let tcp_result = tcp_server.await;
    let uds_result = uds_server.await;
    prepared.cleanup();
    report_server_result("http", tcp_result)?;
    report_server_result("control", uds_result)?;
    info!("shutdown complete");
    Ok(())
}

fn report_server_result(
    name: &str,
    result: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error).with_context(|| format!("{name} server failed")),
        Err(error) => Err(error).with_context(|| format!("{name} server task failed")),
    }
}

async fn wait_for_shutdown(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// First positional argument after any leading global CLI options.
fn cli_subcommand(args: &[String]) -> Option<&str> {
    let mut skip_value = false;
    for argument in args.iter().skip(1) {
        if skip_value {
            skip_value = false;
            continue;
        }
        match argument.as_str() {
            "--socket" | "--token-file" | "--runtime-file" | "--timeout" => {
                skip_value = true;
            }
            other if other.starts_with("--") => {}
            other => return Some(other),
        }
    }
    None
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
