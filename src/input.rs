use std::{sync::Arc, time::Duration};

use eyre::Context;
use serde::Deserialize;
use tap::Tap;
use tokio::{io::AsyncReadExt, select, sync::mpsc, task::JoinHandle, time::interval};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, instrument, trace};

use crate::{RunningTasks, decoder};

const DEFAULT_TIMEOUT_MS: u64 = 50;

pub type InputSender = mpsc::Sender<(String, String)>;

#[derive(Clone, Debug, Deserialize)]
pub struct InputConfig {
    name: String,
    timeout_ms: Option<u64>,
    #[serde(flatten)]
    device: InputDeviceConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputDeviceConfig {
    Hid(InputDeviceHidConfig),
    Serial(InputDeviceSerialConfig),
}

#[derive(Clone, Debug, Deserialize)]
pub struct InputDeviceHidConfig {
    vendor_id: u16,
    product_id: u16,
    usage_id: u16,
    usage_page: u16,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InputDeviceSerialConfig {
    path: String,
    baud_rate: u32,
}

macro_rules! interval_input {
    ($token:expr, $tx:expr, $dur:expr, $input_id:expr, $buf:expr, $fut:expr, $handler:expr) => {
        let mut value = String::new();
        let mut interval = interval($dur);

        loop {
            select! {
                _ = interval.tick() => {
                    send_value!($token, $tx, $input_id, value);
                }
                _ = $tx.closed() => {
                    info!("sender closed, ending input task");
                    break;
                }
                _ = $token.cancelled() => {
                    debug!("token cancelled, ending input task");
                    break;
                }
                res = $fut => {
                    let data = res?;

                    let handler = $handler;
                    if handler(data, &mut value) {
                        send_value!($token, $tx, $input_id, value);
                    }

                    interval.reset();
                }
            }
        }
    };
}

macro_rules! send_value {
    ($token:expr, $tx:expr, $input_id:expr, $value:expr) => {
        if $value.is_empty() {
            continue;
        }

        let final_value = std::mem::take(&mut $value);
        let final_value = final_value.trim_end_matches(['\r', '\n']).to_string();

        if let Err(err) = $tx.send(($input_id, final_value)).await {
            $token.cancel();
            error!("could not send scanned value, stopping input: {err}");
            break;
        }
    };
}

pub async fn create_stream(
    config: super::Config,
    token: CancellationToken,
    tasks: &mut super::RunningTasks,
    decoders: Arc<decoder::DecoderManager>,
    qr_rx: mpsc::Receiver<String>,
) -> eyre::Result<mpsc::Receiver<(Option<String>, decoder::DecodedDataContext)>> {
    let (data_tx, data_rx) = mpsc::channel(1);

    if config
        .decoder
        .enabled_decoders
        .as_ref()
        .map(|decoders| decoders.contains(&decoder::DecoderType::Mdl))
        .unwrap_or_default()
    {
        let mut mdl_verifier =
            decoder::mdl::create_mdl_verifier(config.decoder.mdl.clone()).await?;

        mdl_verifier.add_qr_stream(ReceiverStream::new(qr_rx));

        let mdl_input_name = config
            .decoder
            .mdl
            .input_name
            .clone()
            .unwrap_or_else(|| "mdl".to_string());

        let mut rx = mdl_verifier.start(token.clone()).await?;
        let data_tx_clone = data_tx.clone();
        let mdl_task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    mdl_verifier::VerifierEvent::AuthenticationOutcome(outcome) => {
                        if let Err(err) = data_tx_clone
                            .send((
                                None,
                                decoder::DecodedDataContext {
                                    input_name: mdl_input_name.clone(),
                                    decoder_type: decoder::DecoderType::Mdl,
                                    data: decoder::DecodedData::Mdl(outcome),
                                },
                            ))
                            .await
                        {
                            error!("could not send verifier event: {err}");
                            break;
                        }
                    }
                    other => trace!("got other mdl verifier event: {other:?}"),
                }
            }

            Ok(())
        });
        tasks.push(("mdl".to_string(), mdl_task));
    }

    let mut input_rx = create_inputs(config.inputs, tasks, token.clone()).await?;

    let decode_task = tokio::spawn(async move {
        while let Some((input_name, input_data)) = input_rx.recv().await {
            let data = match decoders.decode(&input_data).await? {
                Some(data) => data,
                None => continue,
            };

            data_tx
                .send((
                    Some(input_data),
                    decoder::DecodedDataContext {
                        input_name,
                        decoder_type: data.decoder_type(),
                        data,
                    },
                ))
                .await?;
        }

        Ok::<_, eyre::Report>(())
    });
    tasks.push(("decode-input".to_string(), decode_task));

    Ok(data_rx)
}

