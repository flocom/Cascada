// CascadaBridge.cs — cTrader Automate cBot (file-based IPC)
// Reads commands from   <root>/cAlgo/Cascada/<login>/cmd.jsonl
// Writes events to      <root>/cAlgo/Cascada/<login>/events.jsonl
// <root> = ~/cAlgo (Mac native) or <MyDocuments>/cAlgo (Win/Wine).
// No network permissions required.

using System;
using System.Globalization;
using System.IO;
using System.Text;
using cAlgo.API;
using cAlgo.API.Internals;

// 618: ModifyPosition / ModifyPendingOrder overloads marked obsolete because a newer
// overload with explicit ProtectionType is preferred. The old ones still work correctly
// across cTrader versions; suppressing keeps backward compat.
#pragma warning disable 618

namespace cAlgo.Robots
{
    [Robot(AccessRights = AccessRights.FullAccess)]
    public class CascadaBridge : Robot
    {
        private string _cmdFile;
        private string _evtFile;
        private long _cmdOffset;
        private DateTime _lastHb = DateTime.MinValue;
        private readonly System.Collections.Generic.HashSet<string> _subs =
            new System.Collections.Generic.HashSet<string>(StringComparer.OrdinalIgnoreCase);
        private static readonly CultureInfo Inv = CultureInfo.InvariantCulture;
        private const int HistoryDays = 7;
        private const int HistoryMax  = 500;

        protected override void OnStart()
        {
            try
            {
                var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
                var macNative = Path.Combine(home, "cAlgo");
                var root = Directory.Exists(macNative)
                    ? macNative
                    : Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments), "cAlgo");
                var dir = Path.Combine(root, "Cascada", Account.Number.ToString(Inv));
                Directory.CreateDirectory(dir);
                _cmdFile = Path.Combine(dir, "cmd.jsonl");
                _evtFile = Path.Combine(dir, "events.jsonl");
                System.IO.File.WriteAllText(_evtFile, "", Encoding.UTF8);
                _cmdOffset = System.IO.File.Exists(_cmdFile) ? new FileInfo(_cmdFile).Length : 0;

                WriteWelcome();
                foreach (var p in Positions)     Safe(() => WriteOpen(p),              "snap_open");
                foreach (var o in PendingOrders) Safe(() => WritePending("pending", o), "snap_pend");
                Safe(WriteHistorySnapshot, "snap_hist");

                Positions.Opened       += e => Safe(() => WriteOpen(e.Position),              "opened");
                Positions.Closed       += e => Safe(() => WriteClose(e.Position, e.Reason),   "closed");
                Positions.Modified     += e => Safe(() => WriteModify(e.Position),            "modified");
                PendingOrders.Created  += e => Safe(() => WritePending("pending",        e.PendingOrder), "pend_create");
                PendingOrders.Modified += e => Safe(() => WritePending("pending_modify", e.PendingOrder), "pend_modify");
                PendingOrders.Cancelled+= e => Safe(() => WritePendingEnd("pending_cancel", e.PendingOrder), "pend_cancel");
                PendingOrders.Filled   += e => Safe(() => WritePendingEnd("pending_fill",   e.PendingOrder), "pend_fill");

