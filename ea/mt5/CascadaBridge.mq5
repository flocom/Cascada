//+------------------------------------------------------------------+
//| CascadaBridge.mq5 — MT5 bridge to Cascada desktop app             |
//| File-based IPC: writes events / reads commands under              |
//|   <TerminalCommonDataPath>/Files/Cascada/MT5/<login>/             |
//| No network, no whitelist — drop the EA on a chart and it works.   |
//+------------------------------------------------------------------+
#property copyright "Cascada"
#property version   "1.00"
#property strict

#include <Trade\Trade.mqh>
#include <Trade\PositionInfo.mqh>
#include <Trade\OrderInfo.mqh>

input int HistoryDays  = 7;     // history snapshot window (days)
input int HistoryMax   = 500;   // max trades emitted in the snapshot
input int PollMs       = 200;   // command-file poll cadence (ms)

string        g_dir;            // "Cascada\MT5\<login>"
string        g_evt_path;
string        g_cmd_path;
int           g_evt_h     = INVALID_HANDLE;   // append-mode handle, kept open
ulong         g_cmd_off   = 0;                // bytes consumed from cmd.jsonl
ulong         g_last_hb   = 0;                // GetTickCount64() ms
CTrade        trade;
CPositionInfo pos;
COrderInfo    ord;
string        g_subs[];          // active quote-stream subscription (uppercased symbols)

const int FFLAGS_RW  = FILE_READ|FILE_WRITE|FILE_BIN|FILE_COMMON|FILE_SHARE_READ|FILE_SHARE_WRITE;
const int FFLAGS_W   = FILE_WRITE|FILE_BIN|FILE_COMMON|FILE_SHARE_READ|FILE_SHARE_WRITE;
const int FFLAGS_R   = FILE_READ|FILE_BIN|FILE_COMMON|FILE_SHARE_READ|FILE_SHARE_WRITE;

//+------------------------------------------------------------------+
int OnInit()
{
   string login = IntegerToString(AccountInfoInteger(ACCOUNT_LOGIN));
   if(StringLen(login) == 0 || login == "0")
   { Print("[Cascada] no account login — not logged in?"); return INIT_FAILED; }

   g_dir      = "Cascada\\MT5\\" + login;
   g_evt_path = g_dir + "\\events.jsonl";
   g_cmd_path = g_dir + "\\cmd.jsonl";

   // Truncate events.jsonl (fresh session) by opening write-only then closing.
   int trunc = FileOpen(g_evt_path, FFLAGS_W);
   if(trunc == INVALID_HANDLE)
   { PrintFormat("[Cascada] cannot create %s (err %d)", g_evt_path, GetLastError()); return INIT_FAILED; }
   FileClose(trunc);

   // Reopen for append; keep handle open for the EA's lifetime.
   g_evt_h = FileOpen(g_evt_path, FFLAGS_RW);
   if(g_evt_h == INVALID_HANDLE)
   { PrintFormat("[Cascada] reopen events failed (err %d)", GetLastError()); return INIT_FAILED; }
   FileSeek(g_evt_h, 0, SEEK_END);

   // Skip any pre-existing commands; only obey new ones.
   int ch = FileOpen(g_cmd_path, FFLAGS_R);
   if(ch != INVALID_HANDLE) { g_cmd_off = (ulong)FileSize(ch); FileClose(ch); }

   trade.SetAsyncMode(false);
   EventSetMillisecondTimer(PollMs);

   WriteWelcome();
   SnapshotAll();
   return INIT_SUCCEEDED;
}

void OnDeinit(const int reason)
{
   EventKillTimer();
   if(g_evt_h != INVALID_HANDLE) { FileClose(g_evt_h); g_evt_h = INVALID_HANDLE; }
}

//+------------------------------------------------------------------+
void OnTimer()
{
   PumpCommands();
   PushHeartbeat();
   PushQuotes();
}

