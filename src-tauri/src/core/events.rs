use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel { Info, Warn, Error }

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub id: u64,
    pub ts: i64,
    pub level: LogLevel,
    pub source: String,
    pub message: String,
}

pub const EVT_LOG: &str = "cascada://log";
pub const EVT_ACCOUNT: &str = "cascada://account";
pub const EVT_TRADE: &str = "cascada://trade";
pub const EVT_QUOTE: &str = "cascada://quote";
pub const EVT_SYMBOLS: &str = "cascada://symbols";
