use std::sync::Arc;

use eyre::Context;
use serde::Deserialize;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use server::ImageStore;

mod action;
mod connection;
mod decoder;
mod input;
mod server;
mod transform;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub server: Option<server::ServerConfig>,
    #[serde(default)]
    pub decoder: decoder::DecoderConfig,
    pub transform: transform::TransformConfig,
    #[serde(default)]
    pub action: action::ActionConfig,
    pub inputs: Vec<input::InputConfig>,
    pub connections: Vec<connection::ConnectionConfig>,
}

type RunningTasks = Vec<(String, JoinHandle<eyre::Result<()>>)>;

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

    let token = CancellationToken::new();
    let mut tasks = Vec::new();

    let image_store = ImageStore::new(config.server.is_some());
    if let Some(server_config) = config.server.clone() {
        server::start(
            server_config,
            image_store.clone(),
            token.clone(),
            &mut tasks,
        )
        .await?;
    }

    let (qr_tx, qr_rx) = mpsc::channel(1);
    let (action_tx, action_rx) = mpsc::channel(1);

    let decoders = Arc::new(decoder::DecoderManager::new(config.decoder.clone(), qr_tx).await?);
    let transform_manager = transform::TransformManager::new(
        config.transform.clone(),
        config.decoder.mdl.extract_portraits,
        image_store,
    )
    .await?;

    action::start(config.action.clone(), token.clone(), action_rx, &mut tasks).await?;

    let connection_manager = connection::ConnectionManager::new(
        token.clone(),
        &mut tasks,
        action_tx,
        config.connections.clone(),
    )
    .await?;

    let input_stream =
        input::create_stream(config, token.clone(), &mut tasks, decoders.clone(), qr_rx).await?;
    tasks.push((
        "input-stream".to_string(),
        tokio::spawn(process_input_stream(
            decoders,
            transform_manager,
            connection_manager,
            input_stream,
        )),
    ));

    let ctrl_c = tokio::signal::ctrl_c();
    let token_clone = token.clone();
    tasks.push((
        "signal-exit".to_string(),
        tokio::spawn(async move {
            tokio::select! {
                _ = ctrl_c => {
                    info!("got exit signal, ending");
                    token_clone.cancel();
                }

                _ = token_clone.cancelled() => {}
            }

            Ok(())
        }),
    ));

    wait_for_tasks(token, tasks).await
}

async fn wait_for_tasks(token: CancellationToken, tasks: RunningTasks) -> eyre::Result<()> {
    let (task_names, task_futs): (Vec<_>, Vec<_>) = tasks.into_iter().unzip();
    let (task_res, task_idx, task_futs) = futures::future::select_all(task_futs).await;

    let mut exit_res = Ok(());

    match task_res {
        Ok(Ok(_)) => {
            if !token.is_cancelled() {
                warn!(name = task_names[task_idx], "task unexpectedly ended");
                token.cancel();
            }
        }
        Ok(Err(err)) => {
            error!(name = task_names[task_idx], "task returned error: {err}");
            exit_res = Err(err);
            token.cancel();
        }
        Err(err) => {
            error!(
                name = task_names[task_idx],
                "task join returned error: {err}"
            );
            exit_res = Err(err.into());
            token.cancel();
        }
    }

    for task_fut in task_futs {
        match task_fut.await {
            Ok(Ok(_)) => continue,
            Ok(Err(err)) => warn!("error cancelling task: {err}"),
            Err(err) => warn!("error joining task: {err}"),
        }
    }

    exit_res
}

async fn process_input_stream(
    decoders: Arc<decoder::DecoderManager>,
    transform_manager: transform::TransformManager,
    connection_manager: connection::ConnectionManager,
    mut rx: mpsc::Receiver<(Option<String>, decoder::DecodedDataContext)>,
) -> eyre::Result<()> {
    while let Some((original_data, mut data_context)) = rx.recv().await {
        debug!("got data: {data_context:?}");

        let (transformed_data, changes) = transform_manager.transform(&mut data_context)?;

        let transformed_data = if let Some(transformed_data) = transformed_data {
            decoders
                .post_transform(
                    data_context.decoder_type,
                    original_data.as_deref(),
                    transformed_data,
                    &changes,
                )
                .await?
        } else {
            serde_json::to_value(data_context.data)?
        };

        debug!("got transformed data: {transformed_data:?}, {changes:?}");

        connection_manager
            .send(&data_context.input_name, &transformed_data, &changes)
            .await?;
    }

    Ok(())
}
