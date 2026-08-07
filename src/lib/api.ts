import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Platform = "cTrader" | "MT4" | "MT5" | "TradingView";
export type AccountRole = "Master" | "Slave" | "Idle";

export interface Account {
  id: string;
  platform: Platform;
  label: string;
  login: string;
  server: string;
  role: AccountRole;
  connected: boolean;
  balance: number;
  equity: number;
  currency: string;
}

export type LotMode = "Fixed" | "Multiplier" | "Equity" | "BalanceRatio" | "RiskPercent";
export type DirectionFilter = "All" | "BuyOnly" | "SellOnly";
export type SlTpMode = "Copy" | "Ignore" | "Fixed";

export interface Schedule {
  enabled: boolean;
  start_min: number;   // minutes since midnight
  end_min: number;
  skip_weekends: boolean;
}

export interface CopyRule {
  id: string;
  name: string;
  master_id: string;
  slave_id: string;
  enabled: boolean;
  lot_mode: LotMode;
  lot_value: number;
  reverse: boolean;
  max_slippage_pips: number;
  symbol_map: Record<string, string>;

  /** Clamp applied to the computed slave volume (reshapes the order). */
  min_lot: number;
  max_lot: number;

  /** Master-lot gate: drops the signal when the *master's* lot falls outside
   *  the band, instead of resizing it like min_lot/max_lot. 0 = off. */
  master_min_lot: number;
  master_max_lot: number;

  symbol_whitelist: string[];
  symbol_blacklist: string[];
  symbol_prefix: string;
  symbol_suffix: string;
  master_strip_prefix: string;
  master_strip_suffix: string;

  direction: DirectionFilter;
  comment_filter: string;
  /** EA magic-number filter. Comma-separated `123`, `100-199`, `!123`.
   *  Empty = off. See `CopyRule::magic_filter` in the Rust model. */
  magic_filter: string;
  close_on_master_close: boolean;

  max_open_positions: number;
  max_exposure_lots: number;
  max_daily_loss: number;

  sl_mode: SlTpMode;
  sl_pips: number;
  tp_mode: SlTpMode;
  tp_pips: number;
  trade_delay_ms: number;
  skip_older_than_secs: number;

  trailing_pips: number;
  breakeven_after_pips: number;

  schedule: Schedule;
  pip_value_per_lot: number;

  /** Manual per-symbol SL/TP pip offset entries (captured via the Compare tab). */
  quote_offsets: QuoteOffset[];
}

export interface QuoteOffset {
  symbol: string;  // master-side ticker (uppercased)
  pips: number;    // signed pip shift applied to SL/TP
  /** Optional TV data-feed marker (`OANDA:`, `*PEPPERSTONE`). When set,
   *  the offset only matches trades from that feed; empty = match any. */
  feed?: string;
}

export interface EaStatus {
  platform: Platform;
  path: string;
  up_to_date: boolean;
  installed_bytes: number;
  bundled_bytes: number;
}

/// Status of the Cascada-managed TradingView sidecar (Python + mitmproxy).
/// Returned by the `tv_proxy_*` commands; powers the Accounts › TradingView
/// panel.
export interface TvProxyStatus {
  installed: boolean;        // venv + mitmproxy ready
  running: boolean;          // mitmdump child alive
  port: number;
  pythonPath?: string | null;
  pythonVersion?: string | null;
  addonPath?: string | null;
  venvPath?: string | null;
  certPath?: string | null;  // ~/.mitmproxy/mitmproxy-ca-cert.pem
  /** All preconditions for the bundled-browser flow are met (proxy
   * installed, cert generated + SPKI hashed, Chromium-family browser
   * found on disk). Drives the "Open TradingView" button. */
  browserReady: boolean;
  /** Resolved Chrome/Edge/Brave/Chromium binary; null when none found. */
  browserPath?: string | null;
  lastError?: string | null;
}

export interface Quote {
  account_id: string;
  symbol: string;
  bid: number;
  ask: number;
  /// Broker-reported pip size. 0/undefined when the EA hasn't been upgraded.
  pip_size?: number;
  ts: number;
}

export function defaultRule(master_id = "", slave_id = ""): CopyRule {
  return {
    id: crypto.randomUUID(),
    name: "",
    master_id, slave_id,
    enabled: true,
    lot_mode: "Multiplier", lot_value: 1,
    reverse: false,
    max_slippage_pips: 3,
    symbol_map: {},
    min_lot: 0, max_lot: 0,
    master_min_lot: 0, master_max_lot: 0,
    symbol_whitelist: [], symbol_blacklist: [],
    symbol_prefix: "", symbol_suffix: "",
    master_strip_prefix: "", master_strip_suffix: "",
    direction: "All",
    comment_filter: "",
    magic_filter: "",
    close_on_master_close: true,
    max_open_positions: 0, max_exposure_lots: 0, max_daily_loss: 0,
    sl_mode: "Copy", sl_pips: 0,
    tp_mode: "Copy", tp_pips: 0,
    trade_delay_ms: 0, skip_older_than_secs: 0,
    trailing_pips: 0, breakeven_after_pips: 0,
    schedule: { enabled: false, start_min: 0, end_min: 24 * 60, skip_weekends: false },
    pip_value_per_lot: 10,
    quote_offsets: [],
  };
}

