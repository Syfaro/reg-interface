use std::{collections::HashMap, ffi::OsStr, io::Cursor, path::PathBuf, sync::Arc, time::Duration};

use axum::{body::Body, extract::Path, response::Response, routing::get, Extension, Router};
use btleplug::{
    api::{Central as _, CentralEvent, Manager as _, Peripheral as _, ScanFilter},
    platform::{Adapter, Manager},
};
use eyre::OptionExt;
use futures::StreamExt;
use hex_literal::hex;
use image::DynamicImage;
use isomdl::{
    definitions::{
        device_request::{self, DataElements, Namespaces},
        helpers::Tag24,
        x509::trust_anchor::{PemTrustAnchor, TrustAnchorRegistry, TrustPurpose},
        BleOptions, DeviceEngagement,
    },
    presentation::{
        authentication::ResponseAuthenticationOutcome,
        reader::{self},
    },
};
use jpeg2k::Image;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tap::TapFallible;
use tokio::{io::AsyncReadExt, sync::Mutex};
use tracing::{debug, instrument, trace, warn};
use uuid::Uuid;

static STATE_ID: Uuid = Uuid::from_bytes(hex!("00000001 A123 48CE 896B 4C76973373E6"));
static CLIENT2SERVER_ID: Uuid = Uuid::from_bytes(hex!("00000002 A123 48CE 896B 4C76973373E6"));
static SERVER2CLIENT_ID: Uuid = Uuid::from_bytes(hex!("00000003 A123 48CE 896B 4C76973373E6"));

/// Maximum length of data to read from a peripheral before aborting.
const MAX_PAYLOAD_SIZE: usize = 512 * 1000;

#[derive(Debug, Deserialize, Serialize)]
pub struct MdlConfig {
    timeout: u64,
    request_attributes: HashMap<String, Vec<String>>,
    certificates: PathBuf,
}

impl Default for MdlConfig {
    fn default() -> Self {
        Self {
            timeout: 60,
            request_attributes: vec![(
                "org.iso.18013.5.1".into(),
                vec![
                    "portrait".to_string(),
                    "given_name".to_string(),
                    "family_name".to_string(),
                    "birth_date".to_string(),
                ],
            )]
            .into_iter()
            .collect(),
            certificates: PathBuf::new(),
        }
    }
}

type Portraits = Arc<Mutex<HashMap<String, Vec<u8>>>>;

pub struct MdlDecoder {
    timeout: u64,
    central: Adapter,
    trust_anchors: TrustAnchorRegistry,
    requested_elements: device_request::Namespaces,
    portraits: Portraits,
}

async fn serve_portrait(
    Extension(portraits): Extension<Portraits>,
    Path(hash): Path<String>,
) -> Response {
    let response = Response::builder();

    if let Some(data) = portraits.lock().await.remove(&hash) {
        response
            .header("content-type", "image/jpeg")
            .header("content-disposition", "inline")
            .body(Body::from(data))
    } else {
        response.status(StatusCode::NOT_FOUND).body(Body::empty())
    }
    .expect("failed to build response")
}

impl MdlDecoder {
    pub async fn new(config: MdlConfig) -> eyre::Result<(Self, Router<()>)> {
        let mut namespaces = config.request_attributes.into_iter();

        let Some((first_namespace, first_namespace_elements)) = namespaces.next() else {
            eyre::bail!("must be at least one namespace");
        };

        let mut requested_elements = Namespaces::new(
            first_namespace,
            Self::convert_elements(first_namespace_elements)?,
        );
        for (namespace, elements) in namespaces {
            requested_elements.insert(namespace, Self::convert_elements(elements)?);
        }

        let mut certs = Vec::new();
        let mut entries = tokio::fs::read_dir(config.certificates).await?;
        while let Ok(Some(file)) = entries.next_entry().await {
            if file.path().extension() != Some(OsStr::new("pem")) {
                continue;
            }
            debug!(path = %file.path().display(), "loading certificate");
            let mut file = tokio::fs::File::open(file.path()).await?;
            let mut data = String::new();
            file.read_to_string(&mut data).await?;
            certs.push(PemTrustAnchor {
                purpose: TrustPurpose::Iaca,
                certificate_pem: data,
            });
        }

        let trust_anchors = TrustAnchorRegistry::from_pem_certificates(certs)
            .map_err(|err| eyre::eyre!(Box::new(err)))?;

        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let central = adapters
            .into_iter()
            .next()
            .ok_or_eyre("no bluetooth adapters were found")?;

        let portraits: Portraits = Default::default();

        let router = Router::<()>::new()
            .route("/mdl/{hash}", get(serve_portrait))
            .layer(Extension(portraits.clone()));

        Ok((
            Self {
                timeout: config.timeout,
                central,
                trust_anchors,
                requested_elements,
                portraits,
            },
            router,
        ))
    }

