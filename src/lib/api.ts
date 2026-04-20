import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Platform = "cTrader" | "MT4" | "MT5";
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

  min_lot: number;
  max_lot: number;

  symbol_whitelist: string[];
  symbol_blacklist: string[];
  symbol_prefix: string;
  symbol_suffix: string;

  direction: DirectionFilter;
  comment_filter: string;

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

  quote_compensate: boolean;
  quote_skip_pips: number;
  /** Deprecated; kept for back-compat deserialization. */
  quote_compensate_symbols: string[];
  /** Manual per-symbol SL/TP pip offset entries. */
  quote_offsets: QuoteOffset[];
}

export interface QuoteOffset {
  symbol: string;  // master-side ticker (uppercased)
  pips: number;    // signed pip shift applied to SL/TP
}

export interface EaStatus {
  platform: Platform;
  path: string;
  up_to_date: boolean;
  installed_bytes: number;
  bundled_bytes: number;
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
    symbol_whitelist: [], symbol_blacklist: [],
    symbol_prefix: "", symbol_suffix: "",
    direction: "All",
    comment_filter: "",
    max_open_positions: 0, max_exposure_lots: 0, max_daily_loss: 0,
    sl_mode: "Copy", sl_pips: 0,
    tp_mode: "Copy", tp_pips: 0,
    trade_delay_ms: 0, skip_older_than_secs: 0,
    trailing_pips: 0, breakeven_after_pips: 0,
    schedule: { enabled: false, start_min: 0, end_min: 24 * 60, skip_weekends: false },
    pip_value_per_lot: 10,
    quote_compensate: false,
    quote_skip_pips: 0,
    quote_compensate_symbols: [],
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
