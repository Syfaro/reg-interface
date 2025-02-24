use std::collections::HashMap;

use async_trait::async_trait;
use eyre::OptionExt;
use rand::Rng;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, MqttOptions, QoS, Transport};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, trace, warn};

use crate::{
    RunningTasks,
    action::ConnectionAction,
    connection::{Connection, ConnectionTypeMqttConfig},
};

pub struct MqttConnection {
    topic: String,
    value_tx: mpsc::Sender<(String, Vec<u8>)>,
}

impl MqttConnection {
    pub async fn new(
        name: String,
        config: ConnectionTypeMqttConfig,
        token: CancellationToken,
        action_tx: mpsc::Sender<ConnectionAction>,
        tasks: &mut RunningTasks,
    ) -> eyre::Result<Self> {
        let (value_tx, value_rx) = mpsc::channel(1);

        let (transport, host) = match config.url.scheme() {
            "mqtt" => (
                Transport::tcp(),
                config.url.host_str().ok_or_eyre("mqtt url missing host")?,
            ),
            "mqtts" => (
                Transport::tls_with_default_config(),
                config.url.host_str().ok_or_eyre("mqtts url missing host")?,
            ),
            "ws" => (Transport::ws(), config.url.as_str()),
            "wss" => (Transport::wss_with_default_config(), config.url.as_str()),
            scheme => eyre::bail!("unknown mqtt scheme: {scheme}"),
        };

        let client_id = config.client_id.clone().unwrap_or_else(|| {
            rand::rng()
                .sample_iter(rand::distr::Alphanumeric)
                .take(12)
                .map(char::from)
                .collect()
        });

        let mut opts = MqttOptions::new(
            client_id,
            host,
            config.url.port_or_known_default().unwrap_or(1883),
        );
        opts.set_transport(transport);

        if let Some(password) = config.url.password() {
            opts.set_credentials(config.url.username(), password);
        }

        let (client, events) = AsyncClient::new(opts, 1);

        let task = tokio::spawn(Self::connection_loop(
            config.clone(),
            token,
            client.clone(),
            value_rx,
            action_tx,
            events,
        ));
        tasks.push((name, task));

        if config.allow_actions {
            for topic in config.action_topic {
                trace!(topic, "subscribing to topic");
                client.subscribe(topic, QoS::AtLeastOnce).await?;
            }
        }

        Ok(Self {
            topic: config.publish_topic,
            value_tx,
        })
    }

    async fn connection_loop(
        config: ConnectionTypeMqttConfig,
        token: CancellationToken,
        client: AsyncClient,
        mut value_rx: mpsc::Receiver<(String, Vec<u8>)>,
        action_tx: mpsc::Sender<ConnectionAction>,
        mut events: EventLoop,
    ) -> eyre::Result<()> {
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    break;
                }

                data = value_rx.recv() => {
                    let Some((topic, payload)) = data else {
                        break;
                    };
                    trace!(topic, "sending data to mqtt");
                    client.publish(topic, QoS::AtLeastOnce, false, payload).await?;
                }

                notif = events.poll() => {
                    match notif {
                        Ok(Event::Incoming(Incoming::Publish(publish))) => {
                            if !config.allow_actions {
                                warn!("got action, but actions are not allowed");
                                continue;
                            }

                            let action: ConnectionAction = if let Some(prefix) = &config.action_topic_prefix {
                                if let Some(stripped_topic) = publish.topic.strip_prefix(prefix) {
                                    if let Some(action) = ConnectionAction::from_name(stripped_topic, &publish.payload).transpose()? {
                                        action
                                    } else {
                                        warn!(topic = publish.topic, "could not determine action from topic");
                                        continue;
                                    }
                                } else {
                                    warn!(topic = publish.topic, "topic did not have expected prefix");
                                    continue;
                                }
                            } else {
                                serde_json::from_slice(&publish.payload)?
                            };

                            debug!("got action: {action:?}");
                            action_tx.send(action).await?;
                        }
                        Ok(event) => {
                            trace!("got other mqtt event: {event:?}");
                        }
                        Err(err) => {
                            error!("mqtt event loop failed: {err}");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Connection for MqttConnection {
    async fn send(
        &self,
        data: &serde_json::Value,
        changes: &HashMap<String, serde_json::Value>,
    ) -> eyre::Result<()> {
        let topic = changes
            .get("mqtt_topic")
            .and_then(|topic| topic.as_str())
            .unwrap_or(&self.topic)
            .to_string();
        let payload = serde_json::to_vec(&data)?;

        self.value_tx.send((topic, payload)).await?;

        Ok(())
    }
}
