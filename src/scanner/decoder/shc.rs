use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use eyre::{bail, OptionExt};
use futures::StreamExt;
use jsonwebtoken::jwk::JwkSet;
use regex::Regex;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use tap::TapFallible;
use time::{
    format_description::well_known::Rfc3339, macros::format_description, Date, PrimitiveDateTime,
};
use tracing::{debug, instrument, trace, warn};

#[derive(Debug, Deserialize, Serialize)]
pub struct ShcConfig {
    #[serde(default)]
    skip_updates: bool,
    #[serde(default)]
    prohibit_lookups: bool,
    #[serde(default = "default_cache_dir")]
    cache_dir: PathBuf,
}

impl Default for ShcConfig {
    fn default() -> Self {
        Self {
            prohibit_lookups: false,
            skip_updates: false,
            cache_dir: default_cache_dir(),
        }
    }
}

fn default_cache_dir() -> PathBuf {
    PathBuf::from("shc-cache")
}

pub struct ShcDecoder {
    prohibit_lookups: bool,
    client: reqwest::Client,
    vci_issuers: Vec<VciIssuerMeta>,
    cvx_codes: HashMap<SmolStr, Arc<CvxCode>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShcData {
    pub verified: bool,
    pub issuer: VciIssuer,
    pub known_issuer: bool,
    pub cvx_codes: HashMap<SmolStr, Arc<CvxCode>>,
    pub entries: Vec<FhirBundleEntry>,
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
    pub code: SmolStr,
    pub short_description: String,
    pub full_name: String,
    pub notes: String,
    pub vaccine_status: bool,
    pub last_updated: Date,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FhirBundleEntry {
    pub full_url: String,
    pub resource: FhirBundleEntryResource,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "resourceType")]
pub enum FhirBundleEntryResource {
    Patient(FhirPatient),
    Immunization(FhirImmunization),
    #[serde(untagged)]
    Other(serde_json::Value),
}

#[derive(Clone, Debug)]
pub enum FhirDateTime {
    Date { date: Date },
    DateTime { date_time: PrimitiveDateTime },
    YearMonth { year: u64, month: u64 },
    Year { year: u64 },
}

impl serde::Serialize for FhirDateTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        static DATE_FORMAT: &[time::format_description::FormatItem<'_>] =
            format_description!("[year]-[month]-[day]");

        match self {
            Self::Date { date } => serializer.serialize_str(
                &date
                    .format(&DATE_FORMAT)
                    .map_err(serde::ser::Error::custom)?,
            ),
            Self::DateTime { date_time } => serializer.serialize_str(
                &date_time
                    .format(&Rfc3339)
                    .map_err(serde::ser::Error::custom)?,
            ),
            Self::YearMonth { year, month } => {
                serializer.collect_str(&format_args!("{year:04}-{month:02}"))
            }
            Self::Year { year } => serializer.collect_str(&format_args!("{year:04}")),
        }
    }
}

