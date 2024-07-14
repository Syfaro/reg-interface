use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use rhai::{Engine, Scope, AST};
use serde::{Deserialize, Serialize};
use tap::{TapFallible, TapOptional};
use tracing::{debug, error, trace, warn};
use url::Url;

use crate::scanner::ScannedData;

use super::ScanResult;

mod shc;

pub use shc::ShcData;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DecoderConfig {
    shc: shc::ShcConfig,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderType {
    Aamva,
    Shc,
    Url,
    Generic,
}

impl std::fmt::Display for DecoderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Aamva => write!(f, "AAMVA"),
            Self::Shc => write!(f, "SHC"),
            Self::Url => write!(f, "URL"),
            Self::Generic => write!(f, "Generic"),
        }
    }
}

impl DecoderType {
    pub fn all() -> HashSet<Self> {
        [
            DecoderType::Aamva,
            DecoderType::Shc,
            DecoderType::Url,
            DecoderType::Generic,
        ]
        .into_iter()
        .collect()
    }
}

pub struct Decoder {
    input_decoders: Vec<HashSet<DecoderType>>,
    open_urls: Vec<bool>,
    transformers: Vec<Option<Mutex<(Engine, AST)>>>,
    connection_targets: Vec<Option<Arc<HashSet<String>>>>,

    shc_decoder: shc::ShcDecoder,
}

impl Decoder {
    pub async fn new(
        scanner_config: &super::ScannerConfig,
        decoder_config: DecoderConfig,
    ) -> eyre::Result<Self> {
        let input_decoders = scanner_config
            .inputs
            .iter()
            .map(|input| input.decoders.clone())
            .collect();

        let open_urls = scanner_config
            .inputs
            .iter()
            .map(|input| input.open_urls)
            .collect();

        let transformers = scanner_config
            .inputs
            .iter()
            .map(|input| {
                input
                    .transformer
                    .as_ref()
                    .map(|transformer| {
                        let mut engine = Engine::new();
                        engine.register_type_with_name::<aamva::DecodedData>("AamvaData");

                        let ast = engine.compile(transformer)?;

                        Ok(Mutex::new((engine, ast)))
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>, eyre::Report>>()?;

        let connection_targets = scanner_config
            .inputs
            .iter()
            .map(|input| input.connection_targets.clone())
            .collect();

        let shc_decoder = shc::ShcDecoder::new(decoder_config.shc).await?;

        Ok(Self {
            input_decoders,
            open_urls,
            transformers,
            connection_targets,
            shc_decoder,
        })
    }

    pub async fn decode(
        &self,
        input_id: usize,
        data: String,
    ) -> eyre::Result<Option<super::ScanResult>> {
        let Some(data) = self.parse_scanned_data(input_id, data).await else {
            return Ok(None);
        };

        let (transformed_data, connection_targets, extras) = if let Some(transformer) =
            &self.transformers[input_id]
        {
            let transformer = transformer.lock().unwrap();
            let (engine, ast) = &*transformer;

            let mut scope = Scope::new();

            let map_data: rhai::Map = serde_json::from_value(serde_json::to_value(&data)?)?;
            scope.push_constant("SCANNED_DATA", map_data);

            let result: rhai::Dynamic = engine.eval_ast_with_scope(&mut scope, ast)?;
            trace!("got result from transformer: {result:?}");

            if let ScannedData::Url { url } = &data {
                let open = scope
                    .get_value("OPEN_URL")
                    .tap_some(|open| trace!(open, "transformer had opinion about opening url"))
                    .unwrap_or(self.open_urls[input_id]);

                if open {
                    debug!(url, "opening url");
                    if let Err(err) = open::that_detached(url) {
                        error!("could not open url: {err}");
                    }
                }
            }

            let connection_targets = scope
                .get_value::<rhai::Array>("CONNECTION_TARGETS")
                .tap_some(|targets| trace!(?targets, "got targets from transformer"))
                .map(|targets| {
                    let targets = targets
                        .into_iter()
                        .filter_map(|target| target.into_string().ok());
                    Some(Arc::new(HashSet::from_iter(targets)))
                })
                .unwrap_or_else(|| self.connection_targets[input_id].clone());

            let extras: HashMap<String, String> = serde_json::from_value(serde_json::to_value(
                scope.get_value::<rhai::Map>("EXTRAS").unwrap_or_default(),
            )?)?;

            if result.is_unit() {
                trace!("result was unit, ignoring transformer");
                (None, connection_targets, extras)
            } else {
                (
                    Some(serde_json::to_value(result)?),
                    connection_targets,
                    extras,
                )
            }
        } else {
            (
                None,
                self.connection_targets[input_id].clone(),
                Default::default(),
            )
        };

        Ok(Some(ScanResult {
            input_id,
            data,
            transformed_data,
            connection_targets,
            extras,
        }))
    }

    async fn parse_scanned_data(
        &self,
        input_id: usize,
        data: String,
    ) -> Option<super::ScannedData> {
        let decoder_types = &self.input_decoders[input_id];

        if data.starts_with('@') && decoder_types.contains(&DecoderType::Aamva) {
            if let Ok(data) = aamva::parse_barcode(&data)
                .tap_err(|err| warn!("expected aamva data could not be decoded: {err}"))
            {
                return Some(ScannedData::Aamva(Box::new(data.into())));
            }
        }

        if data.starts_with("shc:/") && decoder_types.contains(&DecoderType::Shc) {
            if let Ok(data) = self
                .shc_decoder
                .decode(&data)
                .await
                .tap_err(|err| warn!("expected shc data could not be decoded: {err}"))
            {
                return Some(ScannedData::Shc(Box::new(data)));
            }
        }

        if decoder_types.contains(&DecoderType::Url) && Url::parse(&data).is_ok() {
            return Some(ScannedData::Url { url: data });
        }

        if decoder_types.contains(&DecoderType::Generic) {
            return Some(ScannedData::Generic { data });
        }

        None
    }
}