#[instrument(skip_all)]
async fn create_inputs(
    configs: Vec<InputConfig>,
    tasks: &mut RunningTasks,
    token: CancellationToken,
) -> eyre::Result<mpsc::Receiver<(String, String)>> {
    let (tx, rx) = mpsc::channel(1);

    for config in configs {
        debug!("configuring input {}", config.name);
        tasks.push((
            config.name.clone(),
            spawn_input(config, token.clone(), tx.clone()).await?,
        ));
    }

    Ok(rx)
}

#[instrument(skip_all, fields(name = config.name))]
async fn spawn_input(
    config: InputConfig,
    token: CancellationToken,
    tx: InputSender,
) -> eyre::Result<JoinHandle<eyre::Result<()>>> {
    let dur = Duration::from_millis(config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

    let task = match config.device {
        InputDeviceConfig::Serial(device) => {
            debug!("creating serial device");
            spawn_serial_device(dur, config.name, device, token, tx).await?
        }
        InputDeviceConfig::Hid(device) => {
            debug!("creating hid device");
            spawn_hid_device(dur, config.name, device, token, tx).await?
        }
    };

    Ok(task)
}

async fn spawn_serial_device(
    dur: Duration,
    name: String,
    device: InputDeviceSerialConfig,
    token: CancellationToken,
    tx: InputSender,
) -> eyre::Result<JoinHandle<eyre::Result<()>>> {
    debug!(path = device.path, "looking for serial port");
    let port = tokio_serial::new(&device.path, device.baud_rate)
        .open_native_async()
        .wrap_err_with(|| {
            let available_ports = tokio_serial::available_ports()
                .unwrap_or_default()
                .into_iter()
                .map(|port| port.port_name)
                .collect::<Vec<_>>()
                .join(", ");

            format!(
                "failed to open serial port {}, available ports are {available_ports}",
                device.path
            )
        })?;
    debug!("opened serial port");

    Ok(tokio::spawn(
        serial_device_loop(token, tx, dur, name, port).in_current_span(),
    ))
}

async fn serial_device_loop(
    token: CancellationToken,
    tx: InputSender,
    dur: Duration,
    name: String,
    mut port: SerialStream,
) -> eyre::Result<()> {
    let mut buf = [0u8; 4096];

    interval_input!(
        token,
        tx,
        dur,
        name.clone(),
        buf,
        port.read(&mut buf),
        |size, value: &mut String| {
            trace!(size, "got serial input");

            if size == 0 {
                return false;
            }

            let s = String::from_utf8_lossy(&buf[0..size]);
            value.push_str(&s);

            false
        }
    );

    Ok(())
}

async fn spawn_hid_device(
    dur: Duration,
    name: String,
    device: InputDeviceHidConfig,
    token: CancellationToken,
    tx: InputSender,
) -> eyre::Result<JoinHandle<eyre::Result<()>>> {
    debug!("looking for hid device");
    let devices: Vec<_> =
        futures::StreamExt::collect(async_hid::DeviceInfo::enumerate().await?).await;
    let device = devices
        .iter()
        .find(|found_device| {
            found_device.matches(
                device.usage_page,
                device.usage_id,
                device.vendor_id,
                device.product_id,
            )
        })
        .ok_or_else(|| {
            let available_devices = devices
                .iter()
                .map(|device| {
                    format!(
                        "{}:{} ({}:{})",
                        device.vendor_id, device.product_id, device.usage_id, device.usage_page,
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            eyre::eyre!(
                "could not find hid device {device:?}, available devices: {available_devices}",
            )
        })?
        .tap(|device| debug!(name = device.name, "found device"))
        .open(async_hid::AccessMode::Read)
        .await?;
    debug!("opened hid device");

    let span = tracing::Span::current();

    let task = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();

        rt.block_on(
            async {
                let local = tokio::task::LocalSet::new();

                let fut = hid_device_loop(token.clone(), tx, dur, name, device);
                local.run_until(fut).await?;

                Ok(())
            }
            .instrument(span),
        )
    });

    Ok(task)
}

async fn hid_device_loop(
    token: CancellationToken,
    tx: InputSender,
    dur: Duration,
    name: String,
    device: async_hid::Device,
) -> eyre::Result<()> {
    let mut buf = [0u8; 64];

    interval_input!(
        token,
        tx,
        dur,
        name.clone(),
        buf,
        device.read_input_report(&mut buf),
        |_, value: &mut String| {
            let data_len = buf[0] as usize;

            trace!(
                data_len,
                buf = hex::encode(&buf[0..data_len + 1]),
                "got hid report"
            );

            let useful_bytes = if value.is_empty() {
                &buf[3..=data_len]
            } else {
                &buf[1..=data_len]
            };

            let s = String::from_utf8_lossy(useful_bytes);
            value.push_str(&s);

            data_len != 63
        }
    );

    Ok(())
}
