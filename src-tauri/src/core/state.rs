use crate::connectors::file_bridge::cascada_root;
use crate::connectors::{spawn_connector, ConnectorHandle};
use crate::core::engine::CopyEngine;
use crate::core::events::{LogEntry, LogLevel, EVT_ACCOUNT, EVT_LOG, EVT_QUOTE, EVT_SYMBOLS, EVT_TRADE};
use crate::core::model::*;
use crate::core::persistence::{self, Snapshot};
use crate::core::ticket_map::TicketMap;
use anyhow::Result;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, Notify};

const TRADE_BUFFER_CAP: usize = 1000;
const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);
/// Minimum gap between front-end quote events for the same (account, symbol).
/// Tick streams can fire >50 Hz; this caps UI refresh at 5 Hz per pair.
const QUOTE_EMIT_THROTTLE: std::time::Duration = std::time::Duration::from_millis(200);

fn next_log_id() -> u64 {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

pub struct AppState {
    pub accounts: DashMap<String, Account>,
    pub rules: RwLock<Vec<CopyRule>>,
    pub trades: RwLock<VecDeque<Arc<Trade>>>,
    pub connectors: DashMap<String, ConnectorHandle>,
    pub ticket_map: Arc<TicketMap>,
    /// Latest quote per (account_id, uppercased symbol). Updated on every tick;
    /// front-end emission is throttled separately via `quote_last_emit`.
    pub quotes: DashMap<(String, String), Quote>,
    quote_last_emit: DashMap<(String, String), std::time::Instant>,
    /// Active per-account subscription set (uppercased symbols). Authoritative
    /// source replayed to the EA on reconnect.
    pub subscriptions: DashMap<String, Vec<String>>,
    /// Latest broker watchlist per account (uppercased, refreshed on demand).
    pub symbols: DashMap<String, Vec<String>>,
    pub event_tx: mpsc::UnboundedSender<ConnectorEvent>,
    event_rx: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<ConnectorEvent>>>,
    app_handle: OnceLock<AppHandle>,
    save_dirty: AtomicBool,
    save_notify: Notify,
}

impl AppState {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            accounts: DashMap::new(),
            rules: RwLock::new(Vec::new()),
            trades: RwLock::new(VecDeque::with_capacity(TRADE_BUFFER_CAP)),
            connectors: DashMap::new(),
            quotes: DashMap::new(),
            quote_last_emit: DashMap::new(),
            subscriptions: DashMap::new(),
            symbols: DashMap::new(),
            ticket_map: Arc::new(TicketMap::new()),
            event_tx: tx,
            event_rx: parking_lot::Mutex::new(Some(rx)),
            app_handle: OnceLock::new(),
            save_dirty: AtomicBool::new(false),
            save_notify: Notify::new(),
        }
    }

    pub fn attach_app_handle(&self, h: AppHandle) { let _ = self.app_handle.set(h); }

    pub fn emit_log(&self, level: LogLevel, source: &str, message: impl Into<String>) {
        let msg = message.into();
        match level {
            LogLevel::Error => tracing::error!("[{source}] {msg}"),
            LogLevel::Warn => tracing::warn!("[{source}] {msg}"),
            LogLevel::Info => tracing::info!("[{source}] {msg}"),
        }
        if let Some(h) = self.app_handle.get() {
            let _ = h.emit(EVT_LOG, LogEntry {
                id: next_log_id(),
                ts: chrono::Utc::now().timestamp_millis(),
                level, source: source.into(), message: msg,
            });
        }
    }

    fn emit_account(&self, a: &Account) {
        if let Some(h) = self.app_handle.get() { let _ = h.emit(EVT_ACCOUNT, a); }
    }

    /// Same as `emit_account` but callable from outside the module —
    /// commands that mutate accounts directly (rename, set_role) need this
    /// so the UI receives the change without waiting for a manual refresh.
    pub fn emit_account_public(&self, a: &Account) { self.emit_account(a); }

    fn emit_trade(&self, t: &Trade) {
        if let Some(h) = self.app_handle.get() { let _ = h.emit(EVT_TRADE, t); }
    }

    /// Throttled quote emission: drops events when the last emit for this
    /// `key` was less than `QUOTE_EMIT_THROTTLE` ago. The latest value is
    /// always retained in `quotes`, so `list_quotes` returns fresh data.
    /// Takes the pre-built key by reference so the hot path doesn't clone it
    /// twice (once for `quotes`, once for throttle map).
    fn emit_quote_throttled(&self, key: &(String, String), q: &Quote) {
        let now = std::time::Instant::now();
        let should_emit = match self.quote_last_emit.get(key) {
            Some(prev) => now.duration_since(*prev) >= QUOTE_EMIT_THROTTLE,
            None => true,
        };
        if !should_emit { return; }
        self.quote_last_emit.insert(key.clone(), now);
        if let Some(h) = self.app_handle.get() { let _ = h.emit(EVT_QUOTE, q); }
    }

    /// Mark the snapshot dirty; the debounce loop flushes it to disk.
    pub fn mark_dirty(&self) {
        self.save_dirty.store(true, Ordering::Relaxed);
        self.save_notify.notify_one();
    }

    pub fn spawn_save_loop(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                this.save_notify.notified().await;
                tokio::time::sleep(SAVE_DEBOUNCE).await;
                if this.save_dirty.swap(false, Ordering::Relaxed) {
                    if let Err(e) = this.save_to_disk().await {
                        tracing::warn!("save failed: {e}");
                    }
                }
            }
        });
    }

    /// Apply `f` to the account under `id`, emit the update only if `f` returned true.
    fn with_account(&self, id: &str, f: impl FnOnce(&mut Account) -> bool) {
        if let Some(mut a) = self.accounts.get_mut(id) {
            if f(&mut a) { self.emit_account(&a); }
        }
    }

    pub async fn load_from_disk(self: &Arc<Self>) -> Result<()> {
        let snap = persistence::load().await?;
        for mut a in snap.accounts {
            // Connectors haven't dialled in yet — start every account offline
            // and let the first heartbeat flip the pill back to online.
            a.connected = false;
            a.balance = 0.0;
            a.equity = 0.0;
            self.accounts.insert(a.id.clone(), a);
        }
        *self.rules.write() = snap.rules;
        Ok(())
    }

    pub async fn save_to_disk(&self) -> Result<()> {
        // Serialize under the rules read-lock so we never clone the account
        // map or rules vector — the custom `SnapshotRef` borrows straight
        // from `self`. Drop the guard before the async write.
        let bytes = {
            let rules = self.rules.read();
            let snap = persistence::SnapshotRef {
                accounts: &self.accounts,
                rules: rules.as_slice(),
            };
            serde_json::to_vec_pretty(&snap)?
        };
        persistence::save_bytes(bytes).await
    }

    /// Cloned snapshot — used by the user-facing export command, not the
    /// hot debounced autosave path (that one borrows via `SnapshotRef`).
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            accounts: self.accounts.iter().map(|kv| kv.value().clone()).collect(),
            rules: self.rules.read().clone(),
        }
    }

    /// Replace all accounts + rules with the supplied snapshot.
    /// Disconnects every active connector first; caller should call
    /// `reconnect_all` afterwards to wire up the imported accounts.
    pub async fn replace_with(self: &Arc<Self>, snap: Snapshot) {
        let ids: Vec<String> = self.connectors.iter().map(|kv| kv.key().clone()).collect();
        for id in ids { let _ = self.disconnect(&id).await; }
        self.accounts.clear();
        for mut a in snap.accounts {
            a.connected = false;
            a.balance = 0.0;
            a.equity = 0.0;
            self.accounts.insert(a.id.clone(), a.clone());
            self.emit_account(&a);
        }
        *self.rules.write() = snap.rules;
        self.trades.write().clear();
        self.ticket_map.clear();
        self.quotes.clear();
        self.quote_last_emit.clear();
        self.subscriptions.clear();
        self.symbols.clear();
        self.mark_dirty();
    }

    pub async fn start_engine(self: &Arc<Self>) {
        let rx = self.event_rx.lock().take().expect("engine already started");
        let this = self.clone();
        let engine = Arc::new(CopyEngine::new(this.clone()));
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(ev) = rx.recv().await {
                this.handle_event(ev, engine.as_ref()).await;
            }
        });
    }

    async fn handle_event(&self, ev: ConnectorEvent, engine: &CopyEngine) {
        match ev {
            ConnectorEvent::Connected { account_id, login, balance, equity, currency } => {
                let mut should_save = false;
                self.with_account(&account_id, |a| {
                    let changed = !a.connected || a.balance != balance
                        || a.equity != equity || a.currency != currency;
                    a.connected = true;
                    a.balance = balance;
                    a.equity = equity;
                    a.currency = currency;
                    if !login.is_empty() && a.login != login {
                        a.login = login;
                        should_save = true;
                    }
                    changed || should_save
                });
                if should_save { self.mark_dirty(); }
                self.emit_log(LogLevel::Info, &account_id, "connected");
            }
            ConnectorEvent::Disconnected { account_id } => {
                self.with_account(&account_id, |a| {
                    if !a.connected { return false; }
                    a.connected = false;
                    true
                });
                self.emit_log(LogLevel::Warn, &account_id, "disconnected");
            }
            ConnectorEvent::Heartbeat { account_id, balance, equity } => {
                self.with_account(&account_id, |a| {
                    let changed = !a.connected || a.balance != balance || a.equity != equity;
                    a.connected = true;
                    a.balance = balance;
                    a.equity = equity;
                    changed
                });
            }
            ConnectorEvent::TradeOpened(t) => {
                let t = Arc::new(t);
                {
                    let mut trades = self.trades.write();
                    trades.push_front(Arc::clone(&t));
                    if trades.len() > TRADE_BUFFER_CAP { trades.pop_back(); }
                }
                self.emit_trade(&t);

                let is_mirror = t.origin_ticket.as_deref().map_or(false, |origin| {
                    let matched = self.ticket_map.resolve_slave_open(
                        &t.account_id, origin, &t.ticket,
                    );
                    if matched {
                        self.emit_log(LogLevel::Info, &t.account_id,
                            format!("mirror {} ↔ master {origin}", t.ticket));
                    }
                    matched
                });
                // A master TradeOpened whose ticket already has slave mappings
                // is the position born from a pending we already mirrored
                // (ticket_map was migrated by PendingFilled). Skip the engine
                // fan-out so the slave pending can fill on its own instead of
                // receiving a duplicate market order.
                let already_mapped = !is_mirror && self.ticket_map.has_master(
                    &crate::core::ticket_map::MasterKey {
                        account_id: t.account_id.clone(),
                        ticket: t.ticket.clone(),
                    });
                if !is_mirror && !already_mapped {
                    engine.on_trade_opened(&t).await;
                }
            }
            ConnectorEvent::TradeClosed { ticket, account_id, profit, ts } => {
                // Build a minimal Trade payload only for frontend emit; engine
                // only needs (account_id, ticket) and skips the Trade struct.
                let t = Trade {
                    ticket: ticket.clone(), account_id: account_id.clone(),
                    symbol: String::new(), side: Side::Buy, volume: 0.0, price: 0.0,
                    sl: None, tp: None,
                    opened_at: ts, closed_at: Some(ts), profit,
                    origin_ticket: None, comment: String::new(), pip_size: 0.0,
                };
                self.emit_trade(&t);
                engine.on_trade_closed(&account_id, &ticket).await;
            }
            ConnectorEvent::HistoricalTrade(t) => {
                let t = Arc::new(t);
                {
                    let mut trades = self.trades.write();
                    trades.push_back(Arc::clone(&t));
                    if trades.len() > TRADE_BUFFER_CAP { trades.pop_front(); }
                }
                self.emit_trade(&t);
            }
            ConnectorEvent::TradeModified(t) => {
                self.emit_trade(&t);
                engine.on_trade_modified(&t).await;
            }
            ConnectorEvent::Log { account_id, level, message } => {
                self.emit_log(level, &account_id, message);
            }
            ConnectorEvent::Quote(mut q) => {
                // ASCII-uppercase in place — tickers are always ASCII, so this
                // avoids a full UTF-8 `to_uppercase()` allocation per tick.
                q.symbol.make_ascii_uppercase();
                let key = (q.account_id.clone(), q.symbol.clone());
                self.emit_quote_throttled(&key, &q);
                self.quotes.insert(key, q);
            }
            ConnectorEvent::PendingOpened(p) => {
                // Mirror-detection: if a slave EA reports a pending whose
                // `origin_ticket` matches one we dispatched, wire the slave
                // ticket into the ticket_map and skip re-dispatching to
                // avoid infinite mirror loops.
                let is_mirror = p.origin_ticket.as_deref().map_or(false, |origin| {
                    let matched = self.ticket_map.resolve_slave_open(
                        &p.account_id, origin, &p.ticket,
                    );
                    if matched {
                        self.emit_log(LogLevel::Info, &p.account_id,
                            format!("pending mirror {} ↔ master {origin}", p.ticket));
                    }
                    matched
                });
                if !is_mirror { engine.on_pending_opened(&p).await; }
            }
            ConnectorEvent::PendingModified(p) => {
                engine.on_pending_modified(&p).await;
            }
            ConnectorEvent::PendingCancelled { account_id, ticket } => {
                engine.on_pending_cancelled(&account_id, &ticket).await;
            }
            ConnectorEvent::PendingFilled { ticket, account_id, position_ticket } => {
                // Slave pending fills on its own when its broker reaches the
                // target — we don't re-dispatch. But on cTrader the resulting
                // position has a new ID, so migrate the master↔slave mapping
                // onto that ID. The migration also lets the master-side
                // TradeOpened handler recognize "this position is from an
                // already-mirrored pending" and skip the duplicate dispatch.
                if let Some(pid) = position_ticket.as_deref() {
                    self.ticket_map.migrate_ticket(&account_id, &ticket, pid);
                }
                self.emit_log(LogLevel::Info, &account_id,
                    format!("pending {ticket} filled"));
            }
            ConnectorEvent::Symbols { account_id, symbols } => {
                // Preserve the broker's original case — some brokers expose
                // suffixed symbols like "US500.cash" where "US500.CASH" would
                // fail a `MarketInfo` / `SymbolInfoDouble` lookup. We dedupe
                // case-insensitively (stable order) so "EURUSD" vs "eurusd"
                // don't both show up.
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut canon: Vec<String> = Vec::with_capacity(symbols.len());
                for s in symbols {
                    let trimmed = s.trim();
                    if trimmed.is_empty() { continue; }
                    let upper = trimmed.to_uppercase();
                    if seen.insert(upper) { canon.push(trimmed.to_string()); }
                }
                canon.sort_by(|a, b| a.to_ascii_uppercase().cmp(&b.to_ascii_uppercase()));
                self.symbols.insert(account_id.clone(), canon.clone());
                if let Some(h) = self.app_handle.get() {
                    let _ = h.emit(EVT_SYMBOLS, (&account_id, &canon));
                }
            }
        }
    }

    /// Send a list-symbols request to the EA. The reply arrives asynchronously
    /// via `ConnectorEvent::Symbols` and updates `symbols`.
    pub async fn request_symbols(&self, id: &str) -> bool {
        if let Some(h) = self.connector_handle(id) {
            let _ = h.send(ConnectorCmd::ListSymbols).await;
            true
        } else { false }
    }

    /// Replace this account's symbol subscription set and push it to the EA.
    /// Preserves the caller's case so case-sensitive broker tickers
    /// (e.g. `US500.cash`) reach the EA intact. Dedupe is case-insensitive.
    pub async fn set_subscription(&self, id: &str, symbols: Vec<String>) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut canon: Vec<String> = Vec::with_capacity(symbols.len());
        for s in symbols {
            let trimmed = s.trim();
            if trimmed.is_empty() { continue; }
            let upper = trimmed.to_uppercase();
            if seen.insert(upper) { canon.push(trimmed.to_string()); }
        }
        canon.sort_by(|a, b| a.to_ascii_uppercase().cmp(&b.to_ascii_uppercase()));
        self.subscriptions.insert(id.to_string(), canon.clone());
        // Drop any cached quotes for symbols we no longer subscribe to.
        // Quotes are stored with uppercased symbols (see `Quote` arm above),
        // so the keep-set is uppercase to match.
        let keep: std::collections::HashSet<String> = canon.iter().map(|s| s.to_uppercase()).collect();
        self.quotes.retain(|(acc, sym), _| acc != id || keep.contains(sym.as_str()));
        self.quote_last_emit.retain(|(acc, sym), _| acc != id || keep.contains(sym.as_str()));
        if let Some(h) = self.connector_handle(id) {
            let _ = h.send(ConnectorCmd::Subscribe { symbols: canon.clone() }).await;
        }
        canon
    }

    pub async fn connect(self: &Arc<Self>, id: &str) -> Result<()> {
        let account = self.accounts.get(id).map(|a| a.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown account"))?;
        // MT4/MT5 attach automatically via the file-discovery loop.
        if matches!(account.platform, Platform::MT4 | Platform::MT5) { return Ok(()); }
        if self.connectors.contains_key(id) { return Ok(()); }
        let handle = spawn_connector(account, self.event_tx.clone())?;
        self.connectors.insert(id.to_string(), handle.clone());
        // Replay any persisted subscription so the EA resumes streaming.
        if let Some(syms) = self.subscriptions.get(id) {
            let symbols = syms.clone();
            if !symbols.is_empty() {
                let _ = handle.send(ConnectorCmd::Subscribe { symbols }).await;
            }
        }
        Ok(())
    }

    /// Look up an MT account by (platform, login) or create one on the fly.
    /// Used by the MT multiplexer when an EA dials in with an unknown login.
    pub async fn find_or_create_mt_account(
        self: &Arc<Self>, platform: Platform, login: &str, server: &str,
    ) -> Account {
        let existing_id = self.accounts.iter()
            .find(|kv| kv.value().platform == platform && kv.value().login == login)
            .map(|kv| kv.key().clone());
        if let Some(id) = existing_id {
            let mut a = self.accounts.get_mut(&id).unwrap();
            if !server.is_empty() && a.server != server {
                a.server = server.to_string();
                let snapshot = a.clone();
                drop(a);
                self.mark_dirty();
                return snapshot;
            }
            return a.clone();
        }
        let pname = match platform { Platform::MT4 => "MT4", Platform::MT5 => "MT5", _ => "MT" };
        let account = Account {
            id: uuid::Uuid::new_v4().to_string(),
            platform,
            label: format!("{pname} {login}"),
            login: login.to_string(),
            server: server.to_string(),
            role: AccountRole::Idle,
            connected: false,
            balance: 0.0, equity: 0.0,
            currency: "USD".into(),
            password: None,
        };
        self.accounts.insert(account.id.clone(), account.clone());
        self.mark_dirty();
        if let Some(h) = self.app_handle.get() { let _ = h.emit(EVT_ACCOUNT, &account); }
        self.emit_log(LogLevel::Info, &account.id,
            format!("auto-discovered {pname} account {login}"));
        account
    }

    pub fn spawn_mt_discovery(self: &Arc<Self>) {
        crate::connectors::mt_bridge::spawn_discovery(self.clone());
    }

    pub async fn disconnect(&self, id: &str) -> Result<()> {
        if let Some((_, h)) = self.connectors.remove(id) {
            h.shutdown().await;
        }
        self.with_account(id, |a| { a.connected = false; true });
        Ok(())
    }

    pub fn connector_handle(&self, id: &str) -> Option<ConnectorHandle> {
        self.connectors.get(id).map(|h| h.clone())
    }

    pub fn reconnect_all(self: &Arc<Self>) {
        let ids: Vec<String> = self.accounts.iter().map(|kv| kv.key().clone()).collect();
        let this = self.clone();
        tokio::spawn(async move {
            for id in ids {
                if let Err(e) = this.connect(&id).await {
                    this.emit_log(LogLevel::Warn, &id, format!("auto-connect failed: {e}"));
                }
            }
        });
    }

    pub fn spawn_ctrader_discovery(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                tick.tick().await;
                let Some(root) = cascada_root() else { continue };
                let Ok(mut rd) = tokio::fs::read_dir(&root).await else { continue };
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let p = entry.path();
                    if !p.is_dir() { continue; }
                    let Some(login) = p.file_name().and_then(|s| s.to_str()) else { continue };
                    if login.is_empty() { continue; }
                    if !tokio::fs::try_exists(p.join("events.jsonl")).await.unwrap_or(false) { continue; }
                    let already = this.accounts.iter().any(|kv| {
                        kv.value().platform == Platform::CTrader && kv.value().login == login
                    });
                    if already { continue; }
                    let account = Account {
                        id: uuid::Uuid::new_v4().to_string(),
                        platform: Platform::CTrader,
                        label: format!("cTrader {login}"),
                        login: login.to_string(),
                        server: String::new(),
                        role: AccountRole::Idle,
                        connected: false,
                        balance: 0.0, equity: 0.0,
                        currency: "USD".into(),
                        password: None,
                    };
                    let id = account.id.clone();
                    this.accounts.insert(id.clone(), account.clone());
                    this.mark_dirty();
                    if let Some(h) = this.app_handle.get() {
                        let _ = h.emit(EVT_ACCOUNT, &account);
                    }
                    this.emit_log(LogLevel::Info, &id,
                        format!("auto-discovered cTrader account {login}"));
                    let _ = this.connect(&id).await;
                }
            }
        });
    }
}

