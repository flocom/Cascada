//+------------------------------------------------------------------+
//| CascadaBridge.mq4 — MT4 bridge to Cascada desktop app             |
//| File-based IPC: writes events / reads commands under              |
//|   <TerminalCommonDataPath>/Files/Cascada/MT4/<login>/             |
//| No DLL, no network, no whitelist.                                 |
//+------------------------------------------------------------------+
#property copyright "Cascada"
#property version   "1.00"
#property strict

input int HistoryDays  = 7;
input int HistoryMax   = 500;
input int PollMs       = 250;

string g_dir;
string g_evt_path;
string g_cmd_path;
int    g_evt_h   = INVALID_HANDLE;
ulong  g_cmd_off = 0;
datetime g_last_hb = 0;

// State for diff-based open/close/modify detection (MT4 has no OnTradeTransaction).
struct TrackedOrder { int ticket; int type; double lots; double sl; double tp; string symbol; };
TrackedOrder g_tracked[];
string       g_subs[];          // active quote-stream subscription

const int FFLAGS_RW = FILE_READ|FILE_WRITE|FILE_BIN|FILE_COMMON|FILE_SHARE_READ|FILE_SHARE_WRITE;
const int FFLAGS_W  = FILE_WRITE|FILE_BIN|FILE_COMMON|FILE_SHARE_READ|FILE_SHARE_WRITE;
const int FFLAGS_R  = FILE_READ|FILE_BIN|FILE_COMMON|FILE_SHARE_READ|FILE_SHARE_WRITE;