//+------------------------------------------------------------------+
//| MT5 fires this on every trade-server event — much better than poll
//+------------------------------------------------------------------+
void OnTradeTransaction(const MqlTradeTransaction& trans,
                        const MqlTradeRequest&     request,
                        const MqlTradeResult&      result)
{
   if(g_evt_h == INVALID_HANDLE) return;
   switch(trans.type)
   {
      case TRADE_TRANSACTION_DEAL_ADD:
      {
         if(!HistoryDealSelect(trans.deal)) break;
         ENUM_DEAL_TYPE  dtype = (ENUM_DEAL_TYPE)HistoryDealGetInteger(trans.deal, DEAL_TYPE);
         if(dtype == DEAL_TYPE_BALANCE || dtype == DEAL_TYPE_CREDIT
            || dtype == DEAL_TYPE_CHARGE || dtype == DEAL_TYPE_CORRECTION
            || dtype == DEAL_TYPE_BONUS)
         { WriteBalanceDeal(trans.deal, dtype); break; }
         if(dtype != DEAL_TYPE_BUY && dtype != DEAL_TYPE_SELL) break;
         ENUM_DEAL_ENTRY entry = (ENUM_DEAL_ENTRY)HistoryDealGetInteger(trans.deal, DEAL_ENTRY);
         ulong position_id = (ulong)HistoryDealGetInteger(trans.deal, DEAL_POSITION_ID);
         if(entry == DEAL_ENTRY_IN || entry == DEAL_ENTRY_INOUT)
         {
            if(PositionSelectByTicket(position_id)) WriteOpen(position_id);
         }
         else if(entry == DEAL_ENTRY_OUT || entry == DEAL_ENTRY_OUT_BY)
         {
            if(PositionSelectByTicket(position_id)) WriteModify(position_id);
            else WriteCloseFromDeal(trans.deal, position_id);
         }
         break;
      }
      case TRADE_TRANSACTION_POSITION:
         if(PositionSelectByTicket(trans.position)) WriteModify(trans.position);
         break;

      case TRADE_TRANSACTION_ORDER_ADD:
      case TRADE_TRANSACTION_ORDER_UPDATE:
         if(ord.Select(trans.order) && IsPendingType((ENUM_ORDER_TYPE)ord.OrderType()))
            WritePending(trans.type == TRADE_TRANSACTION_ORDER_ADD ? "pending" : "pending_modify",
                         trans.order);
         break;

      case TRADE_TRANSACTION_ORDER_DELETE:
      {
         if(!HistoryOrderSelect(trans.order)) break;
         // MT5 fires ORDER_DELETE for every order leaving the active list,
         // including filled market orders. Only pending types produce the
         // pending_fill / pending_cancel signals the backend expects —
         // skip the rest so market fills don't masquerade as pendings.
         ENUM_ORDER_TYPE otype = (ENUM_ORDER_TYPE)HistoryOrderGetInteger(trans.order, ORDER_TYPE);
         if(!IsPendingType(otype)) break;
         bool filled = (ENUM_ORDER_STATE)HistoryOrderGetInteger(trans.order, ORDER_STATE) == ORDER_STATE_FILLED;
         if(filled)
         {
            ulong position_id = (ulong)HistoryOrderGetInteger(trans.order, ORDER_POSITION_ID);
            WritePendingFill(trans.order, position_id);
         }
         else
         {
            WritePendingEnd("pending_cancel", trans.order);
         }
         break;
      }
   }
}

//+------------------------------------------------------------------+
//| File IO
//+------------------------------------------------------------------+
void AppendBytes(const string s)
{
   if(g_evt_h == INVALID_HANDLE) return;
   uchar bytes[];
   int n = StringToCharArray(s, bytes, 0, WHOLE_ARRAY, CP_UTF8);
   if(n > 0 && bytes[n-1] == 0) n--;
   if(n <= 0) return;
   FileWriteArray(g_evt_h, bytes, 0, n);
   FileFlush(g_evt_h);
}

void WriteEvent(const string ev, const string body)
{
   AppendBytes("{\"ev\":\"" + ev + "\"," + body + "}\n");
}

void WriteLog(const string level, const string msg)
{
   WriteEvent("log", "\"level\":\"" + level + "\",\"message\":\"" + Esc(msg) + "\"");
}

//+------------------------------------------------------------------+
//| Snapshots & periodic events
//+------------------------------------------------------------------+
void WriteWelcome()
{
   string body =
      "\"balance\":"      + F2(AccountInfoDouble(ACCOUNT_BALANCE))      +
      ",\"equity\":"      + F2(AccountInfoDouble(ACCOUNT_EQUITY))       +
      ",\"margin\":"      + F2(AccountInfoDouble(ACCOUNT_MARGIN))       +
      ",\"free_margin\":" + F2(AccountInfoDouble(ACCOUNT_MARGIN_FREE))  +
      ",\"currency\":\""  + Esc(AccountInfoString(ACCOUNT_CURRENCY))    + "\"" +
      ",\"leverage\":"    + IntegerToString(AccountInfoInteger(ACCOUNT_LEVERAGE)) +
      ",\"broker\":\""    + Esc(AccountInfoString(ACCOUNT_COMPANY))     + "\"" +
      ",\"server\":\""    + Esc(AccountInfoString(ACCOUNT_SERVER))      + "\"" +
      ",\"account\":\""   + IntegerToString(AccountInfoInteger(ACCOUNT_LOGIN)) + "\"" +
      ",\"is_live\":"     + (AccountInfoInteger(ACCOUNT_TRADE_MODE) == ACCOUNT_TRADE_MODE_REAL ? "true" : "false");
   WriteEvent("welcome", body);
}

void PushHeartbeat()
{
   ulong now_ms = GetTickCount64();
   if(now_ms - g_last_hb < 1000) return;
   g_last_hb = now_ms;

   double balance = AccountInfoDouble(ACCOUNT_BALANCE);
   double equity  = AccountInfoDouble(ACCOUNT_EQUITY);
   double credit  = AccountInfoDouble(ACCOUNT_CREDIT);
   string body =
      "\"balance\":"       + F2(balance) +
      ",\"equity\":"       + F2(equity)  +
      ",\"margin\":"       + F2(AccountInfoDouble(ACCOUNT_MARGIN))       +
      ",\"free_margin\":"  + F2(AccountInfoDouble(ACCOUNT_MARGIN_FREE))  +
      ",\"margin_level\":" + F2(AccountInfoDouble(ACCOUNT_MARGIN_LEVEL)) +
      ",\"profit\":"       + F2(AccountInfoDouble(ACCOUNT_PROFIT))       +
      ",\"unrealized\":"   + F2(equity - balance - credit) +
      ",\"positions\":"    + IntegerToString(PositionsTotal()) +
      ",\"pending\":"      + IntegerToString(OrdersTotal());
   WriteEvent("heartbeat", body);
}

