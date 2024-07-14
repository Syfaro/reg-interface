use std::collections::HashSet;

use async_trait::async_trait;
use tokio::{
    select,
    sync::mpsc::{channel, Sender},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};

pub struct MqttConnection {
    name: String,
    tx: Sender<serde_json::Value>,
    supported_decoders: HashSet<crate::scanner::DecoderType>,
}

impl MqttConnection {
    pub async fn new(
        name: String,
        supported_decoders: HashSet<crate::scanner::DecoderType>,
        config: super::MqttConnectionConfig,
        token: CancellationToken,
        action_tx: Sender<super::ConnectionAction>,
    ) -> eyre::Result<Self> {
        let (tx, mut rx) = channel::<serde_json::Value>(1);
        let options = rumqttc::MqttOptions::parse_url(config.url)?;
        let (client, mut event_loop) = rumqttc::AsyncClient::new(options, 1);

        if let Some(action_topic) = config.action_topic.as_deref() {
            if config.allow_actions {
                client
                    .subscribe(action_topic, rumqttc::QoS::AtMostOnce)
                    .await?;
            }
        }

        tokio::spawn(async move {
            loop {
                select! {
                    _ = token.cancelled() => {
                        info!("mqtt cancelled, ending task");
                        break;
                    }

                    notification = event_loop.poll() => {
                        match notification {
                            Ok(event) => {
                                trace!(?event, "got event");

                                if let rumqttc::Event::Incoming(rumqttc::Incoming::Publish(publish)) = event {
                                    if Some(publish.topic.as_str()) == config.action_topic.as_deref() {
                                        let action: super::ConnectionAction = match serde_json::from_slice(&publish.payload) {
                                            Ok(action) => action,
                                            Err(err) => {
                                                warn!("could not decode action: {err}");
                                                continue;
                                            }
                                        };

                                        if !config.allow_actions {
                                            warn!("got action, but actions are not allowed");
                                            continue;
                                        }

                                        if let Err(err) = action_tx.send(action).await {
                                            error!("could not send action: {err}");
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                error!("mqtt event loop failed, ending task: {err}");
                                break;
                            }
                        }
                    }

                    data = rx.recv() => {
                        let Some(data) = data else {
                            info!("mqtt data channel ended, ending task");
                            break;
                        };

                        trace!("sending data to mqtt");

                        let json = serde_json::to_string(&data).expect("could not serialize data");
                        if let Err(err) = client.publish(&config.publish_topic, rumqttc::QoS::AtMostOnce, false, json).await {
                            error!("could not send mqtt message: {err}");
                        }
                    }
                }
            }
        });

        Ok(Self {
            name,
            supported_decoders,
            tx,
        })
    }
}

#[async_trait]
impl super::Connection for MqttConnection {
    fn name(&self) -> &str {
        &self.name
    }

    fn supported_decoder_types(&self) -> &HashSet<crate::scanner::DecoderType> {
        &self.supported_decoders
    }

    async fn send(&self, data: &serde_json::Value) -> eyre::Result<()> {
        self.tx.send(data.to_owned()).await.map_err(Into::into)
    }
}
