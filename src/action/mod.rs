use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

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

#[derive(Clone, Debug, Deserialize)]
pub struct ConnectionActionPrint {
    pub url: String,
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
                    Some(ConnectionAction::Print(ConnectionActionPrint { url })) => {
                        if let Some(printer) = &printer {
                            printer.print_url(&url).await?;
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