void SnapshotAll()
{
   // Pre-select full history window so WriteOpen's commission lookup
   // doesn't call HistorySelectByPosition once per position.
   datetime since = TimeCurrent() - HistoryDays * 86400;
   HistorySelect(since, TimeCurrent());
   for(int i = 0, n = PositionsTotal(); i < n; i++)
   {
      ulong t = PositionGetTicket(i);
      if(t != 0) WriteOpen(t);
   }
   for(int i = 0, n = OrdersTotal(); i < n; i++)
   {
      ulong t = OrderGetTicket(i);
      if(t != 0) WritePending("pending", t);
   }
   SnapshotHistory();
}

void SnapshotHistory()
{
   datetime since = TimeCurrent() - HistoryDays * 86400;
   if(!HistorySelect(since, TimeCurrent()))
   { WriteEvent("history_done", "\"count\":0"); return; }
   int total = HistoryDealsTotal();
   int emitted = 0;
   for(int i = total - 1; i >= 0 && emitted < HistoryMax; i--)
   {
      ulong dt = HistoryDealGetTicket(i);
      if(dt == 0) continue;
      ENUM_DEAL_TYPE dtype = (ENUM_DEAL_TYPE)HistoryDealGetInteger(dt, DEAL_TYPE);
      if(dtype != DEAL_TYPE_BUY && dtype != DEAL_TYPE_SELL) continue;
      ENUM_DEAL_ENTRY entry = (ENUM_DEAL_ENTRY)HistoryDealGetInteger(dt, DEAL_ENTRY);
      if(entry != DEAL_ENTRY_OUT && entry != DEAL_ENTRY_OUT_BY) continue;
      WriteHistoryDeal(dt, (ulong)HistoryDealGetInteger(dt, DEAL_POSITION_ID));
      emitted++;
   }
   WriteEvent("history_done", "\"count\":" + IntegerToString(emitted));
}

void WritePong()
{
   WriteEvent("pong", "\"ts\":" + IntegerToString(NowMs()));
}

//+------------------------------------------------------------------+
//| Per-event writers
//+------------------------------------------------------------------+
void WriteOpen(ulong ticket)
{
   if(!PositionSelectByTicket(ticket)) return;
   string sym  = PositionGetString(POSITION_SYMBOL);
   string side = (PositionGetInteger(POSITION_TYPE) == POSITION_TYPE_BUY) ? "Buy" : "Sell";
   string cmt  = PositionGetString(POSITION_COMMENT);
   long ts_ms  = (long)PositionGetInteger(POSITION_TIME_MSC);
   if(ts_ms == 0) ts_ms = (long)PositionGetInteger(POSITION_TIME) * 1000;
   string body =
      "\"ticket\":\""    + IntegerToString((long)ticket) + "\"" +
      ",\"position_id\":\"" + IntegerToString((long)PositionGetInteger(POSITION_IDENTIFIER)) + "\"" +
      ",\"magic\":"      + IntegerToString(PositionGetInteger(POSITION_MAGIC)) +
      ",\"symbol\":\""   + Esc(sym) + "\"" +
      ",\"side\":\""     + side + "\"" +
      ",\"volume\":"     + F2(PositionGetDouble(POSITION_VOLUME)) +
      ",\"price\":"      + F5(PositionGetDouble(POSITION_PRICE_OPEN)) +
      ",\"current\":"    + F5(PositionGetDouble(POSITION_PRICE_CURRENT)) +
      ",\"sl\":"         + F5(PositionGetDouble(POSITION_SL)) +
      ",\"tp\":"         + F5(PositionGetDouble(POSITION_TP)) +
      ",\"commission\":" + F5(EntryCommission(ticket)) +
      ",\"swap\":"       + F5(PositionGetDouble(POSITION_SWAP)) +
      ",\"pip_size\":"   + F5(PipSize(sym)) +
      ",\"comment\":\""  + Esc(cmt) + "\"" +
      ",\"origin\":\""   + Esc(ExtractOrigin(cmt)) + "\"" +
      ",\"ts\":"         + IntegerToString(ts_ms);
   WriteEvent("open", body);
}

double EntryCommission(ulong position_id)
{
   if(!HistorySelectByPosition(position_id)) return 0;
   for(int i = 0, n = HistoryDealsTotal(); i < n; i++)
   {
      ulong d = HistoryDealGetTicket(i);
      if(d == 0) continue;
      if((ENUM_DEAL_ENTRY)HistoryDealGetInteger(d, DEAL_ENTRY) == DEAL_ENTRY_IN)
         return HistoryDealGetDouble(d, DEAL_COMMISSION);
   }
   return 0;
}

void WriteModify(ulong ticket)
{
   if(!PositionSelectByTicket(ticket)) return;
   string body =
      "\"ticket\":\""  + IntegerToString((long)ticket) + "\"" +
      ",\"sl\":"       + F5(PositionGetDouble(POSITION_SL)) +
      ",\"tp\":"       + F5(PositionGetDouble(POSITION_TP)) +
      ",\"volume\":"   + F2(PositionGetDouble(POSITION_VOLUME)) +
      ",\"price\":"    + F5(PositionGetDouble(POSITION_PRICE_OPEN)) +
      ",\"current\":"  + F5(PositionGetDouble(POSITION_PRICE_CURRENT)) +
      ",\"ts\":"       + IntegerToString(NowMs());
   WriteEvent("modify", body);
}

