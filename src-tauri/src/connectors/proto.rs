//! Wire protocol shared by file_bridge (cTrader cBot) and mt_bridge (MT EA).

use crate::connectors::emit_log;
use crate::core::events::LogLevel;
use crate::core::model::*;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum C2S<'a> {
    Open {
        ticket: &'a str, symbol: &'a str, side: Side,
        volume: f64, sl: f64, tp: f64, slippage: u32,
    },
    Close { ticket: &'a str },
    Modify { ticket: &'a str, sl: f64, tp: f64 },
    /// Replace the EA's quote-streaming subscription set. Empty list = stop.
    Subscribe { symbols: &'a [String] },
    /// Ask the EA to dump its full available-symbol list (broker watchlist).
    ListSymbols {},
}

impl<'a> C2S<'a> {
    pub fn from_cmd(cmd: &'a ConnectorCmd) -> Option<Self> {
        Some(match cmd {
            ConnectorCmd::Open(o) => C2S::Open {
                ticket: &o.origin_ticket, symbol: &o.symbol, side: o.side,
                volume: o.volume,
                sl: o.sl.unwrap_or(0.0), tp: o.tp.unwrap_or(0.0),
                slippage: o.max_slippage_pips,
            },
            ConnectorCmd::Close { ticket } => C2S::Close { ticket },
            ConnectorCmd::Modify { ticket, sl, tp } => C2S::Modify {
                ticket, sl: sl.unwrap_or(0.0), tp: tp.unwrap_or(0.0),
            },
            ConnectorCmd::Subscribe { symbols } => C2S::Subscribe { symbols },
            ConnectorCmd::ListSymbols => C2S::ListSymbols {},
            ConnectorCmd::Shutdown => return None,
        })
    }
}

/// Plugin-side events. Unknown fields are ignored so the wire format can grow
/// without breaking older builds.
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum S2C {
    Welcome {
        balance: f64, equity: f64, currency: String,
        #[serde(default)] account: String,
        // Enriched fields — accepted but currently unused by the engine.
        #[serde(default)] margin: f64,
        #[serde(default)] free_margin: f64,
        #[serde(default)] leverage: f64,
        #[serde(default)] broker: String,
        #[serde(default)] is_live: bool,
    },
    Heartbeat {
        balance: f64, equity: f64,
        #[serde(default)] margin: f64,
        #[serde(default)] free_margin: f64,
        #[serde(default)] unrealized: f64,
        #[serde(default)] positions: u32,
        #[serde(default)] pending: u32,
    },
    Open {
        ticket: String, symbol: String, side: Side, volume: f64, price: f64,
        #[serde(default)] sl: f64, #[serde(default)] tp: f64, ts: i64,
        #[serde(default)] origin: String,
        #[serde(default)] commission: f64,
        #[serde(default)] swap: f64,
        #[serde(default)] pip_size: f64,
        #[serde(default)] label: String,
        #[serde(default)] comment: String,
    },
    Close {
        ticket: String, #[serde(default)] profit: f64, ts: i64,
        #[serde(default)] gross: f64,
        #[serde(default)] commission: f64,
        #[serde(default)] swap: f64,
        #[serde(default)] pips: f64,
        #[serde(default)] balance: f64,
        #[serde(default)] reason: String,
        #[serde(default)] price: f64,
    },
    Modify {
        ticket: String,
        #[serde(default)] sl: f64,
        #[serde(default)] tp: f64,
        #[serde(default)] volume: f64,
    },
    Pending {
        ticket: String, symbol: String, side: Side,
        #[serde(default)] order_type: String,
        volume: f64,
        #[serde(default)] target: f64,
        #[serde(default)] sl: f64,
        #[serde(default)] tp: f64,
        #[serde(default)] expiry: i64,
        #[serde(default)] origin: String,
    },
    PendingModify {
        ticket: String,
        #[serde(default)] target: f64,
        #[serde(default)] sl: f64,
        #[serde(default)] tp: f64,
        #[serde(default)] volume: f64,
    },
    PendingCancel { ticket: String, #[serde(default)] symbol: String },
    PendingFill { ticket: String, #[serde(default)] symbol: String },
    History {
        ticket: String, symbol: String, side: Side, volume: f64,
        #[serde(default)] entry: f64,
        #[serde(default)] close: f64,
        #[serde(default)] profit: f64,
        #[serde(default)] origin: String,
        #[serde(default)] opened_at: i64,
        #[serde(default)] closed_at: i64,
    },
    HistoryDone { #[serde(default)] count: u32 },
    Pong { #[serde(default)] ts: i64 },
    Log { level: LogLevel, message: String },
    Quote {
        symbol: String,
        bid: f64,
        ask: f64,
        #[serde(default)] pip_size: f64,
        #[serde(default)] ts: i64,
    },
    Symbols { symbols: Vec<String> },
}

pub fn dispatch(account: &Account, msg: S2C, events: &mpsc::UnboundedSender<ConnectorEvent>) {
    let id = &account.id;
    match msg {
        S2C::Welcome { balance, equity, currency, account, .. } =>
            { let _ = events.send(ConnectorEvent::Connected {
                account_id: id.clone(), login: account, balance, equity, currency }); },
        S2C::Heartbeat { balance, equity, .. } =>
            { let _ = events.send(ConnectorEvent::Heartbeat {
                account_id: id.clone(), balance, equity }); },
        S2C::Open { ticket, symbol, side, volume, price, sl, tp, ts, origin, comment, pip_size, .. } =>
            { let _ = events.send(ConnectorEvent::TradeOpened(Trade {
                ticket, account_id: id.clone(),
                symbol, side, volume, price,
                sl: opt(sl), tp: opt(tp),
                opened_at: ts, closed_at: None, profit: None,
                origin_ticket: (!origin.is_empty()).then_some(origin),
                comment, pip_size,
            })); },
        S2C::Close { ticket, profit, ts, .. } =>
            { let _ = events.send(ConnectorEvent::TradeClosed {
                ticket, account_id: id.clone(), profit: Some(profit), ts,
            }); },
        S2C::Modify { ticket, sl, tp, .. } =>
            { let _ = events.send(ConnectorEvent::TradeModified(Trade {
                ticket, account_id: id.clone(),
                symbol: String::new(), side: Side::Buy, volume: 0.0, price: 0.0,
                sl: opt(sl), tp: opt(tp),
                opened_at: 0, closed_at: None, profit: None,
                origin_ticket: None, comment: String::new(), pip_size: 0.0,
            })); },
        S2C::Pending { ticket, symbol, side, order_type, volume, target, .. } =>
            emit_log(events, id, LogLevel::Info,
                format!("pending {order_type} {side:?} {volume} {symbol} @ {target} (#{ticket})")),
        S2C::PendingModify { ticket, target, .. } =>
            emit_log(events, id, LogLevel::Info,
                format!("pending #{ticket} modified → {target}")),
        S2C::PendingCancel { ticket, .. } =>
            emit_log(events, id, LogLevel::Info, format!("pending #{ticket} cancelled")),
        S2C::PendingFill { ticket, .. } =>
            emit_log(events, id, LogLevel::Info, format!("pending #{ticket} filled")),
        S2C::History { ticket, symbol, side, volume, entry, close: _, profit, origin, opened_at, closed_at } =>
            { let _ = events.send(ConnectorEvent::HistoricalTrade(Trade {
                ticket, account_id: id.clone(),
                symbol, side, volume, price: entry,
                sl: None, tp: None,
                opened_at, closed_at: Some(closed_at), profit: Some(profit),
                origin_ticket: (!origin.is_empty()).then_some(origin),
                comment: String::new(), pip_size: 0.0,
            })); },
        S2C::HistoryDone { count } =>
            emit_log(events, id, LogLevel::Info, format!("history snapshot: {count} trades")),
        S2C::Pong { .. } => {}
        S2C::Log { level, message } => emit_log(events, id, level, message),
        S2C::Quote { symbol, bid, ask, pip_size, ts } => {
            let _ = events.send(ConnectorEvent::Quote(Quote {
                account_id: id.clone(), symbol, bid, ask, pip_size,
                ts: if ts > 0 { ts } else { chrono::Utc::now().timestamp_millis() },
            }));
        }
        S2C::Symbols { symbols } => {
            let _ = events.send(ConnectorEvent::Symbols { account_id: id.clone(), symbols });
        }
    }
}

pub fn opt(f: f64) -> Option<f64> { (f != 0.0).then_some(f) }