export interface Trade {
  ticket: string;
  account_id: string;
  symbol: string;
  side: "Buy" | "Sell";
  volume: number;
  price: number;
  sl: number | null;
  tp: number | null;
  opened_at: number;
  closed_at: number | null;
  profit: number | null;
  /** EA magic number (MT4/MT5). 0 for manual trades, cTrader and TradingView. */
  magic?: number;
  /** Broker-reported pip size. 0/undefined on pre-v0.1.6 EAs. */
  pip_size?: number;
}

export interface LogEntry {
  id: number;
  ts: number;
  level: "info" | "warn" | "error";
  source: string;
  message: string;
}

export const EVT = {
  log: "cascada://log",
  account: "cascada://account",
  trade: "cascada://trade",
  quote: "cascada://quote",
  symbols: "cascada://symbols",
} as const;

export const api = {
  listAccounts: () => invoke<Account[]>("list_accounts"),
  addAccount: (p: Omit<Account, "connected" | "balance" | "equity" | "currency" | "id"> & { password?: string }) =>
    invoke<Account>("add_account", { payload: p }),
  removeAccount: (id: string) => invoke<void>("remove_account", { id }),
  connectAccount: (id: string) => invoke<void>("connect_account", { id }),
  disconnectAccount: (id: string) => invoke<void>("disconnect_account", { id }),
  setRole: (id: string, role: AccountRole) => invoke<void>("set_role", { id, role }),
  renameAccount: (id: string, label: string) => invoke<void>("rename_account", { id, label }),

  listRules: () => invoke<CopyRule[]>("list_rules"),
  upsertRule: (rule: CopyRule) => invoke<CopyRule>("upsert_rule", { rule }),
  deleteRule: (id: string) => invoke<void>("delete_rule", { id }),

  listTrades: () => invoke<Trade[]>("list_trades"),

  subscribeSymbols: (account_id: string, symbols: string[]) =>
    invoke<string[]>("subscribe_symbols", { accountId: account_id, symbols }),
  listQuotes: () => invoke<Quote[]>("list_quotes"),
  listSubscriptions: () => invoke<[string, string[]][]>("list_subscriptions"),
  requestSymbols: (account_id: string) =>
    invoke<boolean>("request_symbols", { accountId: account_id }),
  listAccountSymbols: (account_id: string) =>
    invoke<string[]>("list_symbols", { accountId: account_id }),

  installCtraderBot: () => invoke<string[]>("install_ctrader_bot"),
  installCtraderBotAt: (path: string) => invoke<string>("install_ctrader_bot_at", { path }),
  installMtEaAt: (platform: "MT4" | "MT5", path: string) =>
    invoke<string>("install_mt_ea_at", { platform, path }),
  installMtEa: (platform: "MT4" | "MT5") =>
    invoke<string[]>("install_mt_ea", { platform }),

  /** Compare installed EAs / cBots with the ones bundled in this build. */
  checkEaVersions: () => invoke<EaStatus[]>("check_ea_versions"),

  /** TradingView sidecar lifecycle. `setup` is idempotent — safe to re-run.
   * `openBrowser` ensures the proxy is running, then spawns an isolated
   * Chrome/Edge window pointed at TradingView (no system proxy changes). */
  tvProxyStatus: () => invoke<TvProxyStatus>("tv_proxy_status"),
  tvProxySetup: () => invoke<TvProxyStatus>("tv_proxy_setup"),
  tvProxyStart: () => invoke<TvProxyStatus>("tv_proxy_start"),
  tvProxyStop: () => invoke<TvProxyStatus>("tv_proxy_stop"),
  tvProxyOpenBrowser: () => invoke<TvProxyStatus>("tv_proxy_open_browser"),

  exportSettings: (path: string) => invoke<string>("export_settings", { path }),
  importSettings: (path: string) =>
    invoke<{ accounts: number; rules: number }>("import_settings", { path }),

  onEvent: (cb: (e: LogEntry) => void): Promise<UnlistenFn> =>
    listen<LogEntry>(EVT.log, (e) => cb(e.payload)),
  onAccountUpdate: (cb: (a: Account) => void): Promise<UnlistenFn> =>
    listen<Account>(EVT.account, (e) => cb(e.payload)),
  onTrade: (cb: (t: Trade) => void): Promise<UnlistenFn> =>
    listen<Trade>(EVT.trade, (e) => cb(e.payload)),
  onQuote: (cb: (q: Quote) => void): Promise<UnlistenFn> =>
    listen<Quote>(EVT.quote, (e) => cb(e.payload)),
  onSymbols: (cb: (account_id: string, symbols: string[]) => void): Promise<UnlistenFn> =>
    listen<[string, string[]]>(EVT.symbols, (e) => cb(e.payload[0], e.payload[1])),
};
