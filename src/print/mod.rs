use eyre::{bail, Context};
use futures::TryStreamExt;
use ipp::prelude::*;
use serde::{Deserialize, Serialize};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{debug, info, instrument};

#[derive(Debug, Serialize, Deserialize)]
pub struct PrintConfig {
    #[serde(default = "PrintConfig::default_cups_host")]
    cups_host: String,
    printer_uri: String,
    #[serde(default)]
    attributes: Vec<IppAttribute>,
}

impl PrintConfig {
    fn default_cups_host() -> String {
        "http://localhost:631".to_string()
    }
}

pub struct Printer {
    ipp_client: AsyncIppClient,
    ipp_attributes: Vec<IppAttribute>,

    http_client: reqwest::Client,
    printer_uri: Uri,
}

impl Printer {
    #[instrument(skip(config))]
    pub async fn new(config: PrintConfig) -> eyre::Result<Self> {
        let cups_uri = config
            .cups_host
            .parse()
            .wrap_err_with(|| format!("invalid cups uri: {}", config.cups_host))?;
        let printer_uri = config
            .printer_uri
            .parse()
            .wrap_err_with(|| format!("invalid printer uri: {}", config.printer_uri))?;

        let ipp_client = AsyncIppClient::builder(cups_uri).build();
        let http_client = reqwest::Client::default();

        let printer = Self {
            ipp_client,
            ipp_attributes: config.attributes,
            http_client,
            printer_uri,
        };

        if !printer
            .validate_printer()
            .await
            .wrap_err_with(|| "could not list printers")?
        {
            bail!("could not find printer: {}", printer.printer_uri);
        }

        Ok(printer)
    }

    #[instrument(skip(self))]
    pub async fn print_url(&self, url: &str) -> eyre::Result<()> {
        let resp = self.http_client.get(url).send().await?;
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        let reader = tokio_util::io::StreamReader::new(stream);

        let payload = IppPayload::new_async(reader.compat());
        let op = IppOperationBuilder::print_job(self.printer_uri.clone(), payload)
            .attributes(self.ipp_attributes.iter().cloned())
            .build();
        let resp = self.ipp_client.send(op).await?;

        info!(status_code = %resp.header().status_code(), "sent print job");

        Ok(())
    }

    #[instrument(skip(self))]
    async fn validate_printer(&self) -> eyre::Result<bool> {
        let op = IppOperationBuilder::cups().get_printers();
        let resp = self.ipp_client.send(op).await?;
        debug!(
            status = %resp.header().status_code(),
            "got response status"
        );

        let groups = resp.attributes().groups_of(DelimiterTag::PrinterAttributes);

        let printer_uri_str = self.printer_uri.to_string();
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