    fn convert_elements(attributes: Vec<String>) -> eyre::Result<DataElements> {
        let mut attributes = attributes.into_iter();

        let Some(first_element) = attributes.next() else {
            eyre::bail!("must be at least one request attribute");
        };

        let mut elements = DataElements::new(first_element, false);
        for element in attributes {
            elements.insert(element, false);
        }

        Ok(elements)
    }

    pub async fn decode(&self, data: &str) -> eyre::Result<ResponseAuthenticationOutcome> {
        let resp = tokio::time::timeout(
            Duration::from_secs(self.timeout),
            self.retrieve(data.trim()),
        )
        .await??;

        Ok(resp)
    }

    #[instrument(skip_all)]
    async fn retrieve(&self, qr: &str) -> eyre::Result<ResponseAuthenticationOutcome> {
        let device_engagement_bytes = Tag24::<DeviceEngagement>::from_qr_code_uri(qr)
            .map_err(|err| eyre::eyre!(Box::new(err)))?;
        trace!("{device_engagement_bytes:?}");

        let Some(server_mode) = device_engagement_bytes
            .into_inner()
            .device_retrieval_methods
            .and_then(|methods| {
                methods.iter().find_map(|method| match method {
                    isomdl::definitions::DeviceRetrievalMethod::BLE(BleOptions {
                        peripheral_server_mode: Some(server_mode),
                        ..
                    }) => Some(server_mode.clone()),
                    _ => None,
                })
            })
        else {
            eyre::bail!("could not find BLE peripheral server mode");
        };

        let (mut reader_sm, session_request, _ble_ident) =
            reader::SessionManager::establish_session(
                qr.to_string(),
                self.requested_elements.clone(),
                self.trust_anchors.clone(),
            )
            .map_err(|err| eyre::eyre!(Box::new(err)))?;
        trace!(uuid = %server_mode.uuid, "starting scan for service");

        let mut events = self.central.events().await?;

        self.central
            .start_scan(ScanFilter {
                services: vec![server_mode.uuid],
            })
            .await?;

        let peripheral_id = loop {
            match events.next().await {
                Some(CentralEvent::DeviceDiscovered(id)) => {
                    break id;
                }
                Some(event) => {
                    trace!("got other event: {event:?}");
                }
                None => {
                    eyre::bail!("end of events without discovering device");
                }
            }
        };

        self.central.stop_scan().await?;
        debug!(id = %peripheral_id, "got peripheral, stopped scan");

        let peripheral = self.central.peripheral(&peripheral_id).await?;
        peripheral.connect().await?;
        peripheral.discover_services().await?;
        trace!("discovered services");

        let characteristics: Vec<_> = peripheral
            .characteristics()
            .into_iter()
            .filter(|c| c.service_uuid == server_mode.uuid)
            .collect();

        let Some(state) = characteristics.iter().find(|c| c.uuid == STATE_ID) else {
            eyre::bail!("missing state characteristic");
        };

        let Some(client2server) = characteristics.iter().find(|c| c.uuid == CLIENT2SERVER_ID)
        else {
            eyre::bail!("missing c2s characteristic");
        };

        let Some(server2client) = characteristics.iter().find(|c| c.uuid == SERVER2CLIENT_ID)
        else {
            eyre::bail!("missing s2c characteristic");
        };

        peripheral.subscribe(state).await?;
        peripheral.subscribe(server2client).await?;
        trace!("subscribed to characteristics");

        peripheral
            .write(state, &[0x01], btleplug::api::WriteType::WithoutResponse)
            .await?;
        trace!("wrote start");

        // TODO: figure out the real MTU, but for now our requests are small
        // enough this isn't a huge performance issue.
        let mtu = 20;

        // We can fit up to the usable MTU minus one byte, because we need that
        // byte to indicate if more messages are needed to fit all of the data.
        let mut it = session_request.chunks(mtu - 1).peekable();
        // Buffer for holding the message data.
        let mut buf = Vec::with_capacity(mtu);

        while let Some(chunk) = it.next() {
            buf.clear();

            // Until data fits, prepend 1 to data to signal incomplete message.
            if it.peek().is_some() {
                trace!("writing incomplete packet");
                buf.push(1);
            } else {
                trace!("writing last packet");
                buf.push(0);
            }

            buf.extend_from_slice(chunk);

            peripheral
                .write(
                    client2server,
                    &buf,
                    btleplug::api::WriteType::WithoutResponse,
                )
                .await?;

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let mut notifs = peripheral.notifications().await?;
        trace!("starting peripheral notifications");

        let mut response: Vec<u8> = Vec::new();

        while let Some(data) = notifs.next().await {
            if response.len() + data.value.len() > MAX_PAYLOAD_SIZE {
                eyre::bail!("response payload is too large");
            }

            let Some(first) = data.value.first().copied() else {
                eyre::bail!("value notification did not have any bytes");
            };

            if data.value.len() > 1 {
                trace!(
                    first,
                    "extending response with {} bytes",
                    data.value.len() - 1
                );
                response.extend(&data.value[1..]);
            }

            if first == 0 {
                trace!("data transfer done!");
                break;
            }
        }

        peripheral
            .write(state, &[0x00], btleplug::api::WriteType::WithoutResponse)
            .await?;
        trace!("wrote end");

        peripheral.disconnect().await?;
        trace!("disconnected from peripheral");

        let mut validated = reader_sm.handle_response(&response);

        if let Some(serde_json::Value::Array(portrait)) = validated
            .response
            .get_mut("org.iso.18013.5.1")
            .and_then(|iso18013| iso18013.as_object_mut())
            .and_then(|iso18013| iso18013.remove("portrait"))
        {
            debug!("response had portrait, extracting");
            let data: Vec<u8> = portrait
                .into_iter()
                .filter_map(|value| value.as_i64())
                .filter_map(|value| u8::try_from(value).ok())
                .collect();
            let hash = hex::encode(Sha256::digest(&data));

            let im: Option<DynamicImage> = if infer::image::is_jpeg2000(&data) {
                trace!("got jpeg2000");
                Image::from_bytes(&data)
                    .tap_err(|err| warn!("could not decode jpeg2000: {err}"))
                    .ok()
                    .and_then(|im| (&im).try_into().ok())
            } else if infer::is_image(&data) {
                trace!("got other image format");
                image::load_from_memory(&data)
                    .tap_err(|err| warn!("could not decode image: {err}"))
                    .ok()
            } else {
                warn!("portrait data was not jpeg2000 or jpeg");
                None
            };

            if let Some(im) = im {
                debug!("adding portrait image: {hash}");
                let mut cursor = Cursor::new(Vec::new());
                im.write_to(&mut cursor, image::ImageFormat::Jpeg)?;
                validated.response.insert(
                    "net.syfaro.reg-interface".to_string(),
                    serde_json::json!({
                        "portrait": hash,
                    }),
                );
                self.portraits
                    .lock()
                    .await
                    .insert(hash, cursor.into_inner());
            }
        }
        debug!("{validated:?}");

        Ok(validated)
    }
}