void WriteCloseFromDeal(ulong deal_ticket, ulong position_id)
{
   if(!HistoryDealSelect(deal_ticket)) return;
   double profit = HistoryDealGetDouble(deal_ticket, DEAL_PROFIT);
   double swap   = HistoryDealGetDouble(deal_ticket, DEAL_SWAP);
   double comm   = HistoryDealGetDouble(deal_ticket, DEAL_COMMISSION);
   double price  = HistoryDealGetDouble(deal_ticket, DEAL_PRICE);
   double vol    = HistoryDealGetDouble(deal_ticket, DEAL_VOLUME);
   long   ts_ms  = (long)HistoryDealGetInteger(deal_ticket, DEAL_TIME_MSC);
   if(ts_ms == 0) ts_ms = (long)HistoryDealGetInteger(deal_ticket, DEAL_TIME) * 1000;
   string reason = ReasonToString((ENUM_DEAL_REASON)HistoryDealGetInteger(deal_ticket, DEAL_REASON));
   string body =
      "\"ticket\":\""    + IntegerToString((long)position_id) + "\"" +
      ",\"price\":"      + F5(price) +
      ",\"volume\":"     + F2(vol) +
      ",\"profit\":"     + F2(profit + swap + comm) +
      ",\"gross\":"      + F2(profit) +
      ",\"commission\":" + F2(comm) +
      ",\"swap\":"       + F2(swap) +
      ",\"balance\":"    + F2(AccountInfoDouble(ACCOUNT_BALANCE)) +
      ",\"reason\":\""   + reason + "\"" +
      ",\"ts\":"         + IntegerToString(ts_ms);
   WriteEvent("close", body);
}

void WritePending(const string ev, ulong ticket)
{
   if(!ord.Select(ticket)) return;
   string sym = ord.Symbol();
   ENUM_ORDER_TYPE ot = (ENUM_ORDER_TYPE)ord.OrderType();
   string side = IsBuyOrder(ot) ? "Buy" : "Sell";
   string cmt  = ord.Comment();
   long expiry_s = (long)ord.TimeExpiration();
   string body =
      "\"ticket\":\""    + IntegerToString((long)ticket) + "\"" +
      ",\"magic\":"      + IntegerToString(ord.Magic()) +
      ",\"symbol\":\""   + Esc(sym) + "\"" +
      ",\"side\":\""     + side + "\"" +
      ",\"order_type\":\""+ OrderTypeToString(ot) + "\"" +
      ",\"volume\":"     + F2(ord.VolumeInitial()) +
      ",\"target\":"     + F5(ord.PriceOpen()) +
      ",\"stop_limit\":" + F5(ord.PriceStopLimit()) +
      ",\"sl\":"         + F5(ord.StopLoss()) +
      ",\"tp\":"         + F5(ord.TakeProfit()) +
      ",\"expiry\":"     + IntegerToString(expiry_s * 1000) +
      ",\"comment\":\""  + Esc(cmt) + "\"" +
      ",\"origin\":\""   + Esc(ExtractOrigin(cmt)) + "\"" +
      ",\"ts\":"         + IntegerToString(NowMs());
   WriteEvent(ev, body);
}

void WritePendingEnd(const string ev, ulong ticket)
{
   string sym = "";
   if(HistoryOrderSelect(ticket)) sym = HistoryOrderGetString(ticket, ORDER_SYMBOL);
   string body =
      "\"ticket\":\""  + IntegerToString((long)ticket) + "\"" +
      ",\"symbol\":\"" + Esc(sym) + "\"" +
      ",\"ts\":"       + IntegerToString(NowMs());
   WriteEvent(ev, body);
}

// pending_fill carries the resulting position ID so the backend can migrate
// its master↔slave ticket mapping off the pending ID. On MT5 position_id
// usually equals the pending ticket (hedging accounts reuse the ID), but
// sending it explicitly keeps the protocol uniform with cTrader.
void WritePendingFill(ulong ticket, ulong position_id)
{
   string sym = "";
   if(HistoryOrderSelect(ticket)) sym = HistoryOrderGetString(ticket, ORDER_SYMBOL);
   string body =
      "\"ticket\":\""          + IntegerToString((long)ticket) + "\"" +
      ",\"symbol\":\""         + Esc(sym) + "\"" +
      ",\"position_ticket\":\""+ IntegerToString((long)position_id) + "\"" +
      ",\"ts\":"               + IntegerToString(NowMs());
   WriteEvent("pending_fill", body);
}

