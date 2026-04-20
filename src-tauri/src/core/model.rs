use crate::core::events::LogLevel;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Platform {
    #[serde(rename = "cTrader")]
    CTrader,
    MT4,
    MT5,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AccountRole { Master, Slave, Idle }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side { Buy, Sell }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub platform: Platform,
    pub label: String,
    pub login: String,
    pub server: String,
    pub role: AccountRole,
    // Runtime state — serialized so IPC/events carry live values.
    // Zeroed on load (`load_from_disk`) and on import (`replace_with`), so a
    // stale snapshot in state.json/exports doesn't leak back in.
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub balance: f64,
    #[serde(default)]
    pub equity: f64,
    #[serde(default = "default_ccy")]
    pub currency: String,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub password: Option<String>,
}

fn default_ccy() -> String { "USD".into() }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LotMode {
    Fixed,
    Multiplier,
    Equity,
    RiskPercent,
    BalanceRatio,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum DirectionFilter {
    #[default] All,
    BuyOnly,
    SellOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SlTpMode {
    #[default] Copy,
    Ignore,
    Fixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Schedule {
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub start_min: u32,   // minutes since midnight, broker time
    #[serde(default = "default_end_min")] pub end_min: u32,
    #[serde(default)] pub skip_weekends: bool,
}
fn default_end_min() -> u32 { 24 * 60 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyRule {
    pub id: String,
    #[serde(default)] pub name: String,
    pub master_id: String,
    pub slave_id: String,
    pub enabled: bool,
    pub lot_mode: LotMode,
    pub lot_value: f64,
    pub reverse: bool,
    pub max_slippage_pips: u32,
    #[serde(default)] pub symbol_map: HashMap<String, String>,

    // Lot constraints
    #[serde(default)] pub min_lot: f64,
    #[serde(default)] pub max_lot: f64,    // 0 = no cap

    // Symbol filters
    #[serde(default)] pub symbol_whitelist: Vec<String>,
    #[serde(default)] pub symbol_blacklist: Vec<String>,
    #[serde(default)] pub symbol_prefix: String,
    #[serde(default)] pub symbol_suffix: String,

    // Behaviour filters
    #[serde(default)] pub direction: DirectionFilter,
    #[serde(default)] pub comment_filter: String,   // substring match, "" = off

    // Risk caps
    #[serde(default)] pub max_open_positions: u32,  // 0 = unlimited
    #[serde(default)] pub max_exposure_lots: f64,   // 0 = unlimited
    #[serde(default)] pub max_daily_loss: f64,      // 0 = off; absolute value in account ccy

    // Order shaping
    #[serde(default)] pub sl_mode: SlTpMode,
    #[serde(default)] pub sl_pips: f64,
    #[serde(default)] pub tp_mode: SlTpMode,
    #[serde(default)] pub tp_pips: f64,
    #[serde(default)] pub trade_delay_ms: u64,
    #[serde(default)] pub skip_older_than_secs: i64,

    // Trailing / break-even (stored — engine wires these once a price stream lands).
    #[serde(default)] pub trailing_pips: f64,
    #[serde(default)] pub breakeven_after_pips: f64,

    // Schedule
    #[serde(default)] pub schedule: Schedule,

    // Risk-percent specifics (used when lot_mode = RiskPercent — needs SL distance).
    #[serde(default = "default_pip_value")] pub pip_value_per_lot: f64,

    // Quote-diff compensation: shift slave SL/TP by (slave_quote − master_quote)
    // so the pip-distance to SL/TP matches the master. Skip the copy entirely
    // when |diff| > skip_pips (prevents copying when broker prices have drifted).
    #[serde(default)] pub quote_compensate: bool,
    #[serde(default)] pub quote_skip_pips: f64, // 0 = no skip
    /// Deprecated — kept for backward-compat deserialization only.
    #[serde(default)] pub quote_compensate_symbols: Vec<String>,
    /// Manual per-symbol SL/TP offset in pips. Each entry shifts SL/TP for
    /// matching trades by `pips * pip_size(symbol)` so the slave's stop sits
    /// where the user expects despite broker quote drift.
    #[serde(default)] pub quote_offsets: Vec<QuoteOffset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteOffset {
    pub symbol: String,  // master-side ticker, uppercased
    pub pips: f64,       // signed pip shift applied to SL/TP
}
fn default_pip_value() -> f64 { 10.0 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub account_id: String,
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    /// Broker-provided pip size. 0 when the EA hasn't been upgraded — the
    /// frontend falls back to a symbol-name heuristic in that case.
    #[serde(default)]
    pub pip_size: f64,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub ticket: String,
    pub account_id: String,
    pub symbol: String,
    pub side: Side,
    pub volume: f64,
    pub price: f64,
    pub sl: Option<f64>,
    pub tp: Option<f64>,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub profit: Option<f64>,
    #[serde(default)]
    pub origin_ticket: Option<String>,
    #[serde(default)]
    pub comment: String,
    /// Broker-reported pip size for the instrument at the time of the event.
    /// 0 when the EA hasn't been upgraded — the engine falls back to a
    /// symbol-name heuristic in that case.
    #[serde(default)]
    pub pip_size: f64,
}

/// Kind of pending order — mirrors the master/slave broker's `OP_BUYLIMIT`
/// etc. `StopLimit` is MT5-only; MT4 and cTrader degrade it to `Stop`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PendingType { Limit, Stop, StopLimit }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOrder {
    pub ticket: String,
    pub account_id: String,
    pub symbol: String,
    pub side: Side,
    pub order_type: PendingType,
    pub volume: f64,
    pub target: f64,
    #[serde(default)] pub sl: Option<f64>,
    #[serde(default)] pub tp: Option<f64>,
    /// Expiry as UTC epoch ms; 0 = GTC.
    #[serde(default)] pub expiry: i64,
    #[serde(default)] pub origin_ticket: Option<String>,
    #[serde(default)] pub comment: String,
    #[serde(default)] pub pip_size: f64,
}

/// Normalized event coming from any connector.
#[derive(Debug, Clone)]
pub enum ConnectorEvent {
    Connected { account_id: String, login: String, balance: f64, equity: f64, currency: String },
    Disconnected { account_id: String },
    TradeOpened(Trade),
    /// Close events only need these fields — the wire proto doesn't carry the
    /// full trade at close time, and the engine/state only use ticket+account_id+profit+ts.
    /// Avoids allocating dummy `String::new()` fields for symbol/comment per close.
    TradeClosed { ticket: String, account_id: String, profit: Option<f64>, ts: i64 },
    TradeModified(Trade),
    /// Closed trade replayed from broker history at session start — bypasses copy engine.
    HistoricalTrade(Trade),
    Heartbeat { account_id: String, balance: f64, equity: f64 },
    Log { account_id: String, level: LogLevel, message: String },
    Quote(Quote),
    Symbols { account_id: String, symbols: Vec<String> },
    /// Master placed a new limit/stop order — engine mirrors it on slaves with
    /// target + SL + TP shifted by `quote_offsets`.
    PendingOpened(PendingOrder),
    PendingModified(PendingOrder),
    PendingCancelled { ticket: String, account_id: String },
    /// Pending filled on master — no slave action; the mirror pending on
    /// the slave fills on its own when the slave's broker reaches the target.
    PendingFilled { ticket: String, account_id: String },
}

/// Order request addressed to a connector.
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub origin_ticket: String,
    pub symbol: String,
    pub side: Side,
    pub volume: f64,
    pub sl: Option<f64>,
    pub tp: Option<f64>,
    pub max_slippage_pips: u32,
}

#[derive(Debug, Clone)]
pub struct PendingOrderRequest {
    pub origin_ticket: String,
    pub symbol: String,
    pub side: Side,
    pub order_type: PendingType,
    pub volume: f64,
    pub target: f64,
    pub sl: Option<f64>,
    pub tp: Option<f64>,
    /// UTC epoch ms; 0 = GTC.
    pub expiry: i64,
}

#[derive(Debug, Clone)]
pub enum ConnectorCmd {
    Open(OrderRequest),
    OpenPending(PendingOrderRequest),
    Close { ticket: String },
    Modify { ticket: String, sl: Option<f64>, tp: Option<f64> },
    ModifyPending { ticket: String, target: f64, sl: Option<f64>, tp: Option<f64>, expiry: i64 },
    CancelPending { ticket: String },
    Subscribe { symbols: Vec<String> },
    ListSymbols,
    Shutdown,
}
