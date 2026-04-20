use crate::core::events::LogLevel;
use crate::core::model::*;
use crate::core::state::AppState;
use crate::core::ticket_map::MasterKey;
use chrono::{Datelike, Timelike, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Copy engine: fans out master events to slaves per enabled rule.
pub struct CopyEngine {
    state: Arc<AppState>,
}

#[derive(Default, Clone)]
struct SlaveCaps {
    open_count: u32,
    exposure: f64,
    net_today: f64,
}

impl CopyEngine {
    pub fn new(state: Arc<AppState>) -> Self { Self { state } }

    pub async fn on_trade_opened(&self, t: &Trade) {
        let rules: Vec<CopyRule> = self.state.rules.read().iter()
            .filter(|r| r.enabled && r.master_id == t.account_id)
            .cloned()
            .collect();
        if rules.is_empty() { return; }

        let (master_balance, master_equity) = match self.state.accounts.get(&t.account_id) {
            Some(a) => (a.balance, a.equity), None => return,
        };

        // Cache per-slave caps metrics across rules so we scan `trades` at most
        // once per distinct slave even if several enabled rules share it.
        let mut caps_cache: HashMap<String, SlaveCaps> = HashMap::new();

        for rule in rules {
            let caps = if rule.max_open_positions > 0
                || rule.max_exposure_lots > 0.0
                || rule.max_daily_loss > 0.0
            {
                Some(
                    caps_cache
                        .entry(rule.slave_id.clone())
                        .or_insert_with(|| self.compute_slave_caps(&rule.slave_id))
                        .clone(),
                )
            } else {
                None
            };
            if let Err(reason) = self.preflight(&rule, t, caps.as_ref()) {
                self.state.emit_log(LogLevel::Info, &rule.slave_id,
                    format!("skip {} ({reason})", t.ticket));
                continue;
            }
            let (slave_balance, slave_equity) = match self.state.accounts.get(&rule.slave_id) {
                Some(a) => (a.balance, a.equity), None => continue,
            };

            let symbol = translate_symbol(&rule, &t.symbol);
            let side = if rule.reverse { flip(t.side) } else { t.side };
            let volume = clamp_volume(&rule,
                compute_volume(&rule, master_balance, master_equity, slave_balance, slave_equity, t));
            if volume <= 0.0 {
                self.state.emit_log(LogLevel::Warn, &rule.slave_id,
                    format!("skip {} (volume rounded to 0)", t.ticket));
                continue;
            }

            // Manual per-symbol quote-diff compensation: shift SL/TP by
            // (user-provided pips × pip_size) so the slave's stop sits at the
            // expected absolute price despite broker quote drift. Match by
            // master-side ticker (case-insensitive); first match wins.
            // Prefer the broker-provided pip size rolled in with the trade
            // event — it's the only reliable value for indices, crypto and
            // exotic suffixed tickers where the name-based heuristic guesses
            // 0.0001 by default (and turns a legitimate −0.22-pip offset into
            // an invisible −0.000022 price shift).
            let pip = effective_pip_size(&t);
            let quote_offset: f64 = rule.quote_offsets.iter()
                .find(|o| o.symbol.eq_ignore_ascii_case(&t.symbol))
                .map(|o| o.pips * pip)
                .unwrap_or(0.0);

            let (sl, tp) = override_sl_tp(&rule, t, side, quote_offset);

            let req = OrderRequest {
                origin_ticket: t.ticket.clone(),
                symbol, side, volume, sl, tp,
                max_slippage_pips: rule.max_slippage_pips,
            };

            self.state.ticket_map.mark_pending(
                &rule.slave_id, &t.ticket,
                MasterKey { account_id: t.account_id.clone(), ticket: t.ticket.clone() },
            );

            // Fire the per-rule dispatch on its own task so `trade_delay_ms`
            // on one rule never blocks the others (previously sequential await
            // meant N slaves stacked their delays).
            let state = self.state.clone();
            let slave_id = rule.slave_id.clone();
            let delay = rule.trade_delay_ms;
            tokio::spawn(async move {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                if let Some(h) = state.connector_handle(&slave_id) {
                    if let Err(e) = h.send(ConnectorCmd::Open(req)).await {
                        state.emit_log(LogLevel::Error, &slave_id,
                            format!("copy dispatch failed: {e}"));
                    }
                } else {
                    state.emit_log(LogLevel::Warn, &slave_id, "slave offline, order skipped");
                }
            });
        }
    }

    fn compute_slave_caps(&self, slave_id: &str) -> SlaveCaps {
        let day_start = chrono::Utc::now().date_naive().and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc().timestamp_millis()).unwrap_or(0);
        let mut c = SlaveCaps::default();
        for t in self.state.trades.read().iter() {
            if t.account_id != slave_id { continue; }
            match t.closed_at {
                None => { c.open_count += 1; c.exposure += t.volume; }
                Some(closed) if closed >= day_start => {
                    c.net_today += t.profit.unwrap_or(0.0);
                }
                _ => {}
            }
        }
        c
    }

    /// Returns Err(reason) if the trade should be filtered out.
    fn preflight(&self, rule: &CopyRule, t: &Trade, caps: Option<&SlaveCaps>) -> Result<(), &'static str> {
        // Direction filter
        match rule.direction {
            DirectionFilter::All => {}
            DirectionFilter::BuyOnly  if t.side != Side::Buy  => return Err("direction filter"),
            DirectionFilter::SellOnly if t.side != Side::Sell => return Err("direction filter"),
            _ => {}
        }
        // Symbol whitelist (any match) / blacklist (any match)
        if !rule.symbol_whitelist.is_empty()
            && !rule.symbol_whitelist.iter().any(|s| sym_matches(s, &t.symbol)) {
            return Err("not in whitelist");
        }
        if rule.symbol_blacklist.iter().any(|s| sym_matches(s, &t.symbol)) {
            return Err("blacklisted symbol");
        }
        // Comment substring filter (case-insensitive, ASCII fast path —
        // broker comments are ASCII in practice).
        if !rule.comment_filter.is_empty() && !contains_ci(&t.comment, &rule.comment_filter) {
            return Err("comment filter");
        }
        // Skip stale trades
        if rule.skip_older_than_secs > 0 {
            let now_ms = chrono::Utc::now().timestamp_millis();
            if (now_ms - t.opened_at) / 1000 > rule.skip_older_than_secs {
                return Err("trade too old");
            }
        }
        // Schedule
        if rule.schedule.enabled && !in_window(&rule.schedule) {
            return Err("outside schedule");
        }
        // Open-positions / exposure / daily-loss: caps are pre-computed
        // once per slave by the caller and shared across rules.
        if let Some(c) = caps {
            if rule.max_open_positions > 0 && c.open_count >= rule.max_open_positions {
                return Err("max open positions");
            }
            if rule.max_exposure_lots > 0.0 && c.exposure >= rule.max_exposure_lots {
                return Err("max exposure");
            }
            if rule.max_daily_loss > 0.0 && -c.net_today >= rule.max_daily_loss {
                return Err("daily loss cap");
            }
        }
        Ok(())
    }

    pub async fn on_trade_closed(&self, account_id: &str, ticket: &str) {
        let key = MasterKey { account_id: account_id.to_string(), ticket: ticket.to_string() };
        let slaves = self.state.ticket_map.slaves_for(&key);
        for s in &slaves {
            if let Some(h) = self.state.connector_handle(&s.account_id) {
                let _ = h.send(ConnectorCmd::Close { ticket: s.ticket.clone() }).await;
            }
        }
        self.state.ticket_map.drop_master(&key);
    }

    pub async fn on_trade_modified(&self, t: &Trade) {
        let key = MasterKey { account_id: t.account_id.clone(), ticket: t.ticket.clone() };
        for s in self.state.ticket_map.slaves_for(&key) {
            if let Some(h) = self.state.connector_handle(&s.account_id) {
                let _ = h.send(ConnectorCmd::Modify {
                    ticket: s.ticket, sl: t.sl, tp: t.tp,
                }).await;
            }
        }
    }

    pub async fn on_pending_opened(&self, p: &PendingOrder) {
        let rules: Vec<CopyRule> = self.state.rules.read().iter()
            .filter(|r| r.enabled && r.master_id == p.account_id)
            .cloned()
            .collect();
        if rules.is_empty() { return; }

        let (master_balance, master_equity) = match self.state.accounts.get(&p.account_id) {
            Some(a) => (a.balance, a.equity), None => return,
        };
        let mut caps_cache: HashMap<String, SlaveCaps> = HashMap::new();

        // Synthesize a Trade-shaped value so we can reuse the filter +
        // sizing logic designed for market orders. `price` is the pending
        // target (close enough for RiskPercent's SL-distance maths).
        let as_trade = pending_as_trade(p);

        for rule in rules {
            let caps = if rule.max_open_positions > 0
                || rule.max_exposure_lots > 0.0
                || rule.max_daily_loss > 0.0
            {
                Some(
                    caps_cache.entry(rule.slave_id.clone())
                        .or_insert_with(|| self.compute_slave_caps(&rule.slave_id))
                        .clone(),
                )
            } else { None };
            if let Err(reason) = self.preflight(&rule, &as_trade, caps.as_ref()) {
                self.state.emit_log(LogLevel::Info, &rule.slave_id,
                    format!("skip pending {} ({reason})", p.ticket));
                continue;
            }
            let (slave_balance, slave_equity) = match self.state.accounts.get(&rule.slave_id) {
                Some(a) => (a.balance, a.equity), None => continue,
            };

            let symbol = translate_symbol(&rule, &p.symbol);
            let side = if rule.reverse { flip(p.side) } else { p.side };
            let volume = clamp_volume(&rule,
                compute_volume(&rule, master_balance, master_equity, slave_balance, slave_equity, &as_trade));
            if volume <= 0.0 {
                self.state.emit_log(LogLevel::Warn, &rule.slave_id,
                    format!("skip pending {} (volume rounded to 0)", p.ticket));
                continue;
            }

            // Same manual per-symbol pip-offset used for market copies — but
            // applied to target AND sl AND tp, since a pending has a known
            // entry price (unlike a market order where the slave's entry is
            // whatever its broker fills at).
            let pip = effective_pip_size(&as_trade);
            let quote_offset: f64 = rule.quote_offsets.iter()
                .find(|o| o.symbol.eq_ignore_ascii_case(&p.symbol))
                .map(|o| o.pips * pip)
                .unwrap_or(0.0);
            let target = p.target + quote_offset;
            let sl = p.sl.map(|v| v + quote_offset);
            let tp = p.tp.map(|v| v + quote_offset);

            let req = PendingOrderRequest {
                origin_ticket: p.ticket.clone(),
                symbol, side, order_type: p.order_type,
                volume, target, sl, tp, expiry: p.expiry,
            };

            self.state.ticket_map.mark_pending(
                &rule.slave_id, &p.ticket,
                MasterKey { account_id: p.account_id.clone(), ticket: p.ticket.clone() },
            );

            let state = self.state.clone();
            let slave_id = rule.slave_id.clone();
            let delay = rule.trade_delay_ms;
            tokio::spawn(async move {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                if let Some(h) = state.connector_handle(&slave_id) {
                    if let Err(e) = h.send(ConnectorCmd::OpenPending(req)).await {
                        state.emit_log(LogLevel::Error, &slave_id,
                            format!("pending dispatch failed: {e}"));
                    }
                } else {
                    state.emit_log(LogLevel::Warn, &slave_id, "slave offline, pending skipped");
                }
            });
        }
    }

    /// Mirror a master-side modify to the slave pending(s). The wire frame
    /// doesn't carry the symbol, so we can't re-derive `quote_offset` here;
    /// values are forwarded 1:1 (an acceptable approximation — users who
    /// care can capture a new offset via the Compare tab).
    pub async fn on_pending_modified(&self, p: &PendingOrder) {
        let key = MasterKey { account_id: p.account_id.clone(), ticket: p.ticket.clone() };
        for s in self.state.ticket_map.slaves_for(&key) {
            if let Some(h) = self.state.connector_handle(&s.account_id) {
                let _ = h.send(ConnectorCmd::ModifyPending {
                    ticket: s.ticket, target: p.target, sl: p.sl, tp: p.tp, expiry: p.expiry,
                }).await;
            }
        }
    }

    pub async fn on_pending_cancelled(&self, account_id: &str, ticket: &str) {
        let key = MasterKey { account_id: account_id.to_string(), ticket: ticket.to_string() };
        for s in self.state.ticket_map.slaves_for(&key) {
            if let Some(h) = self.state.connector_handle(&s.account_id) {
                let _ = h.send(ConnectorCmd::CancelPending { ticket: s.ticket.clone() }).await;
            }
        }
        self.state.ticket_map.drop_master(&key);
    }
}

