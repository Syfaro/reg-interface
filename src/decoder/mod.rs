use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use isomdl::presentation::authentication::ResponseAuthenticationOutcome;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

pub mod mdl;
pub mod shc;

#[derive(Clone, Default, Debug, Deserialize)]
pub struct DecoderConfig {
    pub enabled_decoders: Option<HashSet<DecoderType>>,
    #[serde(default)]
    pub mdl: mdl::MdlConfig,
    #[serde(default)]
    pub shc: shc::ShcConfig,
    #[serde(default)]
    pub url: UrlConfig,
}

#[derive(Clone, Default, Debug, Deserialize)]
pub struct UrlConfig {
    open_urls: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderType {
    Aamva,
    Mdl,
    Shc,
    Url,
    Generic,
}

impl std::fmt::Display for DecoderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Aamva => write!(f, "AAMVA"),
            Self::Mdl => write!(f, "mDL"),
            Self::Shc => write!(f, "SHC"),
            Self::Url => write!(f, "URL"),
            Self::Generic => write!(f, "Generic"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum DecoderOutcome {
    Skipped,
    Consumed,
    DecodedData(DecodedData),
}

#[derive(Clone, Debug, Serialize)]
pub struct DecodedDataContext {
    pub input_name: String,
    pub decoder_type: DecoderType,
    pub data: DecodedData,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodedData {
    #[serde(rename = "AAMVA")]
    Aamva(Box<aamva::DecodedData>),
    #[serde(rename = "mDL")]
    Mdl(ResponseAuthenticationOutcome),
    #[serde(rename = "SHC")]
    Shc(shc::ShcData),
    Url(String),
    Generic(String),
}

impl DecodedData {
    pub fn to_dynamic(&self) -> eyre::Result<rhai::Dynamic> {
        let dynamic = match self {
            DecodedData::Aamva(data) => rhai::serde::to_dynamic(data)?,
            DecodedData::Mdl(data) => rhai::serde::to_dynamic(data)?,
            DecodedData::Shc(data) => rhai::serde::to_dynamic(data)?,
            DecodedData::Url(data) => rhai::serde::to_dynamic(data)?,
            DecodedData::Generic(data) => rhai::serde::to_dynamic(data)?,
        };

        Ok(dynamic)
    }
}

impl DecodedData {
    pub fn decoder_type(&self) -> DecoderType {
        match self {
            DecodedData::Aamva(_) => DecoderType::Aamva,
            DecodedData::Mdl(_) => DecoderType::Mdl,
            DecodedData::Shc(_) => DecoderType::Shc,
            DecodedData::Url(_) => DecoderType::Url,
            DecodedData::Generic(_) => DecoderType::Generic,
        }
    }
}

#[async_trait]
pub trait Decoder {
    fn decoder_type(&self) -> DecoderType;

    async fn decode(&self, data: &str) -> eyre::Result<DecoderOutcome>;
    async fn post_transform(
        &self,
        _original_data: Option<&str>,
        transformed_data: serde_json::Value,
        _changes: &HashMap<String, serde_json::Value>,
    ) -> eyre::Result<serde_json::Value> {
        Ok(transformed_data)
    }
}

pub struct DecoderManager {
    decoders: Vec<Box<dyn Decoder + Send + Sync>>,
}

impl DecoderManager {
    pub async fn new(config: DecoderConfig, qr_sender: mpsc::Sender<String>) -> eyre::Result<Self> {
        let enabled_decoders = config.enabled_decoders.unwrap_or_else(|| {
            [DecoderType::Aamva, DecoderType::Url, DecoderType::Generic]
                .into_iter()
                .collect()
        });

        let mut decoders: Vec<Box<dyn Decoder + Send + Sync>> =
            Vec::with_capacity(enabled_decoders.len());

        if enabled_decoders.contains(&DecoderType::Aamva) {
            decoders.push(Box::new(AamvaDecoder));
        }

        if enabled_decoders.contains(&DecoderType::Mdl) {
            decoders.push(Box::new(mdl::MdlDecoder::new(qr_sender).await));
        }

        if enabled_decoders.contains(&DecoderType::Shc) {
            decoders.push(Box::new(shc::ShcDecoder::new(config.shc).await?));
        }

        if enabled_decoders.contains(&DecoderType::Url) {
            decoders.push(Box::new(UrlDecoder::new(config.url)));
        }

        if enabled_decoders.contains(&DecoderType::Generic) {
            decoders.push(Box::new(GenericDecoder));
        }

        Ok(Self { decoders })
    }

    pub async fn decode(&self, data: &str) -> eyre::Result<Option<DecodedData>> {
        for decoder in self.decoders.iter() {
            match decoder.decode(data).await? {
                DecoderOutcome::Skipped => continue,
                DecoderOutcome::Consumed => break,
                DecoderOutcome::DecodedData(data) => return Ok(Some(data)),
            }
        }

        Ok(None)
    }

    pub async fn post_transform(
        &self,
        decoder_type: DecoderType,
        original_data: Option<&str>,
        transformed_data: serde_json::Value,
        changes: &HashMap<String, serde_json::Value>,
    ) -> eyre::Result<serde_json::Value> {
        let Some(decoder) = self
            .decoders
            .iter()
            .find(|decoder| decoder.decoder_type() == decoder_type)
        else {
            return Ok(transformed_data);
        };

        decoder
            .post_transform(original_data, transformed_data, changes)
            .await
    }
}

pub struct AamvaDecoder;

#[async_trait]
impl Decoder for AamvaDecoder {
    fn decoder_type(&self) -> DecoderType {
        DecoderType::Aamva
    }

    async fn decode(&self, data: &str) -> eyre::Result<DecoderOutcome> {
        if !data.starts_with('@') {
            return Ok(DecoderOutcome::Skipped);
        }

        match aamva::parse_barcode(data) {
            Ok(data) => Ok(DecoderOutcome::DecodedData(DecodedData::Aamva(Box::new(
                aamva::DecodedData::from(data),
            )))),
            Err(err) => {
                warn!("expected aamva data could not be decoded: {err}");
                Ok(DecoderOutcome::Consumed)
            }
        }
    }
}

pub struct UrlDecoder {
    open_urls: bool,
}

impl UrlDecoder {
    pub fn new(config: UrlConfig) -> Self {
        Self {
            open_urls: config.open_urls,
        }
    }
}

#[async_trait]
impl Decoder for UrlDecoder {
    fn decoder_type(&self) -> DecoderType {
        DecoderType::Url
    }

    async fn decode(&self, data: &str) -> eyre::Result<DecoderOutcome> {
        if url::Url::parse(data).is_ok() {
            Ok(DecoderOutcome::DecodedData(DecodedData::Url(
                data.to_string(),
            )))
        } else {
            Ok(DecoderOutcome::Skipped)
        }
    }

    async fn post_transform(
        &self,
        original_data: Option<&str>,
        transformed_data: serde_json::Value,
        changes: &HashMap<String, serde_json::Value>,
    ) -> eyre::Result<serde_json::Value> {
        if let Some(original_data) = original_data {
            if changes
                .get("open_url")
                .and_then(|open_url| open_url.as_bool())
                .unwrap_or(self.open_urls)
            {
                open::that_detached(original_data)?;
            }
        }

        Ok(transformed_data)
    }
}

pub struct GenericDecoder;

#[async_trait]
impl Decoder for GenericDecoder {
    fn decoder_type(&self) -> DecoderType {
        DecoderType::Generic
    }

    async fn decode(&self, data: &str) -> eyre::Result<DecoderOutcome> {
        Ok(DecoderOutcome::DecodedData(DecodedData::Generic(
            data.to_string(),
        )))
    }
}
