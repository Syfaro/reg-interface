use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use eyre::{bail, OptionExt};
use tokio::{
    select,
    sync::mpsc::{channel, Sender},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};

pub struct MqttConnection {
    name: String,
    tx: Sender<(serde_json::Value, HashMap<String, String>)>,
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
        let (tx, mut rx) = channel::<(serde_json::Value, HashMap<String, String>)>(1);

        let options = match config.params {
            super::MqttConnectionParams::Url { url } => rumqttc::MqttOptions::parse_url(url)?,
            super::MqttConnectionParams::Options {
                client_id,
                host,
                port,
                credentials,
            } => {
                let mut options = if let Ok(url) = url::Url::parse(&host) {
                    let (transport, host) = match url.scheme() {
                        "mqtt" => (
                            rumqttc::Transport::tcp(),
                            url.host_str().ok_or_eyre("mqtt url scheme missing host")?,
                        ),
                        "mqtts" => (
                            rumqttc::Transport::tls_with_default_config(),
                            url.host_str().ok_or_eyre("mqtts url scheme missing host")?,
                        ),
                        "ws" => (rumqttc::Transport::ws(), url.as_str()),
                        "wss" => (rumqttc::Transport::wss_with_default_config(), url.as_str()),
                        scheme => bail!("unknown scheme: {scheme}"),
                    };

                    let mut options =
                        rumqttc::MqttOptions::new(client_id, host, url.port().unwrap_or(1883));

                    options.set_transport(transport);

                    options
                } else {
                    rumqttc::MqttOptions::new(client_id, host, port.unwrap_or(1883))
                };

                if let Some(super::MqttCredentials { username, password }) = credentials {
                    options.set_credentials(username, password);
                }

                options
            }
        };

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
                        let Some((data, extras)) = data else {
                            info!("mqtt data channel ended, ending task");
                            break;
                        };

                        let topic = extras.get("mqtt_topic").unwrap_or(&config.publish_topic);

                        trace!(topic, "sending data to mqtt");

                        let json = serde_json::to_string(&data).expect("could not serialize data");
                        if let Err(err) = client.publish(topic, rumqttc::QoS::AtMostOnce, false, json).await {
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

    async fn send(&self, result: &crate::scanner::ScanResult) -> eyre::Result<()> {
        self.tx
            .send((result.data_to_send()?.into_owned(), result.extras.clone()))
            .await
            .map_err(Into::into)
    }
}
