use std::{collections::HashSet, time::Duration};

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

pub struct HttpConnection {
    name: String,
    client: reqwest::Client,
    url: Url,
    supported_decoders: HashSet<crate::scanner::DecoderType>,
}

impl HttpConnection {
    pub async fn new(
        name: String,
        supported_decoders: HashSet<crate::scanner::DecoderType>,
        config: super::HttpConnectionConfig,
    ) -> eyre::Result<Self> {
        let url = config.url.parse()?;

        let mut default_headers = config
            .headers
            .into_iter()
            .map(|(name, value)| Ok((HeaderName::try_from(name)?, HeaderValue::try_from(value)?)))
            .collect::<Result<HeaderMap, eyre::Report>>()?;
        default_headers.insert("x-scanner-connection-name", HeaderValue::from_str(&name)?);

        let builder = reqwest::ClientBuilder::default()
            .timeout(Duration::from_secs(config.timeout.unwrap_or(10)))
            .default_headers(default_headers);

        let builder = if let Some(user_agent) = config.user_agent {
            builder.user_agent(user_agent)
        } else {
            builder
        };

        let client = builder.build()?;

        Ok(Self {
            name,
            client,
            url,
            supported_decoders,
        })
    }
}

#[async_trait]
impl super::Connection for HttpConnection {
    fn name(&self) -> &str {
        &self.name
    }

    fn supported_decoder_types(&self) -> &HashSet<crate::scanner::DecoderType> {
        &self.supported_decoders
    }

    async fn send(&self, result: &crate::scanner::ScanResult) -> eyre::Result<()> {
        self.client
            .post(self.url.clone())
            .json(&result.data_to_send()?)
            .send()
            .await?;

        Ok(())
    }
}
