use std::{collections::HashMap, ffi::OsStr, path::PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::{io::AsyncReadExt, sync::mpsc};
use tracing::{debug, instrument};

use crate::decoder::{Decoder, DecoderOutcome, DecoderType};

#[derive(Clone, Default, Debug, Deserialize)]
pub struct MdlConfig {
    pub input_name: Option<String>,
    pub timeout: Option<u64>,
    #[serde(default)]
    pub extract_portraits: bool,
    pub request_elements: Option<HashMap<String, HashMap<String, bool>>>,
    pub certificates_path: Option<PathBuf>,
    pub nfc_connstring: Option<String>,
}

impl MdlConfig {
    fn default_elements() -> HashMap<String, HashMap<String, bool>> {
        vec![(
            "org.iso.18013.5.1".to_string(),
            vec![
                ("portrait".to_string(), false),
                ("given_name".to_string(), false),
                ("family_name".to_string(), false),
                ("birth_date".to_string(), false),
            ]
            .into_iter()
            .collect(),
        )]
        .into_iter()
        .collect()
    }
}

#[instrument(skip_all)]
pub async fn create_mdl_verifier(config: MdlConfig) -> eyre::Result<mdl_verifier::MdlVerifier> {
    let certs = load_certs(config.certificates_path).await?;
    let requested_elements = config
        .request_elements
        .unwrap_or_else(MdlConfig::default_elements);

    let mut verifier =
        mdl_verifier::MdlVerifier::new(certs, requested_elements, config.timeout.unwrap_or(120))?;

    if let Some(connstring) = config.nfc_connstring {
        verifier.add_nfc_stream(connstring)?;
    }

    Ok(verifier)
}

async fn load_certs(path: Option<PathBuf>) -> eyre::Result<Vec<String>> {
    let mut entries =
        tokio::fs::read_dir(path.unwrap_or_else(|| PathBuf::from("mdl-certificates"))).await?;

    let mut certs = Vec::new();
    while let Ok(Some(file)) = entries.next_entry().await {
        if file.path().extension() != Some(OsStr::new("pem")) {
            continue;
        }

        debug!(path = %file.path().display(), "loading certificate");
        let mut file = tokio::fs::File::open(file.path()).await?;
        let mut data = String::new();
        file.read_to_string(&mut data).await?;
        certs.push(data);
    }

    Ok(certs)
}

pub struct MdlDecoder {
    qr_sender: mpsc::Sender<String>,
}

impl MdlDecoder {
    pub async fn new(qr_sender: mpsc::Sender<String>) -> Self {
        Self { qr_sender }
    }
}

#[async_trait]
impl Decoder for MdlDecoder {
    fn decoder_type(&self) -> DecoderType {
        DecoderType::Mdl
    }

    async fn decode(&self, data: &str) -> eyre::Result<DecoderOutcome> {
        if !data.starts_with("mdoc:") {
            return Ok(DecoderOutcome::Skipped);
        }

        self.qr_sender.send(data.to_string()).await?;
        Ok(DecoderOutcome::Consumed)
    }
}
