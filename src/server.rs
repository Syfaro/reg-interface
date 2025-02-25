use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{Extension, Router, body::Body, extract::Path, response::Response, routing::get};
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::RunningTasks;

#[derive(Clone)]
pub struct ImageStore {
    store: bool,
    images: Arc<Mutex<HashMap<String, ImageEntry>>>,
}

impl ImageStore {
    pub fn new(store: bool) -> Self {
        Self {
            store,
            images: Default::default(),
        }
    }

    pub fn add(&self, key: String, entry: ImageEntry) {
        if !self.store {
            return;
        }

        self.images.lock().unwrap().insert(key, entry);
    }

    pub fn get(&self, key: &str) -> Option<ImageEntry> {
        if !self.store {
            return None;
        }

        let mut images = self.images.lock().unwrap();
        if let Some(entry) = images.get(key) {
            if entry.single_use {
                Some(images.remove(key).unwrap())
            } else {
                Some(entry.to_owned())
            }
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct ImageEntry {
    pub data: Vec<u8>,
    pub content_type: String,
    pub single_use: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    pub addr: SocketAddr,
}

pub async fn start(
    config: ServerConfig,
    image_store: ImageStore,
    token: CancellationToken,
    tasks: &mut RunningTasks,
) -> eyre::Result<()> {
    let router = Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/image/{key}", get(serve_image))
        .layer(Extension(image_store));

    let listener = TcpListener::bind(&config.addr).await?;
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(token.cancelled_owned())
            .await?;
        Ok(())
    });
    tasks.push(("server".to_string(), task));

    Ok(())
}

async fn serve_image(
    Extension(images): Extension<ImageStore>,
    Path(key): Path<String>,
) -> Response {
    let response = Response::builder();

    if let Some(entry) = images.get(&key) {
        response
            .header("content-type", entry.content_type)
            .body(Body::from(entry.data))
    } else {
        response.status(StatusCode::NOT_FOUND).body(Body::empty())
    }
    .unwrap()
}