/// Project a PendingOrder into a Trade shape so we can reuse preflight +
/// volume sizing without touching their signatures.
fn pending_as_trade(p: &PendingOrder) -> Trade {
    Trade {
        ticket: p.ticket.clone(),
        account_id: p.account_id.clone(),
        symbol: p.symbol.clone(),
        side: p.side,
        volume: p.volume,
        price: p.target,
        sl: p.sl,
        tp: p.tp,
        opened_at: chrono::Utc::now().timestamp_millis(),
        closed_at: None,
        profit: None,
        origin_ticket: p.origin_ticket.clone(),
        comment: p.comment.clone(),
        pip_size: p.pip_size,
    }
}

fn flip(s: Side) -> Side { if matches!(s, Side::Buy) { Side::Sell } else { Side::Buy } }

fn sym_matches(pattern: &str, symbol: &str) -> bool {
    // Simple case-insensitive substring match (good enough for "EUR", "XAU", "USDJPY.r").
    // ASCII-only fast path — broker tickers are always ASCII; avoids two heap allocs/call.
    contains_ci(symbol, pattern.trim())
}

/// ASCII-case-insensitive substring search; no allocation.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() { return true; }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() { return false; }
    'outer: for i in 0..=h.len() - n.len() {
        for j in 0..n.len() {
            if !h[i + j].eq_ignore_ascii_case(&n[j]) { continue 'outer; }
        }
        return true;
    }
    false
}