void WriteHistoryDeal(ulong deal_ticket, ulong position_id)
{
   double profit = HistoryDealGetDouble(deal_ticket, DEAL_PROFIT);
   double swap   = HistoryDealGetDouble(deal_ticket, DEAL_SWAP);
   double comm   = HistoryDealGetDouble(deal_ticket, DEAL_COMMISSION);
   double close  = HistoryDealGetDouble(deal_ticket, DEAL_PRICE);
   double vol    = HistoryDealGetDouble(deal_ticket, DEAL_VOLUME);
   string sym    = HistoryDealGetString(deal_ticket, DEAL_SYMBOL);
   long   tclose = (long)HistoryDealGetInteger(deal_ticket, DEAL_TIME_MSC);
   if(tclose == 0) tclose = (long)HistoryDealGetInteger(deal_ticket, DEAL_TIME) * 1000;
   ENUM_DEAL_TYPE dt = (ENUM_DEAL_TYPE)HistoryDealGetInteger(deal_ticket, DEAL_TYPE);
   string side = (dt == DEAL_TYPE_BUY) ? "Sell" : "Buy"; // closing deal is opposite side of position
   string cmt  = HistoryDealGetString(deal_ticket, DEAL_COMMENT);
   double entry_px = 0; long topen = 0;
   FindEntryDeal(position_id, entry_px, topen);
   string body =
      "\"ticket\":\""    + IntegerToString((long)position_id) + "\"" +
      ",\"symbol\":\""   + Esc(sym) + "\"" +
      ",\"side\":\""     + side + "\"" +
      ",\"volume\":"     + F2(vol) +
      ",\"entry\":"      + F5(entry_px) +
      ",\"close\":"      + F5(close) +
      ",\"profit\":"     + F2(profit + swap + comm) +
      ",\"gross\":"      + F2(profit) +
      ",\"commission\":" + F2(comm) +
      ",\"swap\":"       + F2(swap) +
      ",\"balance\":"    + F2(AccountInfoDouble(ACCOUNT_BALANCE)) +
      ",\"comment\":\""  + Esc(cmt) + "\"" +
      ",\"origin\":\""   + Esc(ExtractOrigin(cmt)) + "\"" +
      ",\"opened_at\":"  + IntegerToString(topen) +
      ",\"closed_at\":"  + IntegerToString(tclose);
   WriteEvent("history", body);
}

void WriteBalanceDeal(ulong deal_ticket, ENUM_DEAL_TYPE dtype)
{
   double amount = HistoryDealGetDouble(deal_ticket, DEAL_PROFIT);
   string cmt    = HistoryDealGetString(deal_ticket, DEAL_COMMENT);
   string kind   = "Balance";
   if(dtype == DEAL_TYPE_CREDIT)        kind = "Credit";
   else if(dtype == DEAL_TYPE_CHARGE)   kind = "Charge";
   else if(dtype == DEAL_TYPE_CORRECTION) kind = "Correction";
   else if(dtype == DEAL_TYPE_BONUS)    kind = "Bonus";
   WriteLog("info", kind + " " + F2(amount)
            + " (balance=" + F2(AccountInfoDouble(ACCOUNT_BALANCE)) + ")"
            + (StringLen(cmt) > 0 ? " — " + cmt : ""));
}

void FindEntryDeal(ulong position_id, double &price, long &ts_ms)
{
   price = 0; ts_ms = 0;
   for(int i = 0, n = HistoryDealsTotal(); i < n; i++)
   {
      ulong d = HistoryDealGetTicket(i);
      if(d == 0) continue;
      if((ulong)HistoryDealGetInteger(d, DEAL_POSITION_ID) != position_id) continue;
      if((ENUM_DEAL_ENTRY)HistoryDealGetInteger(d, DEAL_ENTRY) == DEAL_ENTRY_IN)
      {
         price = HistoryDealGetDouble(d, DEAL_PRICE);
         ts_ms = (long)HistoryDealGetInteger(d, DEAL_TIME_MSC);
         if(ts_ms == 0) ts_ms = (long)HistoryDealGetInteger(d, DEAL_TIME) * 1000;
         return;
      }
   }
}

//+------------------------------------------------------------------+
//| Inbound command pump
//+------------------------------------------------------------------+
void PumpCommands()
{
   int h = FileOpen(g_cmd_path, FFLAGS_R);
   if(h == INVALID_HANDLE) return;
   ulong size = (ulong)FileSize(h);
   if(size < g_cmd_off) g_cmd_off = 0;            // Cascada truncated → restart
   if(size == g_cmd_off) { FileClose(h); return; }

   FileSeek(h, (long)g_cmd_off, SEEK_SET);
   int nbytes = (int)(size - g_cmd_off);
   uchar buf[];
   ArrayResize(buf, nbytes);
   int read = (int)FileReadArray(h, buf, 0, nbytes);
   FileClose(h);
   if(read <= 0) return;

   // Only consume up to the last newline — partial last line stays for next pump.
   int last_nl = -1;
   for(int i = read - 1; i >= 0; i--) if(buf[i] == 0x0A) { last_nl = i; break; }
   if(last_nl < 0) return;
   g_cmd_off += (ulong)(last_nl + 1);

   int from = 0;
   for(int i = 0; i <= last_nl; i++)
   {
      if(buf[i] == 0x0A)
      {
         if(i > from)
         {
            string line = CharArrayToString(buf, from, i - from, CP_UTF8);
            if(StringLen(line) > 0) HandleCommand(line);
         }
         from = i + 1;
      }
   }
}

void HandleCommand(const string line)
{
   string op = JsonField(line, "op");
   if(op == "")                    return;
   else if(op == "open")           DoOpenMarket(line);
   else if(op == "open_limit")     DoOpenPending(line, true);
   else if(op == "open_stop")      DoOpenPending(line, false);
   else if(op == "close")          DoClose(line);
   else if(op == "close_all")      DoCloseAll(line);
   else if(op == "modify")         DoModify(line);
   else if(op == "modify_pending") DoModifyPending(line);
   else if(op == "cancel")         DoCancel(line);
   else if(op == "cancel_all")     DoCancelAll();
   else if(op == "snapshot")       SnapshotAll();
   else if(op == "ping")           WritePong();
   else if(op == "subscribe")      DoSubscribe(line);
   else if(op == "list_symbols")   DoListSymbols();
   else                            WriteLog("warn", "unknown op: " + op);
}

