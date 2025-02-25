use std::{io::Cursor, net::SocketAddr, time::Duration};

use eyre::{OptionExt, bail};
use futures::TryStreamExt;
use image::DynamicImage;
use ipp::prelude::*;
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use tokio::{io::AsyncWriteExt, net::TcpSocket};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{debug, info, instrument};

use super::ConnectionActionPrint;

mod zpl;

#[derive(Clone, Debug, Deserialize)]
pub struct PrintConfig {
    #[serde(flatten)]
    print_type: PrintTypeConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "print_type", rename_all = "snake_case")]
pub enum PrintTypeConfig {
    Cups(PrintTypeCupsConfig),
    Zpl(PrintTypeZplConfig),
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PrintTypeCupsConfig {
    #[serde_as(as = "DisplayFromStr")]
    #[serde(default = "PrintTypeCupsConfig::default_host")]
    host: Uri,
    #[serde_as(as = "DisplayFromStr")]
    printer_uri: Uri,
    #[serde(default)]
    attributes: Vec<IppAttribute>,
}

impl PrintTypeCupsConfig {
    fn default_host() -> Uri {
        "http://localhost:631".parse().unwrap()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct PrintTypeZplConfig {
    addr: SocketAddr,
    rotate: bool,
    #[cfg(feature = "pdf")]
    label_width: u32,
    #[cfg(feature = "pdf")]
    label_height: u32,
}

pub struct Printer {
    http_client: reqwest::Client,
    connection: PrinterConnection,
}

enum PrinterConnection {
    Cups {
        config: PrintTypeCupsConfig,
        ipp_client: AsyncIppClient,
    },
    Zpl {
        config: PrintTypeZplConfig,
    },
}

impl Printer {
    #[instrument(skip(config))]
    pub async fn new(config: PrintConfig) -> eyre::Result<Self> {
        let http_client = reqwest::Client::default();

        let connection = match config.print_type {
            PrintTypeConfig::Cups(cups) => {
                let ipp_client = AsyncIppClient::builder(cups.host.clone()).build();
                PrinterConnection::Cups {
                    config: cups,
                    ipp_client,
                }
            }
            PrintTypeConfig::Zpl(zpl) => PrinterConnection::Zpl { config: zpl },
        };

        let printer = Self {
            http_client,
            connection,
        };

        if !printer.validate_printer().await? {
            bail!("could validate printer connection");
        }

        Ok(printer)
    }

    #[instrument(skip(self))]
    pub async fn print(&self, action: ConnectionActionPrint) -> eyre::Result<()> {
        match action {
            ConnectionActionPrint::UrlAndData { url, data } => match &self.connection {
                PrinterConnection::Cups { .. } => {
                    eyre::bail!("merging url and data not supported for cups")
                }
                PrinterConnection::Zpl { config } => {
                    let images = self.url_to_images(config, url.as_str()).await?;
                    let images_with_data = images.into_iter().zip(data);

                    for (image, mut data) in images_with_data {
                        let Some(pos) = data.find("^XZ") else {
                            eyre::bail!("data must have end of label");
                        };

                        let field = zpl::image_to_gf(&image, config.rotate);
                        data.insert_str(pos, &field);

                        Self::write_to_socket(config.addr, data.as_bytes()).await?;
                    }
                }
            },
            ConnectionActionPrint::Url { url } => match &self.connection {
                PrinterConnection::Cups { config, ipp_client } => {
                    let resp = self.http_client.get(url).send().await?.error_for_status()?;

                    let stream = resp.bytes_stream().map_err(std::io::Error::other);
                    let reader = tokio_util::io::StreamReader::new(stream);

                    let payload = IppPayload::new_async(reader.compat());
                    let op = IppOperationBuilder::print_job(config.printer_uri.clone(), payload)
                        .attributes(config.attributes.iter().cloned())
                        .build();
                    let resp = ipp_client.send(op).await?;

                    info!(status_code = %resp.header().status_code(), "sent print job");
                }
                PrinterConnection::Zpl { config } => {
                    for image in self.url_to_images(config, url.as_str()).await? {
                        let field = zpl::image_to_gf(&image, config.rotate);

                        Self::write_to_socket(
                            config.addr,
                            format!("^XA^FO0,0{field}^XZ").as_bytes(),
                        )
                        .await?;
                    }
                }
            },
            ConnectionActionPrint::Data { data } => match &self.connection {
                PrinterConnection::Cups { config, ipp_client } => {
                    let cursor = Cursor::new(data);
                    let payload = IppPayload::new(cursor);
                    let op = IppOperationBuilder::print_job(config.printer_uri.clone(), payload)
                        .attributes(config.attributes.iter().cloned())
                        .build();
                    ipp_client.send(op).await?;
                }
                PrinterConnection::Zpl { config } => {
                    Self::write_to_socket(config.addr, data.as_bytes()).await?;
                }
            },
        }

        Ok(())
    }

    #[instrument(skip(self))]
    async fn validate_printer(&self) -> eyre::Result<bool> {
        match &self.connection {
            PrinterConnection::Cups { config, ipp_client } => {
                let op = IppOperationBuilder::cups().get_printers();
                let resp = ipp_client.send(op).await?;
                debug!(
                    status = %resp.header().status_code(),
                    "got response status"
                );

                let groups = resp.attributes().groups_of(DelimiterTag::PrinterAttributes);

                let printer_uri_str = config.printer_uri.to_string();
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

    async fn url_to_images(
        &self,
        config: &PrintTypeZplConfig,
        url: &str,
    ) -> eyre::Result<Vec<DynamicImage>> {
        let bytes = self
            .http_client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let content_type = infer::get(&bytes).ok_or_eyre("unknown url content type")?;

        let im = if content_type.matcher_type() == infer::MatcherType::Image {
            vec![image::load_from_memory(&bytes)?]
        } else {
            Self::render_pdf_to_images(config, &bytes)?
        };

        Ok(im)
    }

    #[cfg(feature = "pdf")]
    fn render_pdf_to_images(
        config: &PrintTypeZplConfig,
        data: &[u8],
    ) -> eyre::Result<Vec<DynamicImage>> {
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
            .set_target_width(width.try_into().unwrap())
            .set_maximum_width(width.try_into().unwrap())
            .set_target_height(height.try_into().unwrap());

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
        _config: &PrintTypeZplConfig,
        _data: &[u8],
    ) -> eyre::Result<Vec<DynamicImage>> {
        eyre::bail!("this build does not support pdf conversion")
    }
}