impl<'de> serde::de::Deserialize<'de> for FhirDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data: String = serde::Deserialize::deserialize(deserializer)?;

        static DATE_FORMAT: &[time::format_description::FormatItem<'_>] =
            format_description!("[year]-[month]-[day]");

        let year_month_format = Regex::new(r"(\d{4})-(\d{2})").unwrap();
        let year_format = Regex::new(r"\d{4}").unwrap();

        if let Ok(date) = Date::parse(&data, &DATE_FORMAT) {
            Ok(Self::Date { date })
        } else if let Ok(date_time) = PrimitiveDateTime::parse(&data, &Rfc3339) {
            Ok(Self::DateTime { date_time })
        } else if let Some(captures) = year_month_format.captures(&data) {
            Ok(Self::YearMonth {
                year: captures[1].parse().map_err(serde::de::Error::custom)?,
                month: captures[2].parse().map_err(serde::de::Error::custom)?,
            })
        } else if year_format.is_match(&data) {
            Ok(Self::Year {
                year: data.parse().map_err(serde::de::Error::custom)?,
            })
        } else {
            Err(serde::de::Error::custom(format!(
                "unknown date format: {data}"
            )))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FhirPatient {
    birth_date: FhirDateTime,
    name: Vec<PatientName>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FhirImmunization {
    lot_number: Option<String>,
    occurrence_date_time: FhirDateTime,
    patient: Reference,
    performer: Vec<Performer>,
    status: String,
    vaccine_code: VaccineCode,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Coding {
    pub code: SmolStr,
    pub system: String,
}

impl ShcDecoder {
    const VCI_ISSUERS: &'static str =
        "https://raw.githubusercontent.com/the-commons-project/vci-directory/main/vci-issuers.json";
    const CVX_CODES: &'static str =
        "https://www2a.cdc.gov/vaccines/iis/iisstandards/downloads/cvx.txt";

    const MAX_AGE_SECS: u64 = 60 * 60 * 24 * 7;

    pub async fn new(config: ShcConfig) -> eyre::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?;

        if !config.cache_dir.exists() {
            tokio::fs::create_dir_all(&config.cache_dir).await?;
        }

        let mut shc_decoder = Self {
            prohibit_lookups: config.prohibit_lookups,
            client,
            vci_issuers: Default::default(),
            cvx_codes: Default::default(),
        };

        shc_decoder.vci_issuers = shc_decoder
            .load_vci_issuers(
                config.cache_dir.join("vci.json").as_path(),
                config.skip_updates,
            )
            .await?;
        shc_decoder.cvx_codes = shc_decoder
            .load_cvx_codes(
                config.cache_dir.join("cvx.json").as_path(),
                config.skip_updates,
            )
            .await?;

        Ok(shc_decoder)
    }

    #[instrument(skip_all, fields(iss))]
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
        trace!(?header, "got jwt header");

        let payload_parts: Vec<_> = decoded_data.split('.').collect();
        eyre::ensure!(payload_parts.len() == 3, "must have exactly 3 parts");

        let header_data: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload_parts[0])?)?;
        let main_data: Cow<'_, str> = if header_data["zip"].as_str() == Some("DEF") {
            trace!("header marked data as compressed");

            let main_data = URL_SAFE_NO_PAD.decode(payload_parts[1])?;

            let mut deflater = flate2::read::DeflateDecoder::new(main_data.as_slice());

            let mut decompressed_data = String::new();
            deflater.read_to_string(&mut decompressed_data)?;
            decompressed_data.into()
        } else {
            trace!("data was not marked as compressed");
            payload_parts[1].into()
        };

        let mut main_data: serde_json::Value = serde_json::from_str(&main_data)?;
        trace!("decoded main data: {main_data:?}");

        let iss = main_data["iss"]
            .as_str()
            .ok_or_eyre("data was missing issuer")?;
        let kid = header.kid.ok_or_eyre("header was missing key id")?;

        tracing::Span::current().record("iss", iss);
        debug!("extracted iss from jwt");

        let issuer = self
            .vci_issuers
            .iter()
            .find(|issuer| issuer.jwk_set.find(&kid).is_some());

        let issuer = if let Some(issuer) = issuer {
            debug!("issuer was known");
            Some((issuer.clone(), true))
        } else if self.prohibit_lookups {
            warn!("unknown issuer and lookups prohibited");
            None
        } else {
            debug!("attempting to look up issuer");

            let existing_issuer = self
                .vci_issuers
                .iter()
                .find(|issuer| issuer.issuer.iss == iss)
                .map(|issuer| issuer.issuer.clone());

            let known_issuer = existing_issuer.is_some();

            let issuer = existing_issuer.unwrap_or_else(|| VciIssuer {
                iss: iss.to_string(),
                name: iss.to_string(),
                website: None,
                canonical_iss: None,
            });

            self.load_vci_issuer(issuer)
                .await
                .tap_err(|err| warn!("could not load issuer: {err}"))
                .ok()
                .map(|issuer| (issuer, known_issuer))
        };

        let Some((issuer, known_issuer)) = issuer else {
            bail!("could not find issuer");
        };
        trace!("found issuer data with {} keys", issuer.jwk_set.keys.len());

        let Some(jwk) = issuer.jwk_set.find(&kid) else {
            bail!("could not find jwk key in issuer");
        };
        debug!(key_id = jwk.common.key_id, "found key id");

        let key = jsonwebtoken::DecodingKey::from_jwk(jwk)?;

        let message = &decoded_data[..decoded_data.rfind('.').expect("jwt must have delimiters")];
        let verified = jsonwebtoken::crypto::verify(
            payload_parts[2],
            message.as_bytes(),
            &key,
            jsonwebtoken::Algorithm::ES256,
        )?;
        debug!(verified, "checked validity of jwt");

        let entries: Vec<FhirBundleEntry> = serde_json::from_value(
            main_data["vc"]["credentialSubject"]["fhirBundle"]["entry"].take(),
        )?;
        debug!(len = entries.len(), "got fhir bundle entries");

        let cvx_codes: HashMap<_, _> = entries
            .iter()
            .filter_map(|entry| match &entry.resource {
                FhirBundleEntryResource::Immunization(immunization) => {
                    Some(&immunization.vaccine_code.coding)
                }
                _ => None,
            })
            .flatten()
            .filter_map(|coding| {
                if coding.system != "http://hl7.org/fhir/sid/cvx" {
                    return None;
                }

                self.cvx_codes.get(&coding.code).cloned()
            })
            .map(|cvx_code| (cvx_code.code.clone(), cvx_code))
            .collect();
        debug!(len = cvx_codes.len(), "found referenced cvx codes");

        let data = ShcData {
            verified,
            cvx_codes,
            issuer: issuer.issuer,
            known_issuer,
            entries,
        };

        Ok(data)
    }

    #[instrument(skip_all)]
    async fn load_vci_issuers(
        &self,
        path: &Path,
        skip_updates: bool,
    ) -> eyre::Result<Vec<VciIssuerMeta>> {
        if Self::file_is_new_enough(path, skip_updates) {
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

        if skip_updates {
            warn!("no cached data and skipping vci issuer updates");
            return Ok(Default::default());
        }

        let issuers: VciIssuers = self
            .client
            .get(Self::VCI_ISSUERS)
            .send()
            .await?
            .json()
            .await?;

        let futs = futures::stream::iter(
            issuers
                .participating_issuers
                .into_iter()
                .map(|issuer| self.load_vci_issuer(issuer)),
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
    async fn load_vci_issuer(&self, issuer: VciIssuer) -> eyre::Result<VciIssuerMeta> {
        debug!("loading issuer");

        let jwk_set = self
            .get_jwks(&issuer.iss)
            .await
            .tap_err(|err| warn!("could not load issuer: {err}"))
            .unwrap_or_else(|_| JwkSet { keys: vec![] });

        Ok(VciIssuerMeta { issuer, jwk_set })
    }

    async fn get_jwks(&self, iss: &str) -> eyre::Result<JwkSet> {
        Ok(self
            .client
            .get(format!("{}/.well-known/jwks.json", iss))
            .send()
            .await?
            .json()
            .await?)
    }

    #[instrument(skip_all)]
    async fn load_cvx_codes(
        &self,
        path: &Path,
        skip_updates: bool,
    ) -> eyre::Result<HashMap<SmolStr, Arc<CvxCode>>> {
        static DATE_FORMAT: &[time::format_description::FormatItem<'_>] =
            format_description!("[year]/[month]/[day]");

        if Self::file_is_new_enough(path, skip_updates) {
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

        if skip_updates {
            warn!("no cached data and skipping cvx code updates");
            return Ok(Default::default());
        }

        let data = self
            .client
            .get(Self::CVX_CODES)
            .send()
            .await?
            .text()
            .await?;

        let codes: HashMap<_, _> = data
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('|');

                let code = SmolStr::new(parts.next()?.trim());

                Some((
                    code.clone(),
                    Arc::new(CvxCode {
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
                    }),
                ))
            })
            .collect();

        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(file, &codes)?;

        Ok(codes)
    }

    #[instrument]
    fn file_is_new_enough(path: &Path, skip_updates: bool) -> bool {
        if !path.exists() {
            debug!("path did not exist");
            return false;
        }

        if skip_updates {
            debug!("skipping updates, ignoring file modification check");
            return true;
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