void DoOpenMarket(const string line)
{
   string sym  = JsonField(line, "symbol");
   string side = JsonField(line, "side");
   double vol  = StringToDouble(JsonField(line, "volume"));
   double sl   = StringToDouble(JsonField(line, "sl"));
   double tp   = StringToDouble(JsonField(line, "tp"));
   int    slip = (int)StringToInteger(JsonField(line, "slippage"));
   string origin = JsonField(line, "ticket");
   if(!EnsureSymbolReady(sym)) return;
   vol = NormalizeVolume(sym, vol);
   if(vol <= 0)                  { WriteLog("error", "bad volume for " + sym); return; }
   sl = NormalizePrice(sym, sl);
   tp = NormalizePrice(sym, tp);
   ENUM_ORDER_TYPE t = (side == "Sell") ? ORDER_TYPE_SELL : ORDER_TYPE_BUY;
   double price = (t == ORDER_TYPE_BUY) ? SymbolInfoDouble(sym, SYMBOL_ASK)
                                        : SymbolInfoDouble(sym, SYMBOL_BID);
   double point = SymbolInfoDouble(sym, SYMBOL_POINT);
   int    pts   = (slip > 0 && point > 0) ? (int)MathRound(slip * PipSize(sym) / point) : 10;
   trade.SetDeviationInPoints(pts);
   trade.SetTypeFillingBySymbol(sym);
   if(!trade.PositionOpen(sym, t, vol, price, sl, tp, "cascada:" + origin))
      WriteLog("error", "open failed " + sym + ": " + IntegerToString(trade.ResultRetcode())
               + " " + trade.ResultComment());
}

void DoOpenPending(const string line, bool is_limit)
{
   string sym  = JsonField(line, "symbol");
   string side = JsonField(line, "side");
   double vol  = StringToDouble(JsonField(line, "volume"));
   double tgt  = StringToDouble(JsonField(line, "target"));
   double sl   = StringToDouble(JsonField(line, "sl"));
   double tp   = StringToDouble(JsonField(line, "tp"));
   long expiry_ms = StringToInteger(JsonField(line, "expiry"));
   string origin = JsonField(line, "ticket");
   if(!EnsureSymbolReady(sym)) return;
   vol = NormalizeVolume(sym, vol);
   if(tgt <= 0 || vol <= 0)        { WriteLog("error", "bad pending params for " + sym); return; }
   tgt = NormalizePrice(sym, tgt);
   sl  = NormalizePrice(sym, sl);
   tp  = NormalizePrice(sym, tp);
   ENUM_ORDER_TYPE_TIME tt = (expiry_ms > 0) ? ORDER_TIME_SPECIFIED : ORDER_TIME_GTC;
   datetime expiry_dt = (expiry_ms > 0) ? (datetime)(expiry_ms / 1000) : (datetime)0;
   string cmt = "cascada:" + origin;
   trade.SetTypeFillingBySymbol(sym);
   bool ok;
   if(side == "Sell")
      ok = is_limit ? trade.SellLimit(vol, tgt, sym, sl, tp, tt, expiry_dt, cmt)
                    : trade.SellStop (vol, tgt, sym, sl, tp, tt, expiry_dt, cmt);
   else
      ok = is_limit ? trade.BuyLimit (vol, tgt, sym, sl, tp, tt, expiry_dt, cmt)
                    : trade.BuyStop  (vol, tgt, sym, sl, tp, tt, expiry_dt, cmt);
   if(!ok) WriteLog("error", "pending failed " + sym + ": "
                    + IntegerToString(trade.ResultRetcode()) + " " + trade.ResultComment());
}

void DoClose(const string line)
{
   ulong ticket = (ulong)StringToInteger(JsonField(line, "ticket"));
   double vol   = StringToDouble(JsonField(line, "volume"));
   if(!PositionSelectByTicket(ticket)) { WriteLog("warn", "close: ticket not found"); return; }
   bool ok = (vol > 0 && vol < PositionGetDouble(POSITION_VOLUME))
             ? trade.PositionClosePartial(ticket, vol)
             : trade.PositionClose(ticket);
   if(!ok) WriteLog("error", "close failed: " + trade.ResultComment());
}

void DoCloseAll(const string line)
{
   string only = JsonField(line, "symbol");
   for(int i = PositionsTotal() - 1; i >= 0; i--)
   {
      if(!pos.SelectByIndex(i)) continue;
      if(StringLen(only) > 0 && pos.Symbol() != only) continue;
      if(!trade.PositionClose(pos.Ticket()))
         WriteLog("warn", "close_all " + IntegerToString((long)pos.Ticket())
                  + ": " + trade.ResultComment());
   }
}

void DoModify(const string line)
{
   ulong ticket = (ulong)StringToInteger(JsonField(line, "ticket"));
   double sl = StringToDouble(JsonField(line, "sl"));
   double tp = StringToDouble(JsonField(line, "tp"));
   if(!PositionSelectByTicket(ticket)) { WriteLog("warn", "modify: ticket not found"); return; }
   string sym = PositionGetString(POSITION_SYMBOL);
   double new_sl = (sl > 0) ? NormalizePrice(sym, sl) : PositionGetDouble(POSITION_SL);
   double new_tp = (tp > 0) ? NormalizePrice(sym, tp) : PositionGetDouble(POSITION_TP);
   if(!trade.PositionModify(ticket, new_sl, new_tp))
      WriteLog("error", "modify failed: " + trade.ResultComment());
}