fn translate_symbol(rule: &CopyRule, master_sym: &str) -> String {
    let base = rule.symbol_map.get(master_sym).cloned().unwrap_or_else(|| master_sym.to_string());
    format!("{}{base}{}", rule.symbol_prefix, rule.symbol_suffix)
}

fn in_window(s: &Schedule) -> bool {
    let now = Utc::now();
    if s.skip_weekends {
        let wd = now.weekday().num_days_from_monday();
        if wd >= 5 { return false; }
    }
    let cur = now.hour() * 60 + now.minute();
    if s.start_min <= s.end_min {
        cur >= s.start_min && cur < s.end_min
    } else {
        // overnight window (e.g. 22:00 → 06:00)
        cur >= s.start_min || cur < s.end_min
    }
}

fn override_sl_tp(rule: &CopyRule, t: &Trade, side: Side, quote_offset: f64) -> (Option<f64>, Option<f64>) {
    let pip = effective_pip_size(t);
    // Copy mode shifts master's absolute SL/TP into slave's price space so the
    // pip-distance is preserved. Fixed mode is already relative to slave entry.
    let sl = match rule.sl_mode {
        SlTpMode::Copy   => t.sl.map(|v| v + quote_offset),
        SlTpMode::Ignore => None,
        SlTpMode::Fixed  => fixed_sl(t.price, side, rule.sl_pips, pip),
    };
    let tp = match rule.tp_mode {
        SlTpMode::Copy   => t.tp.map(|v| v + quote_offset),
        SlTpMode::Ignore => None,
        SlTpMode::Fixed  => fixed_tp(t.price, side, rule.tp_pips, pip),
    };
    (sl, tp)
}