//+------------------------------------------------------------------+
int OnInit()
{
   string login = IntegerToString(AccountNumber());
   if(StringLen(login) == 0 || login == "0")
   { Print("[Cascada] no account login"); return INIT_FAILED; }

   g_dir      = "Cascada\\MT4\\" + login;
   g_evt_path = g_dir + "\\events.jsonl";
   g_cmd_path = g_dir + "\\cmd.jsonl";

   int trunc = FileOpen(g_evt_path, FFLAGS_W);
   if(trunc == INVALID_HANDLE)
   { PrintFormat("[Cascada] cannot create %s (err %d)", g_evt_path, GetLastError()); return INIT_FAILED; }
   FileClose(trunc);

   g_evt_h = FileOpen(g_evt_path, FFLAGS_RW);
   if(g_evt_h == INVALID_HANDLE)
   { PrintFormat("[Cascada] reopen events failed (err %d)", GetLastError()); return INIT_FAILED; }
   FileSeek(g_evt_h, 0, SEEK_END);

   int ch = FileOpen(g_cmd_path, FFLAGS_R);
   if(ch != INVALID_HANDLE) { g_cmd_off = (ulong)FileSize(ch); FileClose(ch); }

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

void OnTimer()
{
   PumpCommands();
   PushHeartbeat();
   SyncOrderState();
   PushQuotes();
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
      "\"balance\":"      + F2(AccountBalance())   +
      ",\"equity\":"      + F2(AccountEquity())    +
      ",\"margin\":"      + F2(AccountMargin())    +
      ",\"free_margin\":" + F2(AccountFreeMargin())+
      ",\"currency\":\""  + Esc(AccountCurrency()) + "\"" +
      ",\"leverage\":"    + IntegerToString(AccountLeverage()) +
      ",\"broker\":\""    + Esc(AccountCompany())  + "\"" +
      ",\"server\":\""    + Esc(AccountServer())   + "\"" +
      ",\"account\":\""   + IntegerToString(AccountNumber()) + "\"" +
      ",\"is_live\":"     + (IsDemo() ? "false" : "true");
   WriteEvent("welcome", body);
}

void PushHeartbeat()
{
   datetime now = TimeCurrent();
   if(now - g_last_hb < 1) return;
   g_last_hb = now;

   double balance = AccountBalance();
   double equity  = AccountEquity();
   double credit  = AccountCredit();
   double margin  = AccountMargin();
   double margin_level = (margin > 0) ? equity / margin * 100.0 : 0.0;

   int positions = 0, pending = 0;
   for(int i = 0; i < OrdersTotal(); i++)
   {
      if(!OrderSelect(i, SELECT_BY_POS, MODE_TRADES)) continue;
      if(OrderType() <= OP_SELL) positions++; else pending++;
   }

   string body =
      "\"balance\":"       + F2(balance) +
      ",\"equity\":"       + F2(equity)  +
      ",\"margin\":"       + F2(margin) +
      ",\"free_margin\":"  + F2(AccountFreeMargin()) +
      ",\"margin_level\":" + F2(margin_level) +
      ",\"profit\":"       + F2(AccountProfit()) +
      ",\"unrealized\":"   + F2(equity - balance - credit) +
      ",\"positions\":"    + IntegerToString(positions) +
      ",\"pending\":"      + IntegerToString(pending);
   WriteEvent("heartbeat", body);
}

void SnapshotAll()
{
   ArrayResize(g_tracked, 0);
   for(int i = 0; i < OrdersTotal(); i++)
   {
      if(!OrderSelect(i, SELECT_BY_POS, MODE_TRADES)) continue;
      if(OrderType() <= OP_SELL) { WriteOpen(); TrackPush(); }
      else                       { WritePending("pending"); }
   }
   SnapshotHistory();
}

void WritePong()
{
   WriteEvent("pong", "\"ts\":" + IntegerToString(NowMs()));
}

void SnapshotHistory()
{
   datetime since = TimeCurrent() - HistoryDays * 86400;
   int total = OrdersHistoryTotal();
   int emitted = 0;
   for(int i = total - 1; i >= 0 && emitted < HistoryMax; i--)
   {
      if(!OrderSelect(i, SELECT_BY_POS, MODE_HISTORY)) continue;
      if(OrderCloseTime() < since) continue;
      if(OrderType() > OP_SELL) continue;        // skip pending cancellations
      WriteHistoryClosed();
      emitted++;
   }
   WriteEvent("history_done", "\"count\":" + IntegerToString(emitted));
}

//+------------------------------------------------------------------+
//| Diff-based event emission (replaces MT5's OnTradeTransaction)
//+------------------------------------------------------------------+
void SyncOrderState()
{
   // Build current snapshot (positions only — pending diffing follows below).
   int now_tickets[]; ArrayResize(now_tickets, OrdersTotal());
   int n_now = 0;

   for(int i = 0; i < OrdersTotal(); i++)
   {
      if(!OrderSelect(i, SELECT_BY_POS, MODE_TRADES)) continue;
      if(OrderType() > OP_SELL)
      {
         // Pending: emit modify only when target/sl/tp/lots change.
         int idx = TrackFind(OrderTicket());
         if(idx < 0)
         {
            WritePending("pending");
            TrackPush();
         }
         else if(g_tracked[idx].sl != OrderStopLoss()
              || g_tracked[idx].tp != OrderTakeProfit()
              || g_tracked[idx].lots != OrderLots())
         {
            WritePending("pending_modify");
            g_tracked[idx].sl = OrderStopLoss();
            g_tracked[idx].tp = OrderTakeProfit();
            g_tracked[idx].lots = OrderLots();
         }
         now_tickets[n_now++] = OrderTicket();
         continue;
      }

      now_tickets[n_now++] = OrderTicket();
      int idx = TrackFind(OrderTicket());
      if(idx < 0)
      {
         WriteOpen();
         TrackPush();
      }
      else if(g_tracked[idx].sl != OrderStopLoss()
           || g_tracked[idx].tp != OrderTakeProfit()
           || g_tracked[idx].lots != OrderLots())
      {
         WriteModify();
         g_tracked[idx].sl   = OrderStopLoss();
         g_tracked[idx].tp   = OrderTakeProfit();
         g_tracked[idx].lots = OrderLots();
      }
   }

   // Vanished tickets → check history to disambiguate close vs cancel.
   for(int i = ArraySize(g_tracked) - 1; i >= 0; i--)
   {
      bool still = false;
      for(int j = 0; j < n_now; j++) if(now_tickets[j] == g_tracked[i].ticket) { still = true; break; }
      if(still) continue;
      EmitVanished(g_tracked[i]);
      TrackRemove(i);
   }
}

// MT4 has no ArrayRemove — shift tail left and shrink.
void TrackRemove(int idx)
{
   int n = ArraySize(g_tracked);
   if(idx < 0 || idx >= n) return;
   for(int k = idx; k < n - 1; k++) g_tracked[k] = g_tracked[k + 1];
   ArrayResize(g_tracked, n - 1);
}

void EmitVanished(TrackedOrder &t)
{
   if(OrderSelect(t.ticket, SELECT_BY_TICKET, MODE_HISTORY))
   {
      bool was_pending = (t.type > OP_SELL);
      bool closed_as_position = (OrderType() <= OP_SELL);
      // Pending → if the resulting history entry is a market position, it filled.
      if(was_pending && !closed_as_position) { WritePendingEnd("pending_cancel", t.ticket); return; }
      if(was_pending && closed_as_position)  { WritePendingEnd("pending_fill",   t.ticket); return; }
      WriteCloseFromHistory(t.ticket);
   }
   else
   {
      WriteEvent("close", "\"ticket\":\"" + IntegerToString(t.ticket)
                 + "\",\"profit\":0.0,\"ts\":" + IntegerToString(NowMs()));
   }
}

int  TrackFind(int ticket)
{
   for(int i = 0; i < ArraySize(g_tracked); i++) if(g_tracked[i].ticket == ticket) return i;
   return -1;
}
void TrackPush()
{
   int n = ArraySize(g_tracked);
   ArrayResize(g_tracked, n + 1);
   g_tracked[n].ticket = OrderTicket();
   g_tracked[n].type   = OrderType();
   g_tracked[n].lots   = OrderLots();
   g_tracked[n].sl     = OrderStopLoss();
   g_tracked[n].tp     = OrderTakeProfit();
   g_tracked[n].symbol = OrderSymbol();
}

//+------------------------------------------------------------------+
//| Per-event writers (assume OrderSelect already called)
//+------------------------------------------------------------------+
void WriteOpen()
{
   string sym  = OrderSymbol();
   string side = (OrderType() == OP_BUY) ? "Buy" : "Sell";
   string cmt  = OrderComment();
   string body =
      "\"ticket\":\""    + IntegerToString(OrderTicket()) + "\"" +
      ",\"magic\":"      + IntegerToString(OrderMagicNumber()) +
      ",\"symbol\":\""   + Esc(sym) + "\"" +
      ",\"side\":\""     + side + "\"" +
      ",\"volume\":"     + F2(OrderLots()) +
      ",\"price\":"      + F5(OrderOpenPrice()) +
      ",\"sl\":"         + F5(OrderStopLoss()) +
      ",\"tp\":"         + F5(OrderTakeProfit()) +
      ",\"commission\":" + F5(OrderCommission()) +
      ",\"swap\":"       + F5(OrderSwap()) +
      ",\"pip_size\":"   + F5(PipSize(sym)) +
      ",\"comment\":\""  + Esc(cmt) + "\"" +
      ",\"origin\":\""   + Esc(ExtractOrigin(cmt)) + "\"" +
      ",\"ts\":"         + IntegerToString((long)OrderOpenTime() * 1000);
   WriteEvent("open", body);
}

void WriteModify()
{
   string body =
      "\"ticket\":\""  + IntegerToString(OrderTicket()) + "\"" +
      ",\"sl\":"       + F5(OrderStopLoss()) +
      ",\"tp\":"       + F5(OrderTakeProfit()) +
      ",\"volume\":"   + F2(OrderLots()) +
      ",\"price\":"    + F5(OrderOpenPrice()) +
      ",\"ts\":"       + IntegerToString(NowMs());
   WriteEvent("modify", body);
}

void WriteCloseFromHistory(int ticket)
{
   string body =
      "\"ticket\":\""    + IntegerToString(ticket) + "\"" +
      ",\"price\":"      + F5(OrderClosePrice()) +
      ",\"volume\":"     + F2(OrderLots()) +
      ",\"profit\":"     + F2(OrderProfit() + OrderSwap() + OrderCommission()) +
      ",\"gross\":"      + F2(OrderProfit()) +
      ",\"commission\":" + F2(OrderCommission()) +
      ",\"swap\":"       + F2(OrderSwap()) +
      ",\"balance\":"    + F2(AccountBalance()) +
      ",\"reason\":\"\""  +
      ",\"ts\":"         + IntegerToString((long)OrderCloseTime() * 1000);
   WriteEvent("close", body);
}

void WritePending(const string ev)
{
   string sym = OrderSymbol();
   int t = OrderType();
   string side = (t == OP_BUYLIMIT || t == OP_BUYSTOP) ? "Buy" : "Sell";
   string otype = (t == OP_BUYLIMIT || t == OP_SELLLIMIT) ? "Limit" : "Stop";
   string cmt = OrderComment();
   string body =
      "\"ticket\":\""    + IntegerToString(OrderTicket()) + "\"" +
      ",\"magic\":"      + IntegerToString(OrderMagicNumber()) +
      ",\"symbol\":\""   + Esc(sym) + "\"" +
      ",\"side\":\""     + side + "\"" +
      ",\"order_type\":\""+ otype + "\"" +
      ",\"volume\":"     + F2(OrderLots()) +
      ",\"target\":"     + F5(OrderOpenPrice()) +
      ",\"sl\":"         + F5(OrderStopLoss()) +
      ",\"tp\":"         + F5(OrderTakeProfit()) +
      ",\"expiry\":"     + IntegerToString((long)OrderExpiration() * 1000) +
      ",\"comment\":\""  + Esc(cmt) + "\"" +
      ",\"origin\":\""   + Esc(ExtractOrigin(cmt)) + "\"" +
      ",\"ts\":"         + IntegerToString(NowMs());
   WriteEvent(ev, body);
}

void WritePendingEnd(const string ev, int ticket)
{
   string sym = "";
   if(OrderSelect(ticket, SELECT_BY_TICKET, MODE_HISTORY)) sym = OrderSymbol();
   string body =
      "\"ticket\":\""  + IntegerToString(ticket) + "\"" +
      ",\"symbol\":\"" + Esc(sym) + "\"" +
      ",\"ts\":"       + IntegerToString(NowMs());
   WriteEvent(ev, body);
}

void WriteHistoryClosed()
{
   string sym  = OrderSymbol();
   string side = (OrderType() == OP_BUY) ? "Buy" : "Sell";
   string cmt  = OrderComment();
   string body =
      "\"ticket\":\""    + IntegerToString(OrderTicket()) + "\"" +
      ",\"symbol\":\""   + Esc(sym) + "\"" +
      ",\"side\":\""     + side + "\"" +
      ",\"volume\":"     + F2(OrderLots()) +
      ",\"entry\":"      + F5(OrderOpenPrice()) +
      ",\"close\":"      + F5(OrderClosePrice()) +
      ",\"profit\":"     + F2(OrderProfit() + OrderSwap() + OrderCommission()) +
      ",\"gross\":"      + F2(OrderProfit()) +
      ",\"commission\":" + F2(OrderCommission()) +
      ",\"swap\":"       + F2(OrderSwap()) +
      ",\"balance\":"    + F2(AccountBalance()) +
      ",\"comment\":\""  + Esc(cmt) + "\"" +
      ",\"origin\":\""   + Esc(ExtractOrigin(cmt)) + "\"" +
      ",\"opened_at\":"  + IntegerToString((long)OrderOpenTime() * 1000) +
      ",\"closed_at\":"  + IntegerToString((long)OrderCloseTime() * 1000);
   WriteEvent("history", body);
}

//+------------------------------------------------------------------+
//| Inbound command pump
//+------------------------------------------------------------------+
void PumpCommands()
{
   int h = FileOpen(g_cmd_path, FFLAGS_R);
   if(h == INVALID_HANDLE) return;
   ulong size = (ulong)FileSize(h);
   if(size < g_cmd_off) g_cmd_off = 0;
   if(size == g_cmd_off) { FileClose(h); return; }

   FileSeek(h, (long)g_cmd_off, SEEK_SET);
   int nbytes = (int)(size - g_cmd_off);
   uchar buf[];
   ArrayResize(buf, nbytes);
   int read = (int)FileReadArray(h, buf, 0, nbytes);
   FileClose(h);
   if(read <= 0) return;

   int last_nl = -1;
   for(int i = read - 1; i >= 0; i--) if(buf[i] == 0x0A) { last_nl = i; break; }
   if(last_nl < 0) return;
   g_cmd_off += (ulong)(last_nl + 1);

   string chunk = CharArrayToString(buf, 0, last_nl + 1, CP_UTF8);
   int from = 0, len = StringLen(chunk);
   for(int i = 0; i <= len; i++)
   {
      if(i == len || StringGetCharacter(chunk, i) == '\n')
      {
         string line = StringSubstr(chunk, from, i - from);
         if(StringLen(line) > 0) HandleCommand(line);
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
   double vol  = NormalizeVolume(sym, StringToDouble(JsonField(line, "volume")));
   double sl   = NormalizePrice(sym, StringToDouble(JsonField(line, "sl")));
   double tp   = NormalizePrice(sym, StringToDouble(JsonField(line, "tp")));
   int    slip = (int)StringToInteger(JsonField(line, "slippage"));
   string origin = JsonField(line, "ticket");
   if(vol <= 0) { WriteLog("error", "bad volume for " + sym); return; }
   if(!EnsureSymbolReady(sym)) return;

   int t = (side == "Sell") ? OP_SELL : OP_BUY;
   double price = (t == OP_BUY) ? MarketInfo(sym, MODE_ASK) : MarketInfo(sym, MODE_BID);
   double point = MarketInfo(sym, MODE_POINT);
   int    pts   = (slip > 0 && point > 0) ? (int)MathRound(slip * PipSize(sym) / point) : 10;
   int r = OrderSend(sym, t, vol, price, pts, sl, tp, "cascada:" + origin, 0, 0, clrNONE);
   if(r < 0) WriteLog("error", "open failed " + sym + ": " + IntegerToString(GetLastError()));
}

void DoOpenPending(const string line, bool is_limit)
{
   string sym  = JsonField(line, "symbol");
   string side = JsonField(line, "side");
   double vol  = NormalizeVolume(sym, StringToDouble(JsonField(line, "volume")));
   double tgt  = NormalizePrice(sym, StringToDouble(JsonField(line, "target")));
   double sl   = NormalizePrice(sym, StringToDouble(JsonField(line, "sl")));
   double tp   = NormalizePrice(sym, StringToDouble(JsonField(line, "tp")));
   long expiry_ms = StringToInteger(JsonField(line, "expiry"));
   string origin = JsonField(line, "ticket");
   if(vol <= 0 || tgt <= 0) { WriteLog("error", "bad pending params for " + sym); return; }
   if(!EnsureSymbolReady(sym)) return;

   int t;
   if(side == "Sell") t = is_limit ? OP_SELLLIMIT : OP_SELLSTOP;
   else               t = is_limit ? OP_BUYLIMIT  : OP_BUYSTOP;
   datetime exp = (expiry_ms > 0) ? (datetime)(expiry_ms / 1000) : (datetime)0;
   int r = OrderSend(sym, t, vol, tgt, 5, sl, tp, "cascada:" + origin, 0, exp, clrNONE);
   if(r < 0) WriteLog("error", "pending failed " + sym + ": " + IntegerToString(GetLastError()));
}

void DoClose(const string line)
{
   int ticket = (int)StringToInteger(JsonField(line, "ticket"));
   double vol = StringToDouble(JsonField(line, "volume"));
   if(!OrderSelect(ticket, SELECT_BY_TICKET)) { WriteLog("warn", "close: ticket not found"); return; }
   double close_lots = (vol > 0 && vol < OrderLots()) ? vol : OrderLots();
   double price = (OrderType() == OP_BUY) ? MarketInfo(OrderSymbol(), MODE_BID)
                                          : MarketInfo(OrderSymbol(), MODE_ASK);
   if(!OrderClose(ticket, close_lots, price, 5, clrNONE))
      WriteLog("error", "close failed: " + IntegerToString(GetLastError()));
}

void DoCloseAll(const string line)
{
   string only = JsonField(line, "symbol");
   for(int i = OrdersTotal() - 1; i >= 0; i--)
   {
      if(!OrderSelect(i, SELECT_BY_POS, MODE_TRADES)) continue;
      if(OrderType() > OP_SELL) continue;
      if(StringLen(only) > 0 && OrderSymbol() != only) continue;
      double price = (OrderType() == OP_BUY) ? MarketInfo(OrderSymbol(), MODE_BID)
                                             : MarketInfo(OrderSymbol(), MODE_ASK);
      if(!OrderClose(OrderTicket(), OrderLots(), price, 5, clrNONE))
         WriteLog("warn", "close_all " + IntegerToString(OrderTicket())
                  + ": " + IntegerToString(GetLastError()));
   }
}

void DoModify(const string line)
{
   int ticket = (int)StringToInteger(JsonField(line, "ticket"));
   double sl  = StringToDouble(JsonField(line, "sl"));
   double tp  = StringToDouble(JsonField(line, "tp"));
   if(!OrderSelect(ticket, SELECT_BY_TICKET)) { WriteLog("warn", "modify: ticket not found"); return; }
   string sym = OrderSymbol();
   double new_sl = (sl > 0) ? NormalizePrice(sym, sl) : OrderStopLoss();
   double new_tp = (tp > 0) ? NormalizePrice(sym, tp) : OrderTakeProfit();
   if(!OrderModify(ticket, OrderOpenPrice(), new_sl, new_tp, 0, clrNONE))
      WriteLog("error", "modify failed: " + IntegerToString(GetLastError()));
}

void DoModifyPending(const string line)
{
   int ticket = (int)StringToInteger(JsonField(line, "ticket"));
   double tgt = StringToDouble(JsonField(line, "target"));
   double sl  = StringToDouble(JsonField(line, "sl"));
   double tp  = StringToDouble(JsonField(line, "tp"));
   long expiry_ms = StringToInteger(JsonField(line, "expiry"));
   if(!OrderSelect(ticket, SELECT_BY_TICKET)) { WriteLog("warn", "modify_pending: ticket not found"); return; }
   string sym = OrderSymbol();
   double new_tgt = (tgt > 0) ? NormalizePrice(sym, tgt) : OrderOpenPrice();
   double new_sl  = (sl  > 0) ? NormalizePrice(sym, sl)  : OrderStopLoss();
   double new_tp  = (tp  > 0) ? NormalizePrice(sym, tp)  : OrderTakeProfit();
   datetime exp = (expiry_ms > 0) ? (datetime)(expiry_ms / 1000) : OrderExpiration();
   if(!OrderModify(ticket, new_tgt, new_sl, new_tp, exp, clrNONE))
      WriteLog("error", "pending modify failed: " + IntegerToString(GetLastError()));
}

void DoCancel(const string line)
{
   int ticket = (int)StringToInteger(JsonField(line, "ticket"));
   if(!OrderDelete(ticket, clrNONE))
      WriteLog("error", "cancel failed: " + IntegerToString(GetLastError()));
}

void DoCancelAll()
{
   for(int i = OrdersTotal() - 1; i >= 0; i--)
   {
      if(!OrderSelect(i, SELECT_BY_POS, MODE_TRADES)) continue;
      if(OrderType() <= OP_SELL) continue;
      if(!OrderDelete(OrderTicket(), clrNONE))
         WriteLog("warn", "cancel_all " + IntegerToString(OrderTicket())
                  + ": " + IntegerToString(GetLastError()));
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
      // Touch MarketInfo to make MT4 stream the symbol if it isn't already.
      MarketInfo(s, MODE_BID);
      int k = ArraySize(g_subs);
      ArrayResize(g_subs, k + 1);
      g_subs[k] = s;
   }
}

void DoListSymbols()
{
   string body = "\"symbols\":[";
   bool first = true;
   int total = SymbolsTotal(false);
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
   for(int i = 0; i < n; i++)
   {
      string sym = g_subs[i];
      double bid = MarketInfo(sym, MODE_BID);
      double ask = MarketInfo(sym, MODE_ASK);
      if(bid <= 0 || ask <= 0) continue;
      string body =
         "\"symbol\":\"" + Esc(sym) + "\"" +
         ",\"bid\":"     + F5(bid) +
         ",\"ask\":"     + F5(ask) +
         ",\"ts\":"      + IntegerToString(now);
      WriteEvent("quote", body);
   }
}

//+------------------------------------------------------------------+
//| Helpers
//+------------------------------------------------------------------+
long NowMs() { return (long)TimeCurrent() * 1000; }

double PipSize(const string sym)
{
   double point = MarketInfo(sym, MODE_POINT);
   int digits = (int)MarketInfo(sym, MODE_DIGITS);
   return (digits == 3 || digits == 5) ? point * 10.0 : point;
}

double NormalizeVolume(const string sym, double vol)
{
   double step = MarketInfo(sym, MODE_LOTSTEP);
   double vmin = MarketInfo(sym, MODE_MINLOT);
   double vmax = MarketInfo(sym, MODE_MAXLOT);
   if(step <= 0) step = 0.01;
   double rounded = MathRound(vol / step) * step;
   if(rounded < vmin) rounded = vmin;
   if(vmax > 0 && rounded > vmax) rounded = vmax;
   return NormalizeDouble(rounded, 2);
}

double NormalizePrice(const string sym, double price)
{
   if(price == 0) return 0;
   int digits = (int)MarketInfo(sym, MODE_DIGITS);
   return NormalizeDouble(price, digits);
}

/// Ensure the symbol has fresh bid/ask quotes before we try to send an order.
/// MT4 has no `SymbolSelect` that guarantees Market Watch, but a MarketInfo
/// probe is usually enough to start the stream on first touch. We retry once
/// per 10 ms for up to 500 ms so the tick cache warms up.
bool EnsureSymbolReady(const string sym)
{
   for(int attempt = 0; attempt < 50; attempt++)
   {
      double bid = MarketInfo(sym, MODE_BID);
      double ask = MarketInfo(sym, MODE_ASK);
      if(bid > 0 && ask > 0) return true;
      Sleep(10);
   }
   WriteLog("error", "no tick for " + sym + " (symbol not streaming)");
   return false;
}

string ExtractOrigin(const string comment)
{
   if(StringLen(comment) >= 8 && StringFind(comment, "cascada:") == 0)
      return StringSubstr(comment, 8);
   return "";
}

string F2(double d) { return DoubleToString(d, 2); }
string F5(double d) { return DoubleToString(d, 5); }

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
      ushort c = (ushort)StringGetCharacter(s, end);
      if(c == ',' || c == '}' || c == '"') break;
      end++;
   }
   return StringSubstr(s, i, end - i);
}

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
      ushort c = (ushort)StringGetCharacter(s, i);
      if(c == ']') break;
      if(c == ' ' || c == ',') { i++; continue; }
      if(c == '"')
      {
         i++;
         int start = i;
         while(i < len)
         {
            ushort cc = (ushort)StringGetCharacter(s, i);
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
      ushort c = (ushort)StringGetCharacter(s, i);
      if(c == '\\' || c == '"' || c < 0x20) { clean = false; break; }
   }
   if(clean) return s;
   string out = "";
   for(int i = 0; i < n; i++)
   {
      ushort c = (ushort)StringGetCharacter(s, i);
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
