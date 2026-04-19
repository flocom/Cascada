pub mod file_bridge;
pub mod mt_bridge;
pub mod proto;

use crate::core::model::{Account, ConnectorCmd, ConnectorEvent, Platform};
use anyhow::Result;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct ConnectorHandle {
    pub tx: mpsc::Sender<ConnectorCmd>,
}

impl ConnectorHandle {
    pub async fn send(&self, cmd: ConnectorCmd) -> Result<()> {
        self.tx.send(cmd).await.map_err(|e| anyhow::anyhow!(e.to_string()))
    }
    pub async fn shutdown(&self) {
        let _ = self.tx.send(ConnectorCmd::Shutdown).await;
    }
}

pub fn spawn_connector(
    account: Account,
    events: mpsc::UnboundedSender<ConnectorEvent>,
) -> Result<ConnectorHandle> {
    let (tx, rx) = mpsc::channel::<ConnectorCmd>(256);
    match account.platform {
        Platform::CTrader => file_bridge::spawn(account, rx, events),
        Platform::MT4 | Platform::MT5 => {
            return Err(anyhow::anyhow!(
                "MT4/MT5 accounts attach automatically via the file-discovery loop"));
        }
    }
    Ok(ConnectorHandle { tx })
}

use crate::core::events::LogLevel;

pub(crate) fn emit_log(
    events: &mpsc::UnboundedSender<ConnectorEvent>,
    account_id: &str,
    level: LogLevel,
    message: impl Into<String>,
) {
    let _ = events.send(ConnectorEvent::Log {
        account_id: account_id.to_string(),
        level,
        message: message.into(),
    });
}
