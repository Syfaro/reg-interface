use std::{net::SocketAddr, time::Duration};

use eyre::{bail, Context};
use futures::TryStreamExt;
use ipp::prelude::*;
use serde::{Deserialize, Serialize};
use tap::TapOptional;
use tokio::{io::AsyncWriteExt, net::TcpSocket};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{debug, info, instrument, trace};

mod zpl;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "printer_type", rename_all = "lowercase")]
pub enum PrintConfig {
    Cups(CupsPrintConfig),
    Zpl(ZplPrintConfig),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CupsPrintConfig {
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

#[derive(Debug, Serialize, Deserialize)]
pub struct ZplPrintConfig {
    printer_addr: SocketAddr,
    rotate: bool,
    label_width: i32,
    label_height: i32,
}

pub struct Printer {
    http_client: reqwest::Client,
    connection: PrinterConnection,
}

enum PrinterConnection {
    Cups {
        ipp_client: AsyncIppClient,
        ipp_attributes: Vec<IppAttribute>,

        printer_uri: Uri,
    },
    Zpl {
        config: ZplPrintConfig,
    },
}

impl std::fmt::Debug for PrinterConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cups { printer_uri, .. } => f
                .debug_struct("PrinterConnection::Cups")
                .field("printer_uri", printer_uri)
                .finish_non_exhaustive(),
            Self::Zpl { config } => f
                .debug_struct("PrinterConnection::Zpl")
                .field("printer_addr", &config.printer_addr)
                .field("label_width", &config.label_width)
                .field("label_height", &config.label_height)
                .finish(),
        }
    }
}

impl Printer {
    #[instrument(skip(config))]
    pub async fn new(config: PrintConfig) -> eyre::Result<Self> {
        match config {
            PrintConfig::Cups(cups) => Self::connect_cups(cups).await,
            PrintConfig::Zpl(zpl) => Ok(Self {
                http_client: Default::default(),
                connection: PrinterConnection::Zpl { config: zpl },
            }),
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
                ipp_attributes: cups.attributes,
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
    pub async fn print_url_and_data(&self, url: String, mut data: String) -> eyre::Result<()> {
        let resp = self.http_client.get(url).send().await?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().map(|s| s.to_string()).ok())
            .tap_some(|content_type| debug!(content_type, "got url content-type"));

        match &self.connection {
            PrinterConnection::Cups { .. } => eyre::bail!("cups printers cannot use zpl data"),
            PrinterConnection::Zpl { config } => {
                let Some(content_type) = content_type else {
                    eyre::bail!("url must include content-type header");
                };

                let Some(pos) = data.find("^XZ") else {
                    eyre::bail!("must have end of label to insert data");
                };

                if content_type.starts_with("image/") {
                    let im_data = resp.bytes().await?;
                    let im = image::load_from_memory(&im_data)?;
                    let field = zpl::image_to_gf(&im, config.rotate);

                    trace!(field, "generated GFA instruction");

                    data.insert_str(pos, &field);

                    Self::write_to_socket(config.printer_addr.to_owned(), data.as_bytes()).await?;
                }
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn print_url(&self, url: String) -> eyre::Result<()> {
        let resp = self.http_client.get(url).send().await?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().map(|s| s.to_string()).ok())
            .tap_some(|content_type| debug!(content_type, "got url content-type"));

        match &self.connection {
            PrinterConnection::Cups {
                ipp_client,
                ipp_attributes,
                printer_uri,
            } => {
                let stream = resp.bytes_stream().map_err(std::io::Error::other);
                let reader = tokio_util::io::StreamReader::new(stream);

                let payload = IppPayload::new_async(reader.compat());
                let op = IppOperationBuilder::print_job(printer_uri.clone(), payload)
                    .attributes(ipp_attributes.iter().cloned())
                    .build();
                let resp = ipp_client.send(op).await?;

                info!(status_code = %resp.header().status_code(), "sent print job");
            }
            PrinterConnection::Zpl { config } => {
                let Some(content_type) = content_type else {
                    eyre::bail!("url must include content-type header");
                };

                if content_type.starts_with("image/") {
                    let data = resp.bytes().await?;
                    let im = image::load_from_memory(&data)?;
                    let field = zpl::image_to_gf(&im, config.rotate);

                    trace!(field, "generated GFA instruction");

                    Self::write_to_socket(
                        config.printer_addr.to_owned(),
                        format!("^XA^FO0,0{field}^XZ").as_bytes(),
                    )
                    .await?;
                } else if content_type == "application/pdf" {
                    let data = resp.bytes().await?;

                    let images = Self::render_pdf_to_images(config, &data)?;

                    for im in images {
                        let field = zpl::image_to_gf(&im, config.rotate);
                        Self::write_to_socket(
                            config.printer_addr.to_owned(),
                            format!("^XA^FO0,0{field}^XZ").as_bytes(),
                        )
                        .await?;
                    }
                }
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn print_data(&self, data: String) -> eyre::Result<()> {
        match &self.connection {
            PrinterConnection::Cups { .. } => eyre::bail!("cups printers cannot use zpl data"),
            PrinterConnection::Zpl { config } => {
                trace!(printer_addr = ?config.printer_addr, "sending data to printer");
                Self::write_to_socket(config.printer_addr, data.as_bytes()).await?;
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
                ..
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
            PrinterConnection::Zpl { .. } => Ok(true),
        }
    }

    async fn write_to_socket(addr: SocketAddr, data: &[u8]) -> eyre::Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let socket = if addr.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            }?;
            let mut stream = socket.connect(addr).await?;

            stream.write_all(data).await?;

            Ok::<_, eyre::Report>(())
        })
        .await??;

        Ok(())
    }

    #[cfg(feature = "pdf")]
    fn render_pdf_to_images(
        config: &ZplPrintConfig,
        data: &[u8],
    ) -> eyre::Result<Vec<image::DynamicImage>> {
        use itertools::Itertools;
        use pdfium_render::prelude::*;

        let pdfium = Pdfium::default();
        let doc = pdfium.load_pdf_from_byte_slice(data, None)?;

        let (width, height) = if config.rotate {
            (config.label_height, config.label_width)
        } else {
            (config.label_width, config.label_height)
        };

        let render_config = PdfRenderConfig::new()
            .use_print_quality(true)
            .set_target_width(width)
            .set_maximum_width(width)
            .set_maximum_height(height);

        doc.pages()
            .iter()
            .map(|page| {
                page.render_with_config(&render_config)
                    .map(|page| page.as_image())
            })
            .try_collect()
            .map_err(eyre::Report::from)
    }

    #[cfg(not(feature = "pdf"))]
    fn render_pdf_to_images(
        _config: &ZplPrintConfig,
        _data: &[u8],
    ) -> eyre::Result<Vec<image::DynamicImage>> {
        eyre::bail!("no pdf support included")
    }
}
