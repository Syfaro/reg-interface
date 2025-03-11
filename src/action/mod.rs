use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use url::Url;

use crate::RunningTasks;

mod print;

#[derive(Clone, Default, Debug, Deserialize)]
pub struct ActionConfig {
    print: Option<print::PrintConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConnectionAction {
    Print(ConnectionActionPrint),
}

impl ConnectionAction {
    pub fn from_name(name: &str, data: &[u8]) -> Option<Result<Self, serde_json::Error>> {
        Some(match name {
            "print" => serde_json::from_slice(data).map(ConnectionAction::Print),
            _ => return None,
        })
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ConnectionActionPrint {
    UrlAndData {
        #[serde_as(as = "DisplayFromStr")]
        url: Url,
        data: Vec<String>,
    },
    Url {
        #[serde_as(as = "DisplayFromStr")]
        url: Url,
    },
    Data {
        data: String,
    },
}

pub async fn start(
    config: ActionConfig,
    token: CancellationToken,
    action_rx: mpsc::Receiver<ConnectionAction>,
    tasks: &mut RunningTasks,
) -> eyre::Result<()> {
    let printer = if let Some(print_config) = config.print {
        Some(print::Printer::new(print_config).await?)
    } else {
        None
    };

    let task = tokio::spawn(action_task(token, action_rx, printer));
    tasks.push(("action".to_string(), task));

    Ok(())
}

async fn action_task(
    token: CancellationToken,
    mut action_rx: mpsc::Receiver<ConnectionAction>,
    printer: Option<print::Printer>,
) -> eyre::Result<()> {
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                break;
            }

            action = action_rx.recv() => {
                match action {
                    Some(ConnectionAction::Print(print_action)) => {
                        if let Some(printer) = &printer {
                            printer.print(print_action).await?;
                        } else {
                            warn!("got print action but had no printer");
                        }
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
