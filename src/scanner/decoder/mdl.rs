use std::{collections::HashMap, ffi::OsStr, path::PathBuf, time::Duration};

use btleplug::{
    api::{Central as _, CentralEvent, Manager as _, Peripheral as _, ScanFilter},
    platform::{Adapter, Manager, PeripheralId},
};
use futures::StreamExt;
use hex_literal::hex;
use isomdl::{
    definitions::{
        device_request::{self, DataElements, Namespaces},
        helpers::Tag24,
        x509::trust_anchor::{PemTrustAnchor, TrustAnchorRegistry, TrustPurpose},
        DeviceEngagement,
    },
    presentation::{
        authentication::ResponseAuthenticationOutcome,
        reader::{self},
    },
};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, sync::oneshot};
use tracing::{debug, instrument, trace};
use uuid::Uuid;

static STATE_ID: Uuid = Uuid::from_bytes(hex!("00000001 A123 48CE 896B 4C76973373E6"));
static C2S_ID: Uuid = Uuid::from_bytes(hex!("00000002 A123 48CE 896B 4C76973373E6"));
static S2C_ID: Uuid = Uuid::from_bytes(hex!("00000003 A123 48CE 896B 4C76973373E6"));

#[derive(Debug, Deserialize, Serialize)]
pub struct MdlConfig {
    request_attributes: HashMap<String, Vec<String>>,
    certificates: PathBuf,
}

impl Default for MdlConfig {
    fn default() -> Self {
        Self {
            request_attributes: vec![(
                "org.iso.18013.5.1".into(),
                vec![
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

pub struct MdlDecoder {
    central: Adapter,
    trust_anchors: TrustAnchorRegistry,
    requested_elements: device_request::Namespaces,
}

impl MdlDecoder {
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

    pub async fn new(config: MdlConfig) -> eyre::Result<Self> {
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
            .expect("no bluetooth adapters found");

        Ok(Self {
            central,
            trust_anchors,
            requested_elements,
        })
    }

    pub async fn decode(&self, data: &str) -> eyre::Result<ResponseAuthenticationOutcome> {
        let qr = data.trim().to_string();

        let device_engagement_bytes = Tag24::<DeviceEngagement>::from_qr_code_uri(&qr)
            .map_err(|err| eyre::eyre!(Box::new(err)))?;
        trace!("{device_engagement_bytes:?}");

        let Some(device_retrieval_methods) = device_engagement_bytes
            .into_inner()
            .device_retrieval_methods
        else {
            eyre::bail!("missing device_retrieval_methods");
        };

        let Some(ble_options) = device_retrieval_methods
            .iter()
            .find_map(|method| match method {
                isomdl::definitions::DeviceRetrievalMethod::BLE(opts) => Some(opts),
                _ => None,
            })
        else {
            eyre::bail!("missing ble options");
        };

        let Some(server_mode) = &ble_options.peripheral_server_mode else {
            eyre::bail!("only support server mode");
        };

        let resp = tokio::time::timeout(
            Duration::from_secs(30),
            self.fetch_response(qr, server_mode.uuid),
        )
        .await??;

        Ok(resp)
    }

    async fn fetch_response(
        &self,
        qr: String,
        server_uuid: Uuid,
    ) -> eyre::Result<ResponseAuthenticationOutcome> {
        let (mut reader_sm, session_request, _ble_ident) =
            reader::SessionManager::establish_session(
                qr,
                self.requested_elements.clone(),
                self.trust_anchors.clone(),
            )
            .map_err(|err| eyre::eyre!(Box::new(err)))?;

        debug!("looking for service {}", server_uuid);

        let (tx, rx) = oneshot::channel();
        events_debug(&self.central, tx).await?;

        debug!("starting scan");

        self.central
            .start_scan(ScanFilter {
                services: vec![server_uuid],
            })
            .await?;

        let peripheral_id = rx.await?;
        self.central.stop_scan().await?;

        debug!("got peripheral, stopped scan");

        let peripheral = self.central.peripheral(&peripheral_id).await?;
        peripheral.connect().await?;
        peripheral.discover_services().await?;

        debug!("discovered services");

        let characteristics: Vec<_> = peripheral
            .characteristics()
            .into_iter()
            .filter(|c| c.service_uuid == server_uuid)
            .collect();

        let Some(state_c) = characteristics.iter().find(|c| c.uuid == STATE_ID) else {
            eyre::bail!("missing state characteristic");
        };

        let Some(c2s_c) = characteristics.iter().find(|c| c.uuid == C2S_ID) else {
            eyre::bail!("missing c2s characteristic");
        };

        let Some(s2c_c) = characteristics.iter().find(|c| c.uuid == S2C_ID) else {
            eyre::bail!("missing s2c characteristic");
        };

        debug!("subscribing to characteristics");

        peripheral.subscribe(state_c).await?;
        peripheral.subscribe(s2c_c).await?;

        debug!("writing characteristic start");

        peripheral
            .write(state_c, &[0x01], btleplug::api::WriteType::WithoutResponse)
            .await?;

        let mtu = 20;

        let mut buf = Vec::with_capacity(mtu);
        let mut it = session_request.chunks(mtu - 1).peekable();
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
                .write(c2s_c, &buf, btleplug::api::WriteType::WithoutResponse)
                .await?;

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let mut notifs = peripheral.notifications().await?;

        let mut response: Vec<u8> = Vec::new();
        while let Some(data) = notifs.next().await {
            let Some(first) = data.value.first().copied() else {
                eyre::bail!("missing data!");
            };

            trace!("extending response with {} bytes", data.value.len() - 1);
            response.extend(&data.value[1..]);

            if first == 0 {
                debug!("data transfer done!");
                break;
            }
        }

        let validated = reader_sm.handle_response(&response);

        debug!("{validated:#?}");

        peripheral.disconnect().await?;

        Ok(validated)
    }
}

#[instrument(skip_all)]
async fn events_debug(central: &Adapter, tx: oneshot::Sender<PeripheralId>) -> eyre::Result<()> {
    let mut events = central.events().await?;

    let mut tx = Some(tx);

    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            match event {
                CentralEvent::DeviceDiscovered(id) => {
                    debug!(?id, "discovered device");

                    if let Some(tx) = tx.take() {
                        tx.send(id).expect("could not send peripheral id");
                    }

                    break;
                }
                event => {
                    debug!("got other event: {event:?}");
                }
            }
        }
    });

    Ok(())
}
