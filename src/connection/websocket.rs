use std::collections::HashSet;

use async_trait::async_trait;
use async_tungstenite::tokio::connect_async;
use futures::{SinkExt, StreamExt};
use tokio::{
    select,
    sync::mpsc::{channel, Sender},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, trace, warn};

pub struct WebSocketConnection {
    name: String,
    tx: Sender<serde_json::Value>,
    supported_decoders: HashSet<crate::scanner::DecoderType>,
}

impl WebSocketConnection {
    pub async fn new(
        name: String,
        supported_decoders: HashSet<crate::scanner::DecoderType>,
        config: super::WebSocketConnectionConfig,
        token: CancellationToken,
        action_tx: Sender<super::ConnectionAction>,
    ) -> eyre::Result<Self> {
        let (tx, mut rx) = channel::<serde_json::Value>(1);
        let (mut ws_stream, _) = connect_async(config.url).await?;

        tokio::spawn(async move {
            loop {
                select! {
                    _ = token.cancelled() => {
                        info!("websocket cancelled, ending task");
                        break;
                    }

                    data = rx.recv() => {
                        let Some(data) = data else {
                            info!("websocket data channel ended, ending task");
                            break;
                        };

                        trace!("sending data to websocket");

                        let json = serde_json::to_string(&data).expect("could not serialize data");
                        if let Err(err) = ws_stream.send(async_tungstenite::tungstenite::Message::Text(json)).await {
                            error!("could not send websocket message: {err}");
                        }
                    }

                    msg = ws_stream.next() => {
                        match msg {
                            Some(Ok(async_tungstenite::tungstenite::Message::Text(msg))) => {
                                trace!(msg, "got websocket data");

                                let action: super::ConnectionAction = match serde_json::from_str(&msg) {
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
                            },
                            Some(msg) => {
                                warn!("got unknown websocket message: {msg:?}");
                            }
                            _ => (),
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
impl super::Connection for WebSocketConnection {
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