fn fixed_sl(entry: f64, side: Side, pips: f64, pip: f64) -> Option<f64> {
    if pips <= 0.0 { return None; }
    Some(match side { Side::Buy => entry - pips * pip, Side::Sell => entry + pips * pip })
}
fn fixed_tp(entry: f64, side: Side, pips: f64, pip: f64) -> Option<f64> {
    if pips <= 0.0 { return None; }
    Some(match side { Side::Buy => entry + pips * pip, Side::Sell => entry - pips * pip })
}

fn pip_size(sym: &str) -> f64 {
    // ASCII case-insensitive checks; tickers are ASCII so this skips a heap alloc.
    if contains_ci(sym, "JPY") { 0.01 }
    else if starts_with_ci(sym, "XAU") || starts_with_ci(sym, "XAG") { 0.1 }
    else { 0.0001 }
}

/// Use the broker-reported pip size if the EA shipped it (all EAs from
/// v0.1.6+ do). Falls back to the heuristic only on trades that predate
/// the upgrade — covers the JPY/XAU cases; indices/crypto need the real
/// value to avoid a 100× off offset application.
fn effective_pip_size(t: &Trade) -> f64 {
    if t.pip_size > 0.0 { t.pip_size } else { pip_size(&t.symbol) }
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    let s = s.as_bytes();
    let p = prefix.as_bytes();
    s.len() >= p.len() && s[..p.len()].iter().zip(p).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn compute_volume(
    rule: &CopyRule,
    master_balance: f64, master_equity: f64,
    slave_balance: f64, slave_equity: f64,
    t: &Trade,
) -> f64 {
    let v = match rule.lot_mode {
        LotMode::Fixed       => rule.lot_value,
        LotMode::Multiplier  => t.volume * rule.lot_value,
        LotMode::Equity      => {
            if master_equity > 0.0 { t.volume * (slave_equity / master_equity) * rule.lot_value }
            else { t.volume * rule.lot_value }
        }
        LotMode::BalanceRatio => {
            if master_balance > 0.0 { t.volume * (slave_balance / master_balance) * rule.lot_value }
            else { t.volume * rule.lot_value }
        }
        LotMode::RiskPercent => {
            // Risk = lot_value % of slave equity, sized off SL distance in pips.
            let risk_amount = slave_equity * rule.lot_value / 100.0;
            let pip = effective_pip_size(t);
            let sl_pips = match (t.sl, t.price) {
                (Some(sl), p) if p > 0.0 && pip > 0.0 => ((p - sl).abs() / pip).max(1.0),
                _ => 20.0,
            };
            let pip_value = if rule.pip_value_per_lot > 0.0 { rule.pip_value_per_lot } else { 10.0 };
            risk_amount / (sl_pips * pip_value)
        }
    };
    (v * 100.0).round() / 100.0
}

fn clamp_volume(rule: &CopyRule, v: f64) -> f64 {
    let mut v = v;
    if rule.min_lot > 0.0 && v < rule.min_lot { v = rule.min_lot; }
    if rule.max_lot > 0.0 && v > rule.max_lot { v = rule.max_lot; }
    v
}