                // 250 ms keeps quote sampling on par with MT4/MT5 (≈4 Hz, well under
                // the backend's 5 Hz emit cap). Heartbeat is self-throttled to 1 Hz
                // via _lastHb, so only PumpCommands + PushQuotes benefit.
                Timer.Start(TimeSpan.FromMilliseconds(250));
            }
            catch (Exception ex) { TryLog("error", "start failed: " + ex.Message); }
        }

        protected override void OnTimer()
        {
            Safe(PumpCommands,  "pump");
            Safe(PushHeartbeat, "hb");
            Safe(PushQuotes,    "quotes");
        }

        // ---------- Event writers ----------

        private void WriteWelcome()
        {
            WriteEvent("welcome", string.Format(Inv,
                "\"balance\":{0},\"equity\":{1},\"margin\":{2},\"free_margin\":{3},\"currency\":\"{4}\",\"leverage\":{5},\"broker\":\"{6}\",\"account\":\"{7}\",\"is_live\":{8}",
                F(Account.Balance), F(Account.Equity),
                F(Account.Margin), F(Account.FreeMargin),
                Esc(Account.Asset.Name), F(Account.PreciseLeverage),
                Esc(Account.BrokerName), Account.Number,
                Account.IsLive ? "true" : "false"));
        }

        private void PushHeartbeat()
        {
            if ((DateTime.UtcNow - _lastHb).TotalSeconds < 1) return;
            _lastHb = DateTime.UtcNow;
            WriteEvent("heartbeat", string.Format(Inv,
                "\"balance\":{0},\"equity\":{1},\"margin\":{2},\"free_margin\":{3},\"unrealized\":{4},\"positions\":{5},\"pending\":{6}",
                F(Account.Balance), F(Account.Equity),
                F(Account.Margin), F(Account.FreeMargin),
                F(Account.UnrealizedNetProfit), Positions.Count, PendingOrders.Count));
        }

        private void WriteOpen(Position p)
        {
            string origin = ExtractOrigin(p.Comment);
            var sym = TryGetSymbol(p.SymbolName);
            double lots    = sym != null ? sym.VolumeInUnitsToQuantity(p.VolumeInUnits) : p.VolumeInUnits / 100000.0;
            double pipSize = sym != null ? sym.PipSize : 0;
            WriteEvent("open", string.Format(Inv,
                "\"ticket\":\"{0}\",\"symbol\":\"{1}\",\"side\":\"{2}\",\"volume\":{3},\"units\":{4},\"price\":{5},\"sl\":{6},\"tp\":{7},\"commission\":{8},\"swap\":{9},\"pip_size\":{10},\"label\":\"{11}\",\"comment\":\"{12}\",\"origin\":\"{13}\",\"ts\":{14}",
                p.Id, Esc(p.SymbolName),
                p.TradeType == TradeType.Buy ? "Buy" : "Sell",
                F(lots), F(p.VolumeInUnits), F(p.EntryPrice),
                F(p.StopLoss ?? 0), F(p.TakeProfit ?? 0),
                F(p.Commissions), F(p.Swap),
                F(pipSize), Esc(p.Label ?? ""), Esc(p.Comment ?? ""), Esc(origin), Now()));
        }

        private void WriteModify(Position p)
        {
            var sym = TryGetSymbol(p.SymbolName);
            double lots = sym != null ? sym.VolumeInUnitsToQuantity(p.VolumeInUnits) : p.VolumeInUnits / 100000.0;
            WriteEvent("modify", string.Format(Inv,
                "\"ticket\":\"{0}\",\"sl\":{1},\"tp\":{2},\"volume\":{3},\"units\":{4},\"ts\":{5}",
                p.Id, F(p.StopLoss ?? 0), F(p.TakeProfit ?? 0),
                F(lots), F(p.VolumeInUnits), Now()));
        }

        private void WriteClose(Position p, PositionCloseReason reason)
        {
            double closePx = p.CurrentPrice;
            WriteEvent("close", string.Format(Inv,
                "\"ticket\":\"{0}\",\"price\":{1},\"profit\":{2},\"gross\":{3},\"commission\":{4},\"swap\":{5},\"pips\":{6},\"balance\":{7},\"reason\":\"{8}\",\"ts\":{9}",
                p.Id, F(closePx), F(p.NetProfit), F(p.GrossProfit),
                F(p.Commissions), F(p.Swap), F(p.Pips),
                F(Account.Balance), reason, Now()));
        }

        private void WritePending(string ev, PendingOrder o)
        {
            var sym = TryGetSymbol(o.SymbolName);
            double lots = sym != null ? sym.VolumeInUnitsToQuantity(o.VolumeInUnits) : o.VolumeInUnits / 100000.0;
            long expiry = o.ExpirationTime.HasValue ? UnixMs(o.ExpirationTime.Value) : 0;
            WriteEvent(ev, string.Format(Inv,
                "\"ticket\":\"{0}\",\"symbol\":\"{1}\",\"side\":\"{2}\",\"order_type\":\"{3}\",\"volume\":{4},\"units\":{5},\"target\":{6},\"sl\":{7},\"tp\":{8},\"expiry\":{9},\"label\":\"{10}\",\"comment\":\"{11}\",\"origin\":\"{12}\",\"ts\":{13}",
                o.Id, Esc(o.SymbolName),
                o.TradeType == TradeType.Buy ? "Buy" : "Sell",
                o.OrderType, F(lots), F(o.VolumeInUnits), F(o.TargetPrice),
                F(o.StopLoss ?? 0), F(o.TakeProfit ?? 0), expiry,
                Esc(o.Label ?? ""), Esc(o.Comment ?? ""),
                Esc(ExtractOrigin(o.Comment)), Now()));
        }

        private void WritePendingEnd(string ev, PendingOrder o)
        {
            WriteEvent(ev, string.Format(Inv,
                "\"ticket\":\"{0}\",\"symbol\":\"{1}\",\"ts\":{2}",
                o.Id, Esc(o.SymbolName), Now()));
        }

        private void WriteHistorySnapshot()
        {
            DateTime since = Server.Time.AddDays(-HistoryDays);
            int emitted = 0;
            foreach (var h in History)
            {
                if (h.ClosingTime < since) continue;
                if (emitted >= HistoryMax) break;
                WriteEvent("history", string.Format(Inv,
                    "\"ticket\":\"{0}\",\"symbol\":\"{1}\",\"side\":\"{2}\",\"volume\":{3},\"units\":{4},\"entry\":{5},\"close\":{6},\"profit\":{7},\"gross\":{8},\"commission\":{9},\"swap\":{10},\"pips\":{11},\"balance\":{12},\"label\":\"{13}\",\"comment\":\"{14}\",\"origin\":\"{15}\",\"opened_at\":{16},\"closed_at\":{17}",
                    h.PositionId, Esc(h.SymbolName),
                    h.TradeType == TradeType.Buy ? "Buy" : "Sell",
                    F(h.Quantity), F(h.VolumeInUnits),
                    F(h.EntryPrice), F(h.ClosingPrice),
                    F(h.NetProfit), F(h.GrossProfit),
                    F(h.Commissions), F(h.Swap), F(h.Pips),
                    F(h.Balance), Esc(h.Label ?? ""), Esc(h.Comment ?? ""),
                    Esc(ExtractOrigin(h.Comment)),
                    UnixMs(h.EntryTime), UnixMs(h.ClosingTime)));
                emitted++;
            }
            WriteEvent("history_done", "\"count\":" + emitted);
        }

        // ---------- Command pump ----------

        private void PumpCommands()
        {
            var fi = new FileInfo(_cmdFile);
            if (!fi.Exists) return;
            if (fi.Length < _cmdOffset) _cmdOffset = 0;
            if (fi.Length == _cmdOffset) return;
            int count = (int)(fi.Length - _cmdOffset);
            var bytes = new byte[count];
            int read = 0;
            using (var fs = new FileStream(_cmdFile, FileMode.Open, FileAccess.Read, FileShare.ReadWrite))
            {
                fs.Seek(_cmdOffset, SeekOrigin.Begin);
                while (read < count)
                {
                    int n = fs.Read(bytes, read, count - read);
                    if (n <= 0) break;
                    read += n;
                }
            }
            int lastNl = -1;
            for (int i = read - 1; i >= 0; i--) if (bytes[i] == (byte)'\n') { lastNl = i; break; }
            if (lastNl < 0) return;
            var chunk = Encoding.UTF8.GetString(bytes, 0, lastNl + 1);
            _cmdOffset += lastNl + 1;
            foreach (var line in chunk.Split('\n'))
                if (!string.IsNullOrWhiteSpace(line)) Safe(() => HandleCommand(line), "handle");
        }

        private void HandleCommand(string line)
        {
            string op = JsonField(line, "op");
            switch (op)
            {
                case "open":            DoOpenMarket(line);  break;
                case "open_limit":      DoOpenPending(line, PendingOrderType.Limit); break;
                case "open_stop":       DoOpenPending(line, PendingOrderType.Stop);  break;
                case "close":           DoClose(line);       break;
                case "close_all":       DoCloseAll(line);    break;
                case "modify":          DoModify(line);      break;
                case "modify_pending":  DoModifyPending(line); break;
                case "cancel":          DoCancel(line);      break;
                case "cancel_all":      DoCancelAll();       break;
                case "subscribe":       DoSubscribe(line);   break;
                case "list_symbols":    DoListSymbols();     break;
                case "hello":           /* legacy, ignore */ break;
                default: TryLog("warn", "unknown op: " + op); break;
            }
        }

        private void DoOpenMarket(string line)
        {
            string sym = JsonField(line, "symbol");
            string side = JsonField(line, "side");
            double vol = ParseDouble(JsonField(line, "volume"));
            double sl = ParseDouble(JsonField(line, "sl"));
            double tp = ParseDouble(JsonField(line, "tp"));
            double slip = ParseDouble(JsonField(line, "slippage"));
            string origin = JsonField(line, "ticket");
            var symbol = TryGetSymbol(sym);
            if (symbol == null) { TryLog("error", "unknown symbol " + sym); return; }
            double units = ResolveUnits(symbol, vol);
            if (units <= 0) { TryLog("error", "volume too small for " + sym); return; }
            var tt = side == "Sell" ? TradeType.Sell : TradeType.Buy;
            string label = "cascada:" + origin;
            double basePx = tt == TradeType.Buy ? symbol.Ask : symbol.Bid;
            double? slPips = sl > 0 ? (double?)(Math.Abs(basePx - sl) / symbol.PipSize) : null;
            double? tpPips = tp > 0 ? (double?)(Math.Abs(tp - basePx) / symbol.PipSize) : null;
            TradeResult r = slip > 0
                ? ExecuteMarketRangeOrder(tt, sym, units, slip, basePx, label, slPips, tpPips)
                : ExecuteMarketOrder(tt, sym, units, label, slPips, tpPips);
            if (!r.IsSuccessful) { TryLog("error", "open failed (" + sym + "): " + r.Error); return; }
            if ((sl > 0 || tp > 0) && r.Position != null && (r.Position.StopLoss == null && r.Position.TakeProfit == null))
            {
                var mr = ModifyPosition(r.Position,
                    sl > 0 ? (double?)sl : null,
                    tp > 0 ? (double?)tp : null);
                if (!mr.IsSuccessful) TryLog("warn", "sl/tp attach failed: " + mr.Error);
            }
        }

        private void DoOpenPending(string line, PendingOrderType ptype)
        {
            string sym = JsonField(line, "symbol");
            string side = JsonField(line, "side");
            double vol = ParseDouble(JsonField(line, "volume"));
            double target = ParseDouble(JsonField(line, "target"));
            double sl = ParseDouble(JsonField(line, "sl"));
            double tp = ParseDouble(JsonField(line, "tp"));
            double expMs = ParseDouble(JsonField(line, "expiry"));
            string origin = JsonField(line, "ticket");
            var symbol = TryGetSymbol(sym);
            if (symbol == null) { TryLog("error", "unknown symbol " + sym); return; }
            double units = ResolveUnits(symbol, vol);
            if (units <= 0 || target <= 0) { TryLog("error", "bad pending params for " + sym); return; }
            var tt = side == "Sell" ? TradeType.Sell : TradeType.Buy;
            DateTime? expiry = expMs > 0 ? (DateTime?)FromUnixMs((long)expMs) : null;
            string label = "cascada:" + origin;
            double? slPx = sl > 0 ? (double?)sl : null;
            double? tpPx = tp > 0 ? (double?)tp : null;
            TradeResult r = ptype == PendingOrderType.Limit
                ? PlaceLimitOrder(tt, sym, units, target, label, slPx, tpPx, expiry, "")
                : PlaceStopOrder (tt, sym, units, target, label, slPx, tpPx, expiry, "");
            if (!r.IsSuccessful) TryLog("error", "pending failed (" + sym + "): " + r.Error);
        }

        private void DoClose(string line)
        {
            long id;
            if (!long.TryParse(JsonField(line, "ticket"), out id)) return;
            double vol = ParseDouble(JsonField(line, "volume"));
            foreach (var p in Positions)
                if (p.Id == id)
                {
                    TradeResult r;
                    if (vol > 0)
                    {
                        var sym = TryGetSymbol(p.SymbolName);
                        double units = ResolveUnits(sym, vol);
                        if (units <= 0 || units >= p.VolumeInUnits) r = ClosePosition(p);
                        else r = ClosePosition(p, units);
                    }
                    else r = ClosePosition(p);
                    if (!r.IsSuccessful) TryLog("error", "close failed: " + r.Error);
                    return;
                }
            TryLog("warn", "close: ticket " + id + " not found");
        }

        private void DoCloseAll(string line)
        {
            string onlySym = JsonField(line, "symbol");
            foreach (var p in Positions)
            {
                if (!string.IsNullOrEmpty(onlySym) && p.SymbolName != onlySym) continue;
                var r = ClosePosition(p);
                if (!r.IsSuccessful) TryLog("warn", "close_all " + p.Id + ": " + r.Error);
            }
        }

        private void DoModify(string line)
        {
            long id;
            if (!long.TryParse(JsonField(line, "ticket"), out id)) return;
            double sl = ParseDouble(JsonField(line, "sl"));
            double tp = ParseDouble(JsonField(line, "tp"));
            string trailStr = JsonField(line, "trailing");
            bool? trailing = null;
            if (trailStr == "true")  trailing = true;
            if (trailStr == "false") trailing = false;
            foreach (var p in Positions)
                if (p.Id == id)
                {
                    double? newSl = sl > 0 ? (double?)sl : p.StopLoss;
                    double? newTp = tp > 0 ? (double?)tp : p.TakeProfit;
                    TradeResult r = trailing.HasValue
                        ? ModifyPosition(p, newSl, newTp, trailing.Value)
                        : ModifyPosition(p, newSl, newTp);
                    if (!r.IsSuccessful) TryLog("error", "modify failed: " + r.Error);
                    return;
                }
            TryLog("warn", "modify: ticket " + id + " not found");
        }

        private void DoModifyPending(string line)
        {
            long id;
            if (!long.TryParse(JsonField(line, "ticket"), out id)) return;
            double target = ParseDouble(JsonField(line, "target"));
            double sl = ParseDouble(JsonField(line, "sl"));
            double tp = ParseDouble(JsonField(line, "tp"));
            double expMs = ParseDouble(JsonField(line, "expiry"));
            foreach (var o in PendingOrders)
                if (o.Id == id)
                {
                    double px = target > 0 ? target : o.TargetPrice;
                    double? slPx = sl > 0 ? (double?)sl : o.StopLoss;
                    double? tpPx = tp > 0 ? (double?)tp : o.TakeProfit;
                    DateTime? expiry = expMs > 0 ? (DateTime?)FromUnixMs((long)expMs) : o.ExpirationTime;
                    var r = ModifyPendingOrder(o, px, slPx, tpPx, expiry);
                    if (!r.IsSuccessful) TryLog("error", "pending modify failed: " + r.Error);
                    return;
                }
            TryLog("warn", "modify_pending: ticket " + id + " not found");
        }

        private void DoCancel(string line)
        {
            long id;
            if (!long.TryParse(JsonField(line, "ticket"), out id)) return;
            foreach (var o in PendingOrders)
                if (o.Id == id) { var r = CancelPendingOrder(o); if (!r.IsSuccessful) TryLog("error", "cancel failed: " + r.Error); return; }
            TryLog("warn", "cancel: ticket " + id + " not found");
        }

        private void DoCancelAll()
        {
            foreach (var o in PendingOrders)
            {
                var r = CancelPendingOrder(o);
                if (!r.IsSuccessful) TryLog("warn", "cancel_all " + o.Id + ": " + r.Error);
            }
        }

        private void DoSubscribe(string line)
        {
            var list = JsonStringArray(line, "symbols");
            _subs.Clear();
            foreach (var s in list)
            {
                var trimmed = (s ?? "").Trim();
                if (trimmed.Length > 0) _subs.Add(trimmed);
            }
        }

        private void DoListSymbols()
        {
            var sb = new StringBuilder();
            sb.Append("\"symbols\":[");
            bool first = true;
            try
            {
                // Use the indexer + Count — `Symbols[i]` returns the symbol *name* string.
                // `GetSymbolNames()` was added in a later cAlgo build, so this stays
                // portable across cTrader versions.
                int count = Symbols.Count;
                for (int i = 0; i < count; i++)
                {
                    string name = Symbols[i];
                    if (string.IsNullOrEmpty(name)) continue;
                    if (!first) sb.Append(',');
                    sb.Append('"').Append(Esc(name)).Append('"');
                    first = false;
                }
            }
            catch (Exception ex) { TryLog("warn", "list_symbols enum: " + ex.Message); }
            sb.Append(']');
            WriteEvent("symbols", sb.ToString());
        }

        private void PushQuotes()
        {
            if (_subs.Count == 0) return;
            foreach (var name in _subs)
            {
                var sym = TryGetSymbol(name);
                if (sym == null) continue;
                double bid = sym.Bid, ask = sym.Ask;
                if (bid <= 0 || ask <= 0) continue;
                WriteEvent("quote", string.Format(Inv,
                    "\"symbol\":\"{0}\",\"bid\":{1},\"ask\":{2},\"pip_size\":{3},\"ts\":{4}",
                    Esc(name), F(bid), F(ask), F(sym.PipSize), Now()));
            }
        }

        // ---------- Plumbing ----------

        private void WriteEvent(string ev, string body)
        {
            try { System.IO.File.AppendAllText(_evtFile, "{\"ev\":\"" + ev + "\"," + body + "}\n", Encoding.UTF8); }
            catch (Exception ex) { Print("write error: " + ex.Message); }
        }

        private void TryLog(string level, string msg)
        {
            WriteEvent("log", "\"level\":\"" + level + "\",\"message\":\"" + Esc(msg ?? "") + "\"");
        }

        private void Safe(Action a, string tag)
        {
            try { a(); }
            catch (Exception ex) { TryLog("error", tag + ": " + ex.Message); }
        }

        private Symbol TryGetSymbol(string name)
        {
            try { return Symbols.GetSymbol(name); }
            catch { return null; }
        }

        private static double ResolveUnits(Symbol symbol, double lots)
        {
            if (symbol == null) return lots * 100000.0;
            double raw = symbol.QuantityToVolumeInUnits(lots);
            if (raw <= 0) raw = lots * 100000.0;
            return symbol.NormalizeVolumeInUnits(raw, RoundingMode.ToNearest);
        }

        private static string ExtractOrigin(string comment)
        {
            return (comment != null && comment.StartsWith("cascada:")) ? comment.Substring(8) : "";
        }

        private static string F(double d) { return d.ToString("0.#####", Inv); }
        private static long Now() { return UnixMs(DateTime.UtcNow); }
        private static long UnixMs(DateTime t)
        {
            var utc = t.Kind == DateTimeKind.Utc ? t : t.ToUniversalTime();
            return (long)(utc - new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc)).TotalMilliseconds;
        }
        private static DateTime FromUnixMs(long ms)
        {
            return new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc).AddMilliseconds(ms);
        }

        private static double ParseDouble(string s)
        {
            double d;
            double.TryParse(s, NumberStyles.Float, Inv, out d);
            return d;
        }

        private static string Esc(string s)
        {
            if (string.IsNullOrEmpty(s)) return "";
            var sb = new StringBuilder(s.Length + 4);
            foreach (var c in s)
            {
                switch (c)
                {
                    case '\\': sb.Append("\\\\"); break;
                    case '"':  sb.Append("\\\""); break;
                    case '\n': sb.Append("\\n");  break;
                    case '\r': sb.Append("\\r");  break;
                    case '\t': sb.Append("\\t");  break;
                    default:
                        if (c < 0x20) sb.AppendFormat(Inv, "\\u{0:x4}", (int)c);
                        else sb.Append(c);
                        break;
                }
            }
            return sb.ToString();
        }

        private static System.Collections.Generic.List<string> JsonStringArray(string s, string key)
        {
            var result = new System.Collections.Generic.List<string>();
            var needle = "\"" + key + "\":";
            int i = s.IndexOf(needle);
            if (i < 0) return result;
            i += needle.Length;
            while (i < s.Length && s[i] == ' ') i++;
            if (i >= s.Length || s[i] != '[') return result;
            i++;
            while (i < s.Length && s[i] != ']')
            {
                while (i < s.Length && (s[i] == ' ' || s[i] == ',')) i++;
                if (i >= s.Length || s[i] == ']') break;
                if (s[i] == '"')
                {
                    i++;
                    var sb = new StringBuilder();
                    while (i < s.Length && s[i] != '"')
                    {
                        if (s[i] == '\\' && i + 1 < s.Length) { sb.Append(s[i + 1]); i += 2; }
                        else { sb.Append(s[i]); i++; }
                    }
                    if (i < s.Length) i++;
                    result.Add(sb.ToString());
                }
                else i++;
            }
            return result;
        }

        private static string JsonField(string s, string key)
        {
            var needle = "\"" + key + "\":";
            int i = s.IndexOf(needle);
            if (i < 0) return "";
            i += needle.Length;
            while (i < s.Length && (s[i] == ' ' || s[i] == '"')) i++;
            int end = i;
            while (end < s.Length)
            {
                char c = s[end];
                if (c == ',' || c == '}' || c == '"') break;
                end++;
            }
            return s.Substring(i, end - i);
        }
    }
}
