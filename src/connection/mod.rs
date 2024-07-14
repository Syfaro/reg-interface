use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, trace};

mod http;
mod mqtt;
mod websocket;

#[derive(Debug, Deserialize)]
pub struct ConnectionConfig {
    targets: Vec<ConnectionTargetConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ConnectionTargetConfig {
    name: String,
    #[serde(default = "crate::scanner::DecoderType::all")]
    supported_decoders: HashSet<crate::scanner::DecoderType>,
    #[serde(flatten)]
    connection_type: ConnectionType,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "connection_type", rename_all = "lowercase")]
pub enum ConnectionType {
    Http(HttpConnectionConfig),
    WebSocket(WebSocketConnectionConfig),
    Mqtt(MqttConnectionConfig),
}

#[derive(Debug, Deserialize)]
pub struct HttpConnectionConfig {
    url: String,
    timeout: Option<u64>,
    user_agent: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct WebSocketConnectionConfig {
    url: String,
    #[serde(default)]
    allow_actions: bool,
}

#[derive(Debug, Deserialize)]
pub struct MqttConnectionConfig {
    url: String,
    publish_topic: String,
    action_topic: Option<String>,
    #[serde(default)]
    allow_actions: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConnectionAction {
    Print { url: String },
}

#[async_trait]
pub trait Connection: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn supported_decoder_types(&self) -> &HashSet<crate::scanner::DecoderType>;

    async fn send(&self, data: &crate::scanner::ScanResult) -> eyre::Result<()>;
}

pub struct ConnectionManager {
    connections: Vec<Box<dyn Connection>>,
}

impl ConnectionManager {
    #[instrument(skip(self, result))]
    pub async fn send(&self, result: crate::scanner::ScanResult) -> eyre::Result<()> {
        let decoder_type = result.data.decoder_type();

        for connection in self.connections.iter() {
            if !connection.supported_decoder_types().contains(&decoder_type) {
                trace!(
                    name = connection.name(),
                    "connection target did not support decoder type"
                );
                continue;
            }

            if matches!(result.connection_targets.as_ref(), Some(targets) if !targets.contains(connection.name()))
            {
                trace!(
                    name = connection.name(),
                    "result did not want connection target"
                );
                continue;
            }

            debug!(
                name = connection.name(),
                "sending data to connection target"
            );
            connection.send(&result).await?;
        }

        Ok(())
    }
}

pub async fn start_connections(
    config: ConnectionConfig,
    token: CancellationToken,
    tx: Sender<ConnectionAction>,
) -> eyre::Result<ConnectionManager> {
    let mut connections: Vec<Box<dyn Connection>> = Vec::with_capacity(config.targets.len());

    for target in config.targets {
        let connection = match target.connection_type {
            ConnectionType::Http(config) => Box::new(
                http::HttpConnection::new(target.name, target.supported_decoders, config).await?,
            ) as Box<dyn Connection>,
            ConnectionType::WebSocket(config) => Box::new(
                websocket::WebSocketConnection::new(
                    target.name,
                    target.supported_decoders,
                    config,
                    token.clone(),
                    tx.clone(),
                )
                .await?,
            ),
            ConnectionType::Mqtt(config) => Box::new(
                mqtt::MqttConnection::new(
                    target.name,
                    target.supported_decoders,
                    config,
                    token.clone(),
                    tx.clone(),
                )
                .await?,
            ),
        };

        connections.push(connection);
    }

    Ok(ConnectionManager { connections })
}
