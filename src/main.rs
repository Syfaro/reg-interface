use std::net::SocketAddr;

use axum::{routing, Router};
use eyre::Context;
use serde::Deserialize;
use tokio::{select, sync::mpsc::channel};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, trace, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use connection::ConnectionAction;

mod connection;
mod print;
mod scanner;

#[derive(Debug, Deserialize)]
struct Config {
    server_addr: SocketAddr,
    print: Option<print::PrintConfig>,
    decoder: Option<scanner::DecoderConfig>,
    scanner: Option<scanner::ScannerConfig>,
    connection: Option<connection::ConnectionConfig>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::Layer::default().pretty())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = std::path::PathBuf::from(
        std::env::var("REG_INTERFACE_CONFIG_PATH").unwrap_or_else(|_| "config.toml".to_string()),
    );

    debug!(path = %config_path.display(), "loading config");

    let config_contents = tokio::fs::read_to_string(&config_path)
        .await
        .wrap_err_with(|| {
            let path = std::path::absolute(&config_path)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| config_path.display().to_string());

            format!("failed to read config from {path}")
        })?;
    let config: Config = toml::from_str(&config_contents).wrap_err("failed to decode config")?;

    trace!("loaded config: {config:#?}");

    info!("starting");

    let token = CancellationToken::new();

    let mut app = Router::<()>::new().route("/health", routing::get(|| async move { "OK" }));

    let printer = if let Some(print) = config.print {
        info!("print enabled");
        Some(print::Printer::new(print).await?)
    } else {
        None
    };

    let (scanner_tx, mut scanner_rx) = channel(1);

    if let Some(scanner) = config.scanner {
        info!("scanner enabled");
        let router = scanner::setup_scanners(
            scanner,
            config.decoder.unwrap_or_default(),
            token.child_token(),
            scanner_tx,
        )
        .await?;
        app = app.merge(router);
    }

    let (connection_tx, mut connection_rx) = channel(1);

    let connection_manager = if let Some(connection) = config.connection {
        info!("connections enabled");
        Some(connection::start_connections(connection, token.child_token(), connection_tx).await?)
    } else {
        None
    };

    let listener = tokio::net::TcpListener::bind(&config.server_addr).await?;
    let token_clone = token.clone();
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app)
            .with_graceful_shutdown(token_clone.cancelled_owned())
            .await
        {
            error!("serve error: {err}")
        }
    });

    let token_clone = token.clone();
    tokio::spawn(async move {
        loop {
            select! {
                _ = token_clone.cancelled() => {
                    info!("main task cancelled, ending action task");
                    break;
                }

                action = connection_rx.recv() => {
                    let Some(action) = action else {
                        warn!("action channel closed, ending action task");
                        token_clone.cancel();
                        break;
                    };

                    debug!(?action, "got action");

                    if let Err(err) = process_action(printer.as_ref(), action).await {
                        error!("could not process action: {err}");
                    }
                }
            }
        }
    });

    loop {
        select! {
            _ = token.cancelled() => {
                info!("main task cancelled, ending main task");
                break;
            }

            data = scanner_rx.recv() => {
                let Some(data) = data else {
                    error!("scanner channel closed, ending main task");
                    break;
                };

                process_data(connection_manager.as_ref(), data).await?;
            }
        }
    }

    Ok(())
}

async fn process_action(
    printer: Option<&print::Printer>,
    action: ConnectionAction,
) -> eyre::Result<()> {
    match action {
        ConnectionAction::Print { url } => {
            let Some(printer) = printer else {
                warn!("got print command but no printer configured");
                return Ok(());
            };

            printer.print_url(&url).await?;
        }
    }

    Ok(())
}

#[instrument(err, skip_all, fields(input_id = result.input_id))]
async fn process_data(
    connection_manager: Option<&connection::ConnectionManager>,
    result: scanner::ScanResult,
) -> eyre::Result<()> {
    trace!("got result: {result:?}");

    if let Some(connection_manager) = connection_manager {
        connection_manager.send(result).await?;
    }

    Ok(())
}
