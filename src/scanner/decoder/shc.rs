use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use eyre::OptionExt;
use futures::StreamExt;
use jsonwebtoken::jwk::JwkSet;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr, PickFirst};
use time::{macros::format_description, Date};
use tracing::{debug, instrument, warn};

#[derive(Debug, Deserialize, Serialize)]
pub struct ShcConfig {
    #[serde(default)]
    skip_updates: bool,
    cache_dir: PathBuf,
}

impl Default for ShcConfig {
    fn default() -> Self {
        Self {
            skip_updates: false,
            cache_dir: PathBuf::from("shc-cache"),
        }
    }
}

pub struct ShcDecoder {
    client: reqwest::Client,
    vci_issuers: Vec<VciIssuerMeta>,
    cvx_codes: HashMap<u32, CvxCode>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShcData {
    pub verified: bool,
    pub entries: Vec<FhirBundleEntry>,
    pub issuer: VciIssuer,
    pub cvx_codes: HashMap<u32, CvxCode>,
}

#[derive(Debug, Deserialize, Serialize)]
struct VciIssuers {
    participating_issuers: Vec<VciIssuer>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VciIssuer {
    pub iss: String,
    pub name: String,
    pub website: Option<String>,
    pub canonical_iss: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VciIssuerMeta {
    issuer: VciIssuer,
    jwk_set: JwkSet,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CvxCode {
    pub code: u32,
    pub short_description: String,
    pub full_name: String,
    pub notes: String,
    pub vaccine_status: bool,
    pub last_updated: Date,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FhirBundleEntry {
    #[serde(rename = "fullUrl")]
    pub full_url: String,
    pub resource: FhirBundleEntryResource,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "resourceType", rename_all_fields = "camelCase")]
pub enum FhirBundleEntryResource {
    Patient {
        birth_date: String,
        name: Vec<PatientName>,
    },
    Immunization {
        lot_number: Option<String>,
        occurrence_date_time: String,
        patient: Reference,
        performer: Vec<Performer>,
        status: String,
        vaccine_code: VaccineCode,
    },
    #[serde(untagged)]
    Other(serde_json::Value),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PatientName {
    pub family: String,
    pub given: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Reference {
    pub reference: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Performer {
    pub actor: Actor,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Actor {
    pub display: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VaccineCode {
    pub coding: Vec<Coding>,
}

#[serde_as]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Coding {
    #[serde_as(as = "PickFirst<(_, DisplayFromStr)>")]
    pub code: u32,
    pub system: String,
}

impl ShcDecoder {
    const VCI_ISSUERS: &'static str =
        "https://raw.githubusercontent.com/the-commons-project/vci-directory/main/vci-issuers.json";
    const CVX_CODES: &'static str =
        "https://www2a.cdc.gov/vaccines/iis/iisstandards/downloads/cvx.txt";

    const MAX_AGE_SECS: u64 = 60 * 60 * 24 * 7;

    pub async fn new(config: ShcConfig) -> eyre::Result<Self> {
        let client = reqwest::Client::default();

        if !config.cache_dir.exists() {
            tokio::fs::create_dir_all(&config.cache_dir).await?;
        }

        let vci_issuers =
            Self::load_vci_issuers(&client, config.cache_dir.join("vci.json").as_path()).await?;
        let cvx_codes =
            Self::load_cvx_codes(&client, config.cache_dir.join("cvx.json").as_path()).await?;

        Ok(Self {
            client,
            vci_issuers,
            cvx_codes,
        })
    }

    pub async fn decode(&self, data: &str) -> eyre::Result<ShcData> {
        let data = data
            .trim()
            .strip_prefix("shc:/")
            .ok_or_eyre("missing shc prefix")?;
        eyre::ensure!(data.len() % 2 == 0, "data length must be even");

        let mut decoded_data = String::with_capacity(data.len() / 2);
        for pos in 0..(data.len() / 2) {
            let chunk = &data[pos * 2..=pos * 2 + 1];
            let num: u8 = chunk.parse()?;
            let ch = (num + 45) as char;
            decoded_data.push(ch);
        }
        let header = jsonwebtoken::decode_header(&decoded_data)?;

        let payload_parts: Vec<_> = decoded_data.split('.').collect();
        eyre::ensure!(payload_parts.len() == 3, "must have exactly 3 parts");

        let header_data: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload_parts[0])?)?;
        let main_data: Cow<'_, str> = if header_data["zip"].as_str() == Some("DEF") {
            let main_data = URL_SAFE_NO_PAD.decode(payload_parts[1])?;

            let mut deflater = flate2::read::DeflateDecoder::new(main_data.as_slice());

            let mut decompressed_data = String::new();
            deflater.read_to_string(&mut decompressed_data)?;
            decompressed_data.into()
        } else {
            payload_parts[1].into()
        };

        let mut main_data: serde_json::Value = serde_json::from_str(&main_data)?;

        let iss = main_data["iss"]
            .as_str()
            .ok_or_eyre("data was missing issuer")?;
        let kid = header.kid.ok_or_eyre("header was missing key ID")?;

        let issuer = self
            .vci_issuers
            .iter()
            .find(|issuer| issuer.jwk_set.find(&kid).is_some());

        let issuer = if let Some(issuer) = issuer {
            issuer.clone()
        } else {
            let issuer_name = self
                .vci_issuers
                .iter()
                .find(|issuer| issuer.issuer.iss == iss)
                .map(|issuer| issuer.issuer.name.as_str())
                .unwrap_or("Unknown Issuer")
                .to_string();

            Self::load_vci_issuer(
                &self.client,
                VciIssuer {
                    iss: iss.to_string(),
                    name: issuer_name,
                    website: None,
                    canonical_iss: None,
                },
            )
            .await?
        };

        let jwk = issuer
            .jwk_set
            .find(&kid)
            .expect("key id was just found in issuer key set");

        let key = jsonwebtoken::DecodingKey::from_jwk(jwk)?;

        let message = &decoded_data[..decoded_data.rfind('.').expect("jwt must have delimiters")];
        let verified = jsonwebtoken::crypto::verify(
            payload_parts[2],
            message.as_bytes(),
            &key,
            jsonwebtoken::Algorithm::ES256,
        )?;

        let entries: Vec<FhirBundleEntry> = serde_json::from_value(
            main_data["vc"]["credentialSubject"]["fhirBundle"]["entry"].take(),
        )?;

        let referenced_cvx_codes = entries
            .iter()
            .filter_map(|entry| match &entry.resource {
                FhirBundleEntryResource::Immunization { vaccine_code, .. } => {
                    Some(&vaccine_code.coding)
                }
                _ => None,
            })
            .flat_map(|coding| coding.iter().map(|coding| coding.code))
            .filter_map(|code| self.cvx_codes.get(&code).cloned())
            .map(|cvx_code| (cvx_code.code, cvx_code));

        let data = ShcData {
            verified,
            cvx_codes: referenced_cvx_codes.collect(),
            issuer: issuer.issuer,
            entries,
        };

        Ok(data)
    }

    #[instrument(skip_all)]
    async fn load_vci_issuers(
        client: &reqwest::Client,
        path: &Path,
    ) -> eyre::Result<Vec<VciIssuerMeta>> {
        if Self::file_is_new_enough(path) {
            let file = std::fs::File::open(path)?;
            match serde_json::from_reader(file) {
                Ok(codes) => {
                    debug!("loaded vci issuers from file");
                    return Ok(codes);
                }
                Err(err) => {
                    warn!("existing file could not be decoded: {err}");
                }
            }
        }

        let issuers: VciIssuers = client.get(Self::VCI_ISSUERS).send().await?.json().await?;

        let futs = futures::stream::iter(
            issuers
                .participating_issuers
                .into_iter()
                .map(|issuer| Self::load_vci_issuer(client, issuer)),
        );
        let issuer_metas: Vec<_> = futs
            .buffer_unordered(4)
            .filter_map(|res| futures::future::ready(res.ok()))
            .collect()
            .await;

        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(file, &issuer_metas)?;

        Ok(issuer_metas)
    }

    #[instrument(skip_all, fields(iss = issuer.iss))]
    async fn load_vci_issuer(
        client: &reqwest::Client,
        issuer: VciIssuer,
    ) -> eyre::Result<VciIssuerMeta> {
        debug!("loading issuer");

        let jwk_set = Self::get_jwks(client, &issuer.iss)
            .await
            .unwrap_or_else(|_| JwkSet { keys: vec![] });

        Ok(VciIssuerMeta { issuer, jwk_set })
    }

    async fn get_jwks(client: &reqwest::Client, iss: &str) -> eyre::Result<JwkSet> {
        Ok(client
            .get(format!("{}/.well-known/jwks.json", iss))
            .send()
            .await?
            .json()
            .await?)
    }

    #[instrument(skip_all)]
    async fn load_cvx_codes(
        client: &reqwest::Client,
        path: &Path,
    ) -> eyre::Result<HashMap<u32, CvxCode>> {
        static DATE_FORMAT: &[time::format_description::FormatItem<'_>] =
            format_description!("[year]/[month]/[day]");

        if Self::file_is_new_enough(path) {
            let file = std::fs::File::open(path)?;
            match serde_json::from_reader(file) {
                Ok(codes) => {
                    debug!("loaded cvx codes from file");
                    return Ok(codes);
                }
                Err(err) => {
                    warn!("existing file could not be decoded: {err}");
                }
            }
        }

        let data = client.get(Self::CVX_CODES).send().await?.text().await?;

        let codes: HashMap<u32, CvxCode> = data
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('|');

                let code = parts.next()?.trim().parse().ok()?;

                Some((
                    code,
                    CvxCode {
                        code,
                        short_description: parts.next()?.to_string(),
                        full_name: parts.next()?.to_string(),
                        notes: parts.next()?.to_string(),
                        vaccine_status: match parts.nth(1)? {
                            "True" => true,
                            "False" => false,
                            val => {
                                warn!("unknown vaccine status: {val}");
                                return None;
                            }
                        },
                        last_updated: Date::parse(parts.next()?, &DATE_FORMAT).ok()?,
                    },
                ))
            })
            .collect();

        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(file, &codes)?;

        Ok(codes)
    }

    #[instrument]
    fn file_is_new_enough(path: &Path) -> bool {
        if !path.exists() {
            debug!("path did not exist");
            return false;
        }

        let Ok(metadata) = path.metadata() else {
            warn!("could not get file metadata");
            return false;
        };

        let Ok(modified) = metadata.modified() else {
            warn!("could not get file modified time");
            return false;
        };

        let Ok(elapsed) = modified.elapsed() else {
            warn!("could not calculate elapsed time");
            return false;
        };

        elapsed.as_secs() < Self::MAX_AGE_SECS
    }
}