void DoModifyPending(const string line)
{
   ulong ticket = (ulong)StringToInteger(JsonField(line, "ticket"));
   double tgt = StringToDouble(JsonField(line, "target"));
   double sl  = StringToDouble(JsonField(line, "sl"));
   double tp  = StringToDouble(JsonField(line, "tp"));
   long expiry_ms = StringToInteger(JsonField(line, "expiry"));
   if(!ord.Select(ticket)) { WriteLog("warn", "modify_pending: ticket not found"); return; }
   string sym = ord.Symbol();
   double new_tgt = (tgt > 0) ? NormalizePrice(sym, tgt) : ord.PriceOpen();
   double new_sl  = (sl > 0)  ? NormalizePrice(sym, sl)  : ord.StopLoss();
   double new_tp  = (tp > 0)  ? NormalizePrice(sym, tp)  : ord.TakeProfit();
   ENUM_ORDER_TYPE_TIME tt = (expiry_ms > 0) ? ORDER_TIME_SPECIFIED : (ENUM_ORDER_TYPE_TIME)ord.TypeTime();
   datetime expiry_dt = (expiry_ms > 0) ? (datetime)(expiry_ms / 1000) : ord.TimeExpiration();
   if(!trade.OrderModify(ticket, new_tgt, new_sl, new_tp, tt, expiry_dt))
      WriteLog("error", "pending modify failed: " + trade.ResultComment());
}

void DoCancel(const string line)
{
   ulong ticket = (ulong)StringToInteger(JsonField(line, "ticket"));
   if(!trade.OrderDelete(ticket))
      WriteLog("error", "cancel failed: " + trade.ResultComment());
}

void DoCancelAll()
{
   for(int i = OrdersTotal() - 1; i >= 0; i--)
   {
      ulong t = OrderGetTicket(i);
      if(t == 0) continue;
      if(!trade.OrderDelete(t))
         WriteLog("warn", "cancel_all " + IntegerToString((long)t)
                  + ": " + trade.ResultComment());
   }
}

void DoSubscribe(const string line)
{
   ArrayResize(g_subs, 0);
   string list[];
   int n = JsonStringArray(line, "symbols", list);
   for(int i = 0; i < n; i++)
   {
      string s = list[i];
      StringTrimLeft(s); StringTrimRight(s);
      if(StringLen(s) == 0) continue;
      SymbolSelect(s, true);
      int k = ArraySize(g_subs);
      ArrayResize(g_subs, k + 1);
      g_subs[k] = s;
   }
}

// Dump the broker's full symbol list (Market Watch + hidden) as one event.
void DoListSymbols()
{
   string body = "\"symbols\":[";
   bool first = true;
   int total = SymbolsTotal(false);   // false = all known, not just Market Watch
   for(int i = 0; i < total; i++)
   {
      string name = SymbolName(i, false);
      if(StringLen(name) == 0) continue;
      if(!first) body += ",";
      body += "\"" + Esc(name) + "\"";
      first = false;
   }
   body += "]";
   WriteEvent("symbols", body);
}

void PushQuotes()
{
   int n = ArraySize(g_subs);
   if(n == 0) return;
   long now = NowMs();
   MqlTick tick;
   for(int i = 0; i < n; i++)
   {
      string sym = g_subs[i];
      if(!SymbolInfoTick(sym, tick)) continue;
      if(tick.bid <= 0 || tick.ask <= 0) continue;
      string body =
         "\"symbol\":\""  + Esc(sym) + "\"" +
         ",\"bid\":"      + F5(tick.bid) +
         ",\"ask\":"      + F5(tick.ask) +
         ",\"pip_size\":" + F5(PipSize(sym)) +
         ",\"ts\":"       + IntegerToString(now);
      WriteEvent("quote", body);
   }
}

//+------------------------------------------------------------------+
//| Helpers
//+------------------------------------------------------------------+
long NowMs()
{
   long t = (long)TimeTradeServer();
   if(t == 0) t = (long)TimeCurrent();
   return t * 1000;
}

double PipSize(const string sym)
{
   double point  = SymbolInfoDouble(sym, SYMBOL_POINT);
   int    digits = (int)SymbolInfoInteger(sym, SYMBOL_DIGITS);
   return (digits == 3 || digits == 5) ? point * 10.0 : point;
}

double NormalizeVolume(const string sym, double vol)
{
   double step = SymbolInfoDouble(sym, SYMBOL_VOLUME_STEP);
   double vmin = SymbolInfoDouble(sym, SYMBOL_VOLUME_MIN);
   double vmax = SymbolInfoDouble(sym, SYMBOL_VOLUME_MAX);
   if(step <= 0) step = 0.01;
   double rounded = MathRound(vol / step) * step;
   if(rounded < vmin) rounded = vmin;
   if(vmax > 0 && rounded > vmax) rounded = vmax;
   return NormalizeDouble(rounded, 2);
}

double NormalizePrice(const string sym, double price)
{
   if(price == 0) return 0;
   int digits = (int)SymbolInfoInteger(sym, SYMBOL_DIGITS);
   return NormalizeDouble(price, digits);
}

