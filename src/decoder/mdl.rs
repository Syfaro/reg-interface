use std::{collections::HashMap, ffi::OsStr, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use eyre::OptionExt;
use isomdl::{
    definitions::{
        BleOptions, DeviceEngagement,
        helpers::{ByteStr, NonEmptyMap, Tag24},
        session::Handover,
        x509::trust_anchor::{PemTrustAnchor, TrustAnchorRegistry},
    },
    presentation::{authentication::ResponseAuthenticationOutcome, reader::SessionManager},
};
use postcard::accumulator::{CobsAccumulator, FeedResult};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_serial::{SerialPort, SerialPortBuilderExt, SerialStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use crate::decoder::{Decoder, DecoderOutcome, DecoderType};

#[derive(Clone, Default, Debug, Deserialize)]
pub struct MdlConfig {
    pub input_name: Option<String>,
    pub path: String,
    pub baud: u32,
    #[serde(default)]
    pub extract_portraits: bool,
    pub request_elements: Option<HashMap<String, HashMap<String, bool>>>,
    pub certificates_path: Option<PathBuf>,
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

pub struct EspMdl {
    requested_elements: NonEmptyMap<String, NonEmptyMap<String, bool>>,
    trust_anchor_registry: TrustAnchorRegistry,

    token: CancellationToken,
    serial: Mutex<SerialStream>,

    active_session: Mutex<Option<([u8; 16], SessionManager)>>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub enum HostRequest {
    Ping,
    Ack,
    StartBluetoothPeripheral {
        service_uuid: [u8; 16],
        ble_ident: [u8; 16],
        payload: Vec<u8>,
    },
    StartBluetoothCentral {
        service_uuid: [u8; 16],
        payload: Vec<u8>,
    },
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub enum DeviceResponse {
    Ack,
    NfcEngagementStarted {
        uid: [u8; 4],
    },
    NfcEngagementFailed {
        uid: [u8; 4],
    },
    NfcEngagementData {
        uid: [u8; 4],
        use_central: bool,
        service_uuid: [u8; 16],
        device_engagement: Vec<u8>,
        handover_select: Vec<u8>,
        handover_request: Option<Vec<u8>>,
    },
    BleEngagementFailed {
        service_uuid: [u8; 16],
    },
    BleEngagementData {
        service_uuid: [u8; 16],
        payload: Vec<u8>,
        complete: bool,
    },
}

impl EspMdl {
    pub async fn new(config: MdlConfig, token: CancellationToken) -> eyre::Result<Self> {
        let certs = load_certs(config.certificates_path).await?;
        let trust_anchor_registry = TrustAnchorRegistry::from_pem_certificates(certs)
            .map_err(|err| eyre::eyre!("could not build trust registry: {err}"))?;

        let requested_elements = config
            .request_elements
            .unwrap_or_else(MdlConfig::default_elements);
        let requested_elements = non_empty_requested_elements(requested_elements)
            .ok_or_eyre("unable to use provided requested elements")?;

        let serial = tokio_serial::new(config.path, config.baud).open_native_async()?;
        serial.clear(tokio_serial::ClearBuffer::All)?;

        Ok(Self {
            requested_elements,
            trust_anchor_registry,
            token,
            serial: Mutex::new(serial),
            active_session: Mutex::new(None),
        })
    }

    pub fn create_input_task(
        self: Arc<Self>,
        tx: mpsc::Sender<ResponseAuthenticationOutcome>,
    ) -> JoinHandle<eyre::Result<()>> {
        tokio::task::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut cobs_buf: CobsAccumulator<4096> = CobsAccumulator::new();

            let mut current_service_uuid: Option<[u8; 16]> = None;
            let mut payload_chunks = Vec::new();

            loop {
                if self.token.is_cancelled() {
                    return Ok(());
                }

                let mut serial = self.serial.lock().await;

                while let Ok(Some(Ok(len))) = tokio::time::timeout(
                    Duration::from_millis(25),
                    self.token.run_until_cancelled(serial.read(&mut buf)),
                )
                .await
                {
                    if len == 0 {
                        break;
                    }

                    let data = &buf[..len];
                    trace!("got new data: {}", hex::encode(data));
                    let mut window = data;

                    'cobs: while !window.is_empty() {
                        window = match cobs_buf.feed::<DeviceResponse>(window) {
                            FeedResult::Consumed => break 'cobs,
                            FeedResult::OverFull(new_window) => {
                                warn!("overfull");
                                new_window
                            }
                            FeedResult::DeserError(new_window) => {
                                warn!("deserialization issue");
                                new_window
                            }
                            FeedResult::Success { data, remaining } => {
                                trace!("got device response: {data:?}");

                                match data {
                                    DeviceResponse::Ack => {
                                        warn!("got unexpected ack");
                                    }
                                    DeviceResponse::NfcEngagementStarted { uid } => {
                                        debug!("starting engagement with tag {}", hex::encode(uid));
                                        self.send_request(HostRequest::Ack, &mut serial).await?;
                                    }
                                    DeviceResponse::NfcEngagementFailed { uid } => {
                                        debug!("engagement with tag failed {}", hex::encode(uid));
                                        self.send_request(HostRequest::Ack, &mut serial).await?;
                                    }
                                    DeviceResponse::NfcEngagementData {
                                        use_central,
                                        service_uuid,
                                        device_engagement,
                                        handover_select,
                                        handover_request,
                                        ..
                                    } => {
                                        let Ok(device_engagement) =
                                            Tag24::<DeviceEngagement>::from_bytes(
                                                device_engagement,
                                            )
                                        else {
                                            warn!("data was not device engagement");
                                            window = remaining;
                                            continue;
                                        };

                                        self.send_request(HostRequest::Ack, &mut serial).await?;

                                        self.start_bluetooth_locked(
                                            &mut serial,
                                            device_engagement,
                                            Handover::NFC(
                                                ByteStr::from(handover_select),
                                                handover_request.map(ByteStr::from),
                                            ),
                                            service_uuid,
                                            !use_central,
                                        )
                                        .await?;
                                    }
                                    DeviceResponse::BleEngagementFailed { mut service_uuid } => {
                                        service_uuid.reverse();
                                        warn!(
                                            "engagement with service failed {}",
                                            uuid::Uuid::from_bytes(service_uuid)
                                        );
                                        self.send_request(HostRequest::Ack, &mut serial).await?;
                                    }
                                    DeviceResponse::BleEngagementData {
                                        service_uuid,
                                        payload,
                                        complete,
                                    } => {
                                        if current_service_uuid.is_some()
                                            && current_service_uuid != Some(service_uuid)
                                        {
                                            warn!("service uuid changed!");
                                            payload_chunks.clear();
                                        }

                                        current_service_uuid = Some(service_uuid);
                                        payload_chunks.extend_from_slice(&payload);

                                        self.send_request(HostRequest::Ack, &mut serial).await?;

                                        if complete {
                                            if let Some((session_service_uuid, mut reader_sm)) =
                                                self.active_session.lock().await.take()
                                            {
                                                if service_uuid != session_service_uuid {
                                                    warn!("service uuid changed from session!");
                                                } else {
                                                    let outcome =
                                                        reader_sm.handle_response(&payload_chunks);
                                                    tx.send(outcome).await?;
                                                }
                                            }

                                            current_service_uuid = None;
                                            payload_chunks.clear();
                                        }
                                    }
                                }
                                remaining
                            }
                        }
                    }
                }
            }
        })
    }

    fn get_connection(
        device_engagement_bytes: &Tag24<DeviceEngagement>,
    ) -> Option<(bool, [u8; 16])> {
        device_engagement_bytes
            .clone()
            .into_inner()
            .device_retrieval_methods
            .into_iter()
            .flat_map(|methods| methods.into_inner())
            .filter_map(|method| match method {
                isomdl::definitions::DeviceRetrievalMethod::BLE(ble) => Some(ble),
                _ => None,
            })
            .find_map(|ble| match ble {
                BleOptions {
                    peripheral_server_mode: Some(peripheral_server_mode),
                    ..
                } => Some((true, peripheral_server_mode.uuid.into_bytes())),
                BleOptions {
                    central_client_mode: Some(central_client_mode),
                    ..
                } => Some((false, central_client_mode.uuid.into_bytes())),
                _ => None,
            })
    }

    pub async fn start_bluetooth(
        &self,
        device_engagement_bytes: Tag24<DeviceEngagement>,
        handover: Handover,
        service_uuid: [u8; 16],
        server: bool,
    ) -> eyre::Result<()> {
        let mut serial = self.serial.lock().await;
        self.start_bluetooth_locked(
            &mut serial,
            device_engagement_bytes,
            handover,
            service_uuid,
            server,
        )
        .await
    }

    async fn start_bluetooth_locked(
        &self,
        serial: &mut SerialStream,
        device_engagement_bytes: Tag24<DeviceEngagement>,
        handover: Handover,
        mut service_uuid: [u8; 16],
        server: bool,
    ) -> eyre::Result<()> {
        service_uuid.reverse();

        let (reader_sm, session_request, ble_ident) =
            SessionManager::establish_session_with_handover(
                device_engagement_bytes,
                self.requested_elements.clone(),
                self.trust_anchor_registry.clone(),
                handover,
            )
            .map_err(|err| eyre::eyre!("failed to establish session: {err}"))?;

        let request = if server {
            HostRequest::StartBluetoothCentral {
                service_uuid,
                payload: session_request,
            }
        } else {
            HostRequest::StartBluetoothPeripheral {
                service_uuid,
                ble_ident,
                payload: session_request,
            }
        };

        *self.active_session.lock().await = Some((service_uuid, reader_sm));
        self.send_request(request, serial).await?;

        Ok(())
    }

    async fn send_request(
        &self,
        request: HostRequest,
        serial: &mut SerialStream,
    ) -> eyre::Result<()> {
        let data = postcard::to_allocvec_cobs(&request)?;
        trace!("sending request to esp: {}", hex::encode(&data));

        for chunk in data.chunks(128) {
            trace!("sending chunk: {}", hex::encode(chunk));
            serial.write_all(chunk).await?;
        }
        serial.flush().await?;
        trace!("finished sending data to esp");

        Ok(())
    }
}

async fn load_certs(path: Option<PathBuf>) -> eyre::Result<Vec<PemTrustAnchor>> {
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
        certs.push(PemTrustAnchor {
            purpose: isomdl::definitions::x509::trust_anchor::TrustPurpose::Iaca,
            certificate_pem: data,
        });
    }

    Ok(certs)
}

fn non_empty_requested_elements(
    requested_elements: HashMap<String, HashMap<String, bool>>,
) -> Option<NonEmptyMap<String, NonEmptyMap<String, bool>>> {
    NonEmptyMap::maybe_new(
        requested_elements
            .into_iter()
            .filter_map(|(namespace, elements)| {
                NonEmptyMap::maybe_new(elements.into_iter().collect()).map(|map| (namespace, map))
            })
            .collect(),
    )
}

pub struct MdlDecoder {
    esp_mdl: Arc<EspMdl>,
}

impl MdlDecoder {
    pub async fn new(esp_mdl: Arc<EspMdl>) -> Self {
        Self { esp_mdl }
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

        trace!("got mdoc data: {data}");

        let Ok(device_engagement_bytes) = Tag24::<DeviceEngagement>::from_qr_code_uri(data) else {
            warn!("looked like mdoc url but could not be decoded");
            return Ok(DecoderOutcome::Skipped);
        };

        let Some((server, service_uuid)) = EspMdl::get_connection(&device_engagement_bytes) else {
            warn!("no supported connection methods");
            return Ok(DecoderOutcome::Skipped);
        };

        self.esp_mdl
            .start_bluetooth(device_engagement_bytes, Handover::QR, service_uuid, server)
            .await?;

        Ok(DecoderOutcome::Consumed)
    }
}
