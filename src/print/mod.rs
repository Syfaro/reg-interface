use eyre::{bail, Context};
use futures::TryStreamExt;
use ipp::prelude::*;
use serde::{Deserialize, Serialize};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{debug, info, instrument};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "printer_type", rename_all = "lowercase")]
pub enum PrintConfig {
    Cups(CupsPrintConfig),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CupsPrintConfig {
    #[serde(default = "PrintConfig::default_cups_host")]
    cups_host: String,
    printer_uri: String,
}

impl PrintConfig {
    fn default_cups_host() -> String {
        "http://localhost:631".to_string()
    }
}

pub struct Printer {
    http_client: reqwest::Client,
    connection: PrinterConnection,
}

enum PrinterConnection {
    Cups {
        ipp_client: AsyncIppClient,
        printer_uri: Uri,
    },
}

impl std::fmt::Debug for PrinterConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cups { printer_uri, .. } => f
                .debug_struct("PrinterConnection::Cups")
                .field("printer_uri", printer_uri)
                .finish_non_exhaustive(),
        }
    }
}

impl Printer {
    #[instrument(skip(config))]
    pub async fn new(config: PrintConfig) -> eyre::Result<Self> {
        match config {
            PrintConfig::Cups(cups) => Self::connect_cups(cups).await,
        }
    }

    async fn connect_cups(cups: CupsPrintConfig) -> eyre::Result<Self> {
        let cups_uri = cups
            .cups_host
            .parse()
            .wrap_err_with(|| format!("invalid cups uri: {}", cups.cups_host))?;
        let printer_uri = cups
            .printer_uri
            .parse()
            .wrap_err_with(|| format!("invalid printer uri: {}", cups.printer_uri))?;

        let ipp_client = AsyncIppClient::builder(cups_uri).build();
        let http_client = reqwest::Client::default();

        let printer = Self {
            http_client,
            connection: PrinterConnection::Cups {
                ipp_client,
                printer_uri,
            },
        };

        if !printer
            .validate_printer()
            .await
            .wrap_err_with(|| "could not list printers")?
        {
            bail!("could not find printer: {:?}", printer.connection);
        }

        Ok(printer)
    }

    #[instrument(skip(self))]
    pub async fn print_url(&self, url: &str) -> eyre::Result<()> {
        let resp = self.http_client.get(url).send().await?;
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        let reader = tokio_util::io::StreamReader::new(stream);

        match &self.connection {
            PrinterConnection::Cups {
                ipp_client,
                printer_uri,
            } => {
                let payload = IppPayload::new_async(reader.compat());
                let op = IppOperationBuilder::print_job(printer_uri.clone(), payload).build();
                let resp = ipp_client.send(op).await?;

                info!(status_code = %resp.header().status_code(), "sent print job");
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    async fn validate_printer(&self) -> eyre::Result<bool> {
        match &self.connection {
            PrinterConnection::Cups {
                ipp_client,
                printer_uri,
            } => {
                let op = IppOperationBuilder::cups().get_printers();
                let resp = ipp_client.send(op).await?;
                debug!(
                    status = %resp.header().status_code(),
                    "got response status"
                );

                let groups = resp.attributes().groups_of(DelimiterTag::PrinterAttributes);

                let printer_uri_str = printer_uri.to_string();
                let is_known_printer = groups.into_iter().any(|group| {
                    let attributes = group.attributes();

                    ["device-uri", "printer-uri-supported"]
                        .into_iter()
                        .any(|attribute_name| {
                            attributes[attribute_name].value().to_string() == printer_uri_str
                        })
                });

                Ok(is_known_printer)
            }
        }
    }
}