bool EnsureSymbolReady(const string sym)
{
   if(!SymbolSelect(sym, true)) { WriteLog("error", "unknown symbol " + sym); return false; }
   MqlTick tick;
   if(!SymbolInfoTick(sym, tick) || tick.time == 0 || tick.bid <= 0 || tick.ask <= 0)
   { WriteLog("error", "no tick for " + sym + " (symbol not streaming)"); return false; }
   return true;
}

bool IsPendingType(ENUM_ORDER_TYPE t)
{
   return t == ORDER_TYPE_BUY_LIMIT      || t == ORDER_TYPE_SELL_LIMIT
       || t == ORDER_TYPE_BUY_STOP       || t == ORDER_TYPE_SELL_STOP
       || t == ORDER_TYPE_BUY_STOP_LIMIT || t == ORDER_TYPE_SELL_STOP_LIMIT;
}

bool IsBuyOrder(ENUM_ORDER_TYPE t)
{
   return t == ORDER_TYPE_BUY || t == ORDER_TYPE_BUY_LIMIT
       || t == ORDER_TYPE_BUY_STOP || t == ORDER_TYPE_BUY_STOP_LIMIT;
}

string OrderTypeToString(ENUM_ORDER_TYPE t)
{
   switch(t)
   {
      case ORDER_TYPE_BUY:             return "Market";
      case ORDER_TYPE_SELL:            return "Market";
      case ORDER_TYPE_BUY_LIMIT:       return "Limit";
      case ORDER_TYPE_SELL_LIMIT:      return "Limit";
      case ORDER_TYPE_BUY_STOP:        return "Stop";
      case ORDER_TYPE_SELL_STOP:       return "Stop";
      case ORDER_TYPE_BUY_STOP_LIMIT:  return "StopLimit";
      case ORDER_TYPE_SELL_STOP_LIMIT: return "StopLimit";
   }
   return "Unknown";
}

string ReasonToString(ENUM_DEAL_REASON r)
{
   switch(r)
   {
      case DEAL_REASON_CLIENT:   return "Client";
      case DEAL_REASON_MOBILE:   return "Mobile";
      case DEAL_REASON_WEB:      return "Web";
      case DEAL_REASON_EXPERT:   return "Expert";
      case DEAL_REASON_SL:       return "StopLoss";
      case DEAL_REASON_TP:       return "TakeProfit";
      case DEAL_REASON_SO:       return "StopOut";
      case DEAL_REASON_ROLLOVER: return "Rollover";
      case DEAL_REASON_VMARGIN:  return "VMargin";
      case DEAL_REASON_SPLIT:    return "Split";
   }
   return "Unknown";
}

string ExtractOrigin(const string comment)
{
   if(StringLen(comment) >= 8 && StringFind(comment, "cascada:") == 0)
      return StringSubstr(comment, 8);
   return "";
}

string F2(double d) { return DoubleToString(d, 2); }
string F5(double d) { return DoubleToString(d, 5); }

//+------------------------------------------------------------------+
//| Minimal JSON helpers (string-based, no allocation library)
//+------------------------------------------------------------------+
string JsonField(const string s, const string key)
{
   string needle = "\"" + key + "\":";
   int i = StringFind(s, needle);
   if(i < 0) return "";
   i += StringLen(needle);
   int len = StringLen(s);
   while(i < len && (StringGetCharacter(s, i) == ' ' || StringGetCharacter(s, i) == '"')) i++;
   int end = i;
   while(end < len)
   {
      ushort c = StringGetCharacter(s, end);
      if(c == ',' || c == '}' || c == '"') break;
      end++;
   }
   return StringSubstr(s, i, end - i);
}

// Parse a JSON array of strings: returns the count and fills `out`.
int JsonStringArray(const string s, const string key, string &out[])
{
   ArrayResize(out, 0);
   string needle = "\"" + key + "\":";
   int i = StringFind(s, needle);
   if(i < 0) return 0;
   i += StringLen(needle);
   int len = StringLen(s);
   while(i < len && StringGetCharacter(s, i) == ' ') i++;
   if(i >= len || StringGetCharacter(s, i) != '[') return 0;
   i++;
   while(i < len)
   {
      ushort c = StringGetCharacter(s, i);
      if(c == ']') break;
      if(c == ' ' || c == ',') { i++; continue; }
      if(c == '"')
      {
         i++;
         int start = i;
         while(i < len)
         {
            ushort cc = StringGetCharacter(s, i);
            if(cc == '\\' && i + 1 < len) { i += 2; continue; }
            if(cc == '"') break;
            i++;
         }
         int k = ArraySize(out);
         ArrayResize(out, k + 1);
         out[k] = StringSubstr(s, start, i - start);
         if(i < len) i++;
      }
      else i++;
   }
   return ArraySize(out);
}

string Esc(const string s)
{
   int n = StringLen(s);
   if(n == 0) return "";
   bool clean = true;
   for(int i = 0; i < n; i++)
   {
      ushort c = StringGetCharacter(s, i);
      if(c == '\\' || c == '"' || c < 0x20) { clean = false; break; }
   }
   if(clean) return s;
   string out = "";
   for(int i = 0; i < n; i++)
   {
      ushort c = StringGetCharacter(s, i);
      if(c == '\\')      out += "\\\\";
      else if(c == '"')  out += "\\\"";
      else if(c == '\n') out += "\\n";
      else if(c == '\r') out += "\\r";
      else if(c == '\t') out += "\\t";
      else if(c < 0x20)  out += StringFormat("\\u%04x", c);
      else               out += ShortToString(c);
   }
   return out;
}
