use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::{RunningTasks, action::ConnectionAction};

mod http;
mod mqtt;

#[derive(Clone, Debug, Deserialize)]
pub struct ConnectionConfig {
    pub name: String,
    pub input_names: Option<HashSet<String>>,
    #[serde(flatten)]
    pub connection_type: ConnectionTypeConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "connection_type", rename_all = "lowercase")]
pub enum ConnectionTypeConfig {
    Http(http::ConnectionTypeHttpConfig),
    Mqtt(mqtt::ConnectionTypeMqttConfig),
}

#[async_trait]
pub trait Connection {
    async fn send(
        &self,
        data: &serde_json::Value,
        changes: &HashMap<String, serde_json::Value>,
    ) -> eyre::Result<()>;
}

pub struct ConnectionManager {
    connections: Vec<ConnectionMetadata>,
}

struct ConnectionMetadata {
    name: String,
    input_names: Option<HashSet<String>>,
    connection: Box<dyn Connection + Send + Sync>,
}

impl ConnectionManager {
    pub async fn new(
        token: CancellationToken,
        tasks: &mut RunningTasks,
        action_tx: mpsc::Sender<ConnectionAction>,
        config: Vec<ConnectionConfig>,
    ) -> eyre::Result<Self> {
        let mut connections = Vec::with_capacity(config.len());

        for target in config {
            let connection = Self::create_connection(
                token.clone(),
                action_tx.clone(),
                tasks,
                target.name.clone(),
                target.connection_type,
            )
            .await?;

            connections.push(ConnectionMetadata {
                name: target.name,
                input_names: target.input_names,
                connection,
            });
        }

        Ok(Self { connections })
    }

    pub async fn send(
        &self,
        input_name: &str,
        data: &serde_json::Value,
        changes: &HashMap<String, serde_json::Value>,
    ) -> eyre::Result<()> {
        let targets: Option<HashSet<&str>> = changes
            .get("targets")
            .and_then(|targets| targets.as_array())
            .map(|targets| {
                targets
                    .iter()
                    .filter_map(|target| target.as_str())
                    .collect()
            });

        for conn_meta in self.connections.iter() {
            if matches!(targets, Some(ref targets) if !targets.contains(conn_meta.name.as_str())) {
                trace!("specified targets did not include connection");
                continue;
            }

            if matches!(conn_meta.input_names, Some(ref input_names) if targets.is_none() && !input_names.contains(input_name))
            {
                trace!("default targets did not include connection");
                continue;
            }

            conn_meta.connection.send(data, changes).await?;
        }

        Ok(())
    }

    async fn create_connection(
        token: CancellationToken,
        action_tx: mpsc::Sender<ConnectionAction>,
        tasks: &mut RunningTasks,
        name: String,
        config: ConnectionTypeConfig,
    ) -> eyre::Result<Box<dyn Connection + Send + Sync>> {
        let connection = match config {
            ConnectionTypeConfig::Http(http) => {
                debug!(name, "creating http connection");
                Box::new(http::HttpConnection::new(name, http).await?)
                    as Box<dyn Connection + Send + Sync>
            }
            ConnectionTypeConfig::Mqtt(mqtt) => {
                debug!(name, "creating mqtt connection");
                Box::new(mqtt::MqttConnection::new(name, mqtt, token, action_tx, tasks).await?)
                    as Box<dyn Connection + Send + Sync>
            }
        };

        Ok(connection)
    }
}
