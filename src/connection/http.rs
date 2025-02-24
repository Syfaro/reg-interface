use std::{collections::HashMap, time::Duration};

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

use super::{Connection, ConnectionTypeHttpConfig};

pub struct HttpConnection {
    url: Url,
    client: reqwest::Client,
}

impl HttpConnection {
    pub async fn new(name: String, config: ConnectionTypeHttpConfig) -> eyre::Result<Self> {
        let mut default_headers = config
            .headers
            .into_iter()
            .map(|(name, value)| Ok((HeaderName::try_from(name)?, HeaderValue::try_from(value)?)))
            .collect::<Result<HeaderMap, eyre::Report>>()?;
        default_headers.insert("x-interface-connection-name", HeaderValue::from_str(&name)?);

        let builder = reqwest::ClientBuilder::default()
            .timeout(Duration::from_secs(config.timeout.unwrap_or(10)))
            .default_headers(default_headers);

        let client = builder.build()?;

        Ok(Self {
            url: config.url,
            client,
        })
    }
}

#[async_trait]
impl Connection for HttpConnection {
    async fn send(
        &self,
        data: &serde_json::Value,
        _changes: &HashMap<String, serde_json::Value>,
    ) -> eyre::Result<()> {
        self.client.post(self.url.clone()).json(data).send().await?;

        Ok(())
    }
}
