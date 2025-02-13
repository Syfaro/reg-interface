use std::{collections::HashSet, sync::Arc, time::Duration};

use eyre::{eyre, Context};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tap::Tap;
use tokio::{io::AsyncReadExt, select, sync::mpsc::Sender, time::interval};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, trace, Instrument};

use super::decoder::DecoderType;

const DEFAULT_TIMEOUT_MS: u64 = 50;

#[derive(Debug, Deserialize, Serialize)]
pub struct ScannerInputConfig {
    timeout_ms: Option<u64>,
    pub decoders: HashSet<DecoderType>,
    pub transformer: Option<String>,
    pub connection_targets: Option<Arc<HashSet<String>>>,
    #[serde(default)]
    pub open_urls: bool,
    #[serde(flatten)]
    device: ScannerInputDeviceConfig,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ScannerInputDeviceConfig {
    Hid(HidDevice),
    Serial(SerialDevice),
}

impl ScannerInputDeviceConfig {
    fn display_name(&self) -> String {
        match self {
            Self::Hid(hid) => format!("hid ({})", hid.vendor_product_display()),
            Self::Serial(serial) => format!("serial ({})", serial.path),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HidDevice {
    vendor_id: u16,
    product_id: u16,
    usage_id: u16,
    usage_page: u16,
}

impl HidDevice {
    fn vendor_product_display(&self) -> String {
        format!("{}:{}", self.vendor_id, self.product_id)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SerialDevice {
    path: String,
    baud_rate: u32,
}

type BarcodeSender = Sender<(usize, String)>;

macro_rules! send_value {
    ($token:expr, $tx:expr, $input_id:expr, $value:expr) => {
        if $value.is_empty() {
            continue;
        }

        let final_value = std::mem::take(&mut $value);
        let final_value = final_value
            .strip_suffix("\n")
            .unwrap_or(&final_value)
            .strip_suffix("\r")
            .unwrap_or(&final_value)
            .to_string();

        if let Err(err) = $tx.send(($input_id, final_value)).await {
            $token.cancel();
            error!("could not send scanned value, stopping input: {err}");
            break;
        }
    };
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
                    info!("token cancelled, ending input task");
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

pub async fn connect_scanner_inputs(
    inputs: Vec<ScannerInputConfig>,
    token: CancellationToken,
    tx: BarcodeSender,
) -> eyre::Result<()> {
    for (input_id, input) in inputs.into_iter().enumerate() {
        start_input(input_id, input, token.child_token(), tx.clone()).await?;
    }

    Ok(())
}

#[instrument(skip_all, fields(device_name = input.device.display_name()))]
async fn start_input(
    input_id: usize,
    input: ScannerInputConfig,
    token: CancellationToken,
    tx: BarcodeSender,
) -> eyre::Result<()> {
    let dur = Duration::from_millis(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

    match input.device {
        ScannerInputDeviceConfig::Serial(device) => {
            debug!("creating serial device");
            start_serial_device(dur, input_id, device, token, tx).await?
        }
        ScannerInputDeviceConfig::Hid(device) => {
            debug!("creating hid device");
            start_hid_device(dur, input_id, device, token, tx).await?
        }
    }

    Ok(())
}

async fn start_serial_device(
    dur: Duration,
    input_id: usize,
    device: SerialDevice,
    token: CancellationToken,
    tx: BarcodeSender,
) -> eyre::Result<()> {
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

    tokio::spawn(
        async move {
            if let Err(err) = serial_device_loop(token.clone(), tx, dur, input_id, port).await {
                error!("serial device loop had error: {err}");
                token.cancel();
            }
        }
        .in_current_span(),
    );

    Ok(())
}

async fn serial_device_loop(
    token: CancellationToken,
    tx: BarcodeSender,
    dur: Duration,
    input_id: usize,
    mut port: SerialStream,
) -> eyre::Result<()> {
    let mut buf = [0u8; 4096];

    interval_input!(
        token,
        tx,
        dur,
        input_id,
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

async fn start_hid_device(
    dur: Duration,
    input_id: usize,
    device: HidDevice,
    token: CancellationToken,
    tx: BarcodeSender,
) -> eyre::Result<()> {
    debug!("looking for hid device");
    let devices: Vec<_> = async_hid::DeviceInfo::enumerate().await?.collect().await;
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

            eyre!(
                "could not find hid device {}, available devices: {available_devices}",
                device.vendor_product_display()
            )
        })?
        .tap(|device| debug!(name = device.name, "found device"))
        .open(async_hid::AccessMode::Read)
        .await?;
    debug!("opened hid device");

    let span = tracing::Span::current();

    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();

        rt.block_on(
            async {
                let local = tokio::task::LocalSet::new();

                let fut = hid_device_loop(token.clone(), tx, dur, input_id, device);

                if let Err(err) = local.run_until(fut).await {
                    error!("serial device loop had error: {err}");
                    token.cancel();
                }
            }
            .instrument(span),
        )
    });

    Ok(())
}

async fn hid_device_loop(
    token: CancellationToken,
    tx: BarcodeSender,
    dur: Duration,
    input_id: usize,
    device: async_hid::Device,
) -> eyre::Result<()> {
    let mut buf = [0u8; 64];

    interval_input!(
        token,
        tx,
        dur,
        input_id,
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
