use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::{
    select,
    sync::mpsc::{channel, Receiver, Sender},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn, Instrument};

mod decoder;
mod input;

pub use decoder::DecoderType;

#[derive(Debug, Deserialize, Serialize)]
pub struct ScannerConfig {
    inputs: Vec<input::ScannerInputConfig>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "data_type")]
pub enum ScannedData {
    Aamva(Box<aamva::DecodedData>),
    Url { url: String },
    Generic { data: String },
}

impl ScannedData {
    pub fn decoder_type(&self) -> decoder::DecoderType {
        match self {
            Self::Aamva(_) => decoder::DecoderType::Aamva,
            Self::Url { .. } => decoder::DecoderType::Url,
            Self::Generic { .. } => decoder::DecoderType::Generic,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScanResult {
    pub input_id: usize,
    pub data: ScannedData,
    pub transformed_data: Option<serde_json::Value>,
    pub connection_targets: Option<Arc<HashSet<String>>>,
    pub extras: HashMap<String, String>,
}

impl ScanResult {
    pub fn data_to_send(&self) -> eyre::Result<Cow<'_, serde_json::Value>> {
        if let Some(transformed_data) = self.transformed_data.as_ref() {
            Ok(Cow::Borrowed(transformed_data))
        } else {
            serde_json::to_value(self.data.clone())
                .map(Cow::Owned)
                .map_err(Into::into)
        }
    }
}

pub async fn setup_scanners(
    config: ScannerConfig,
    token: CancellationToken,
    scanned_data_tx: Sender<ScanResult>,
) -> eyre::Result<()> {
    let (tx, rx) = channel(1);

    let decoder = decoder::Decoder::new(&config)?;

    input::connect_scanner_inputs(config.inputs, token.clone(), tx).await?;
    tokio::spawn(decode_inputs(decoder, token, rx, scanned_data_tx).in_current_span());

    Ok(())
}

async fn decode_inputs(
    decoder: decoder::Decoder,
    token: CancellationToken,
    mut rx: Receiver<(usize, String)>,
    tx: Sender<ScanResult>,
) -> eyre::Result<()> {
    loop {
        select! {
            _ = token.cancelled() => {
                info!("scanner task cancelled, ending decoding task");
                break;
            }
            val = rx.recv() => {
                let Some((input_id, s)) = val else {
                    info!("scanner input channel closed, ending decoding task");
                    break;
                };

                trace!("got input: {s}");

                let data = match decoder.decode(input_id, s).await {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        warn!("no decoders could successfully decode input");
                        continue;
                    }
                    Err(err) => {
                        error!("error attempting to decode data: {err}");
                        continue;
                    }
                };

                debug!("got decoded data: {data:?}");

                if let Err(err) = tx.send(data).await {
                    error!("could not send decoded data, ending decoding task: {err}");
                    break;
                }
            }
        }
    }

    Ok(())
}
