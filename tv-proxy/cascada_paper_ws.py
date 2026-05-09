"""WebSocket-driven handlers for the TV PaperTrading bridge.

Mixed into the main TVBridge class via inheritance. All methods on
this mixin take `self` and rely on state owned by the TVBridge
dataclass (see cascada_addon.py): `paper_position_by_symbol`,
`pending_paper`, `_pending_buffer`, `quote_cache`, etc.

Handlers covered here:
  - `on_websocket_message`  — top-level mitmproxy WS hook
  - `_on_qsd`               — prodata.tradingview.com Quote ticks
  - `_on_pushstream_frame`  — pushstream `trading` channel envelope
  - `_on_paper_order_update`    — order lifecycle (open / pending / fill)
  - `_on_paper_position_update` — netted position state + partial close
  - `_handle_cmd`           — cmd.jsonl `subscribe` from the slave side
"""

import json
import re
import threading
import time
from typing import Any

from cascada_helpers import (
    PositionMeta,
    PendingMeta,
    _safe_float,
    now_ms,
    to_side,
)


class PaperWsMixin:
    # ─── WebSocket bridge ───────────────────────────────────────────
    #
    # TV restricts each user to one active REST session. The browser
    # the user keeps in front gets the `/trading/account` API; every
    # other client (incl. Cascada-Chrome on the same login) sees 401.
    # The pushstream WS stays alive regardless and broadcasts the full
    # order/position lifecycle to every connected client of the
    # account, so we read trades off it instead of polling REST.

    def on_websocket_message(self, flow) -> None:  # type: ignore[no-untyped-def]
        ws = getattr(flow, "websocket", None)
        if ws is None or not ws.messages:
            return
        msg = ws.messages[-1]
        host = flow.request.host if flow.request else ""
        if msg.from_client or not msg.is_text:
            return
        text = (msg.content.decode("utf-8", errors="replace")
                if isinstance(msg.content, bytes) else str(msg.content))

        # pushstream: trade lifecycle WS — `{text:{channel,content}}` JSON.
        if "pushstream.tradingview" in host:
            try:
                obj = json.loads(text)
            except Exception:
                return
            if isinstance(obj, dict):
                self._on_pushstream_frame(obj.get("text") or {})
            return

        # Quote feeds: TV streams `~m~LEN~m~JSON` frames carrying `qsd`
        # quote payloads. We accept any `*.tradingview.com` host and
        # filter at the message-type layer (only `qsd` is processed),
        # so a feed served from a less-common endpoint still flows.
        if "tradingview.com" not in host:
            return
        for chunk in re.findall(r'~m~\d+~m~(\{[^~]*\})', text):
            try:
                obj = json.loads(chunk)
            except Exception:
                continue
            if isinstance(obj, dict) and obj.get("m") == "qsd":
                self._on_qsd(obj.get("p") or [])

    def _on_qsd(self, params: list) -> None:
        # qsd payload: ["qs_session", {"n":"<exchange>:<sym>", "v":{bid,
        # ask, pricescale, …}, "s":"ok"}]. We KEEP the exchange prefix
        # in the cache key so the same instrument from two different
        # data feeds (BITSTAMP:BTCUSD vs COINBASE:BTCUSD) doesn't
        # collide and the user can pick which feed to compare against
        # in the Quote-compare panel. Trade flows still strip the
        # prefix since the slave broker expects its own local ticker.
        if len(params) < 2 or not isinstance(params[1], dict):
            return
        payload = params[1]
        symbol = (payload.get("n") or "").upper()
        if not symbol:
            return
        v = payload.get("v") or {}
        if not isinstance(v, dict):
            return
        cached = self.quote_cache.setdefault(
            symbol, {"bid": 0.0, "ask": 0.0, "pip_size": 0.0})
        if "bid" in v:
            try: cached["bid"] = float(v["bid"])
            except (TypeError, ValueError): pass
        if "ask" in v:
            try: cached["ask"] = float(v["ask"])
            except (TypeError, ValueError): pass
        # Fallback to last price when an exchange only ships `lp`
        # (BITSTAMP / some crypto feeds don't carry bid/ask separately).
        # The Compare UI keys off bid > 0 and ask > 0; without this
        # symbols stay frozen at "—" even though TV sent a quote.
        if "lp" in v:
            try:
                lp = float(v["lp"])
                if lp > 0:
                    if cached["bid"] <= 0: cached["bid"] = lp
                    if cached["ask"] <= 0: cached["ask"] = lp
            except (TypeError, ValueError):
                pass
        # pricescale=10^precision (TV convention). Forex 5-digit gives
        # pricescale=100000 → pip_size=0.0001 (one tenth of the smallest
        # quoted increment — the standard fx pip).
        ps = v.get("pricescale")
        if isinstance(ps, (int, float)) and ps > 0:
            cached["pip_size"] = 10.0 / float(ps)
        if cached["bid"] <= 0 or cached["ask"] <= 0:
            return
        # Match the qsd symbol against the slave's subscription. Exact
        # hit wins (the user picked a specific feed). Otherwise we
        # check the bare ticker: strip a leading "EXCHANGE:" prefix or
        # a trailing "*BROKER" suffix and try that — lets the user
        # subscribe to "EURUSD" and still receive quotes from
        # "OANDA:EURUSD" or "EURUSD*PEPPERSTONE" without having to
        # pick the feed manually.
        match = symbol if symbol in self.quote_subs else None
        if not match:
            bare = symbol
            colon = bare.find(":")
            if colon > 0: bare = bare[colon + 1:]
            star = bare.find("*")
            if star > 0: bare = bare[:star]
            if bare and bare in self.quote_subs:
                match = bare
        if not match:
            return
        self._emit({
            "ev": "quote",
            "symbol": match,
            "bid": cached["bid"],
            "ask": cached["ask"],
            "pip_size": cached["pip_size"],
            "ts": now_ms(),
        })

    def _on_pushstream_frame(self, txt: dict[str, Any]) -> None:
        # The `trading` channel envelope is `{channel, content:{m, p, accountId}}`.
        # Only `order_update` / `position_update` / `balance_update` are
        # routed: account_change / execution / journal are either
        # redundant or fire on partial closes (which would mis-emit a
        # full close to the slave).
        if txt.get("channel") != "trading":
            return
        content = txt.get("content") or {}
        if not isinstance(content, dict):
            return
        frame_account = str(content.get("accountId") or "")
        if not frame_account:
            return
        # Cross-browser bootstrap: when the user trades from a non-Cascada
        # browser, REST auto-discovery never fires. Use the first WS frame
        # to seed account_id AND go through configure() so out_dir lands
        # under cascada_root/TradingView/<id>/ where the slave EA tails.
        if not self.account_id:
            self.configure("papertrading.tradingview.com",
                           frame_account, self.cascada_root_override)
            self._log(f"auto-discovered TV PaperTrading account={frame_account} (ws)")
        elif frame_account != self.account_id:
            return
        m = content.get("m")
        p = content.get("p") or {}
        if not isinstance(p, dict):
            return
        if m == "order_update":
            self._on_paper_order_update(p)
        elif m == "position_update":
            self._on_paper_position_update(p)
        elif m == "balance_update":
            # `{balance: <usd>, accountId: …}`. PaperTrading exposes
            # only `balance`; mirror it into equity so the heartbeat
            # carries a non-zero number to the Cascada UI.
            bal = _safe_float(p.get("balance"))
            if bal:
                self.last_balance = bal
                self.last_equity = bal

    def _on_paper_order_update(self, p: dict[str, Any]) -> None:
        # Frame shape: { id, symbol, symboltype, side, qty, type:
        # "market"|"limit"|"stop"|"stoplimit", status: "pending"|"filled"|
        # "canceled"|"working"|…, sl, tp, close-date, label, parent }.
        # PaperTrading is a netting account: the entry order id is
        # reused as the position ticket for the lifetime of the
        # position, so we key tracking off it directly.
        symbol = self._strip_exchange(p.get("symbol", ""))
        if not symbol:
            return
        order_id = str(p.get("id") or "")
        label = str(p.get("label") or "")
        close_date = p.get("close-date")
        side = to_side(p.get("side", ""))
        symboltype = str(p.get("symboltype") or "forex")
        qty_abs = abs(self._qty_to_lots(_safe_float(p.get("qty")), symboltype))
        sl = _safe_float(p.get("sl"))
        tp = _safe_float(p.get("tp"))
        order_type = str(p.get("type") or "").lower()
        status = str(p.get("status") or "").lower()
        target = _safe_float(p.get("price"))
        pip_size = self.pip_sizes.get(symbol, 0.0)
        # Status keywords. Computed once and reused across the SL/TP
        # child, pending lifecycle, pending termination and buffered
        # fast-path branches — keeps the classification consistent.
        is_filled = any(k in status for k in ("fill", "execut"))
        is_cancelled = any(k in status for k in ("cancel", "reject", "expir"))
        is_terminal = is_filled or is_cancelled

        # Child SL/TP order. TV emits one whenever the user sets, moves
        # or REMOVES a stop / take-profit on an open position. Removal
        # arrives as a child `order_update` with status=cancelled — the
        # price field still carries the OLD level, so we read the
        # cancellation signal explicitly. The parent points at one
        # specific entry id but TV nets all entries on a symbol under
        # the same sl/tp, so we propagate the change to every slave
        # ticket for this symbol.
        if label in ("sl", "tp"):
            parent_id = str(p.get("parent") or "")
            anchor = self._ticket_for_order_id(parent_id)
            if not anchor:
                # Synth-opened fallback: when Cascada started after a
                # position was already open, we keyed it by a synthetic
                # `paper-<account>-<symbol>` ticket — the child's
                # parent_id (the real TV order id) won't resolve. Look
                # up by symbol instead.
                fallback = self.paper_position_by_symbol.get(symbol, [])
                if not fallback:
                    return
                anchor = fallback[0]
            anchor_meta = self.positions.get(anchor)
            if not anchor_meta:
                return
            # close-date is not used as a cancellation signal on child
            # SL/TP orders — TV stamps it as the level's natural
            # expiration time even while the order is still active.
            new_val = 0.0 if is_cancelled else _safe_float(p.get("price"))
            tickets = self.paper_position_by_symbol.get(anchor_meta.symbol, [anchor])
            emitted = False
            for t in tickets:
                meta = self.positions.get(t)
                if not meta:
                    continue
                changed = False
                if label == "sl" and new_val != meta.sl:
                    meta.sl = new_val; changed = True
                elif label == "tp" and new_val != meta.tp:
                    meta.tp = new_val; changed = True
                if not changed:
                    continue
                self._emit({"ev": "modify", "ticket": t,
                            "sl": meta.sl, "tp": meta.tp, "ts": now_ms()})
                emitted = True
            if not emitted:
                return
            self._log(f"modify {symbol} ticket={anchor} "
                      f"sl={anchor_meta.sl} tp={anchor_meta.tp} (ws)")
            return

        is_pending_kind = order_type in ("limit", "stop", "stoplimit")
        # Pending lifecycle handles every non-terminal frame for a
        # limit/stop order — TV's first frame after placement isn't
        # always `status="pending"` (sometimes "working" or empty),
        # and we'd previously fall through to the market open path
        # and emit an `open` to the slave before the buffered pending
        # landed. Terminal frames are routed to the pending
        # termination / buffered fast-path branches below.
        if is_pending_kind and not is_terminal and order_id:
            order_kind = ("Limit" if order_type == "limit"
                          else "StopLimit" if order_type == "stoplimit"
                          else "Stop")
            # Instant-fill protection comes from the deferred-emit
            # buffer: held ~200ms before flush, so a TV-side immediate
            # fill (terminal frame arriving in the window) drops the
            # buffered pending and emits a plain `open` instead.
            pmeta = PendingMeta(
                symbol=symbol, side=side, volume=qty_abs,
                target=target, sl=sl, tp=tp,
                order_type=order_kind, pip_size=pip_size,
            )
            event = {
                "ev": "pending",
                "ticket": order_id,
                "symbol": symbol,
                "side": side,
                "order_type": order_kind,
                "volume": qty_abs,
                "target": target,
                "sl": sl, "tp": tp,
                "expiry": 0,
                "pip_size": pip_size,
                "ts": now_ms(),
            }
            log = (f"pending {symbol} {side} {qty_abs:g} "
                   f"{order_kind}@{target} sl={sl} tp={tp} "
                   f"ticket={order_id} (ws)")
            # Already in the deferred-emit buffer: TV is re-broadcasting
            # the pending (e.g. with new sl/tp) before we flushed the
            # original — refresh the buffered payload.
            if order_id in self._pending_buffer:
                self._pending_buffer[order_id] = {
                    "data": event, "log": log, "meta": pmeta,
                    "ts": self._pending_buffer[order_id]["ts"],
                }
                return
            existing = self.pending_paper.get(order_id)
            if existing is None:
                self._pending_buffer[order_id] = {
                    "data": event, "log": log, "meta": pmeta,
                    "ts": time.time(),
                }
            elif (existing.target != target or existing.sl != sl
                    or existing.tp != tp or existing.volume != qty_abs):
                existing.target = target
                existing.sl = sl; existing.tp = tp
                existing.volume = qty_abs
                self._emit({
                    "ev": "pending_modify",
                    "ticket": order_id,
                    "target": target,
                    "sl": sl, "tp": tp,
                    "volume": qty_abs,
                    "expiry": 0,
                    "ts": now_ms(),
                })
                self._log(f"pending_modify {symbol} ticket={order_id} "
                          f"target={target} sl={sl} tp={tp} (ws)")
            return

        # Buffered-pending fast path: TV filled (or cancelled) within
        # the 200ms hold window so the buffered `pending` event was
        # never written to the slave wire. Triggers ONLY on explicit
        # terminal keywords — `close_date` alone or transient statuses
        # like "working" don't count.
        buf = self._pending_buffer.pop(order_id, None) if is_terminal else None
        if buf:
            if is_cancelled:
                self._log(f"pending {symbol} ticket={order_id} "
                          f"cancelled before emit (ws)")
                return
            pmeta = buf["meta"]
            meta = PositionMeta(
                symbol=symbol, side=pmeta.side, volume=pmeta.volume,
                price=_safe_float(p.get("price") or p.get("avg_price"))
                       or pmeta.target,
                sl=pmeta.sl, tp=pmeta.tp,
                pip_size=pmeta.pip_size,
                order_id=order_id,
            )
            self.positions[order_id] = meta
            self.paper_position_by_symbol.setdefault(symbol, []).append(order_id)
            self._emit_open(order_id, meta)
            self._log(f"open {symbol} {meta.side} {meta.volume:g} lot "
                      f"sl={meta.sl} tp={meta.tp} ticket={order_id} (ws/instant-fill)")
            return

        # Pending termination — order tracked as pending and now
        # filled or cancelled. On fill we seed the position tracker so
        # subsequent modify/close for the resulting position resolve
        # back to this ticket.
        if order_id in self.pending_paper and is_terminal:
            pending = self.pending_paper.pop(order_id)
            if is_cancelled:
                self._emit({"ev": "pending_cancel", "ticket": order_id,
                            "symbol": symbol, "ts": now_ms()})
                self._log(f"pending_cancel {symbol} ticket={order_id} (ws)")
                return
            self._emit({
                "ev": "pending_fill",
                "ticket": order_id,
                "symbol": symbol,
                "position_ticket": order_id,
                "ts": now_ms(),
            })
            self._log(f"pending_fill {symbol} ticket={order_id} (ws)")
            meta = PositionMeta(
                symbol=symbol, side=pending.side, volume=pending.volume,
                price=_safe_float(p.get("price") or p.get("avg_price")),
                sl=pending.sl, tp=pending.tp,
                pip_size=pending.pip_size,
                order_id=order_id,
            )
            self.positions[order_id] = meta
            self.paper_position_by_symbol.setdefault(symbol, []).append(order_id)
            return

        # close-date alone is the order-completed flag, NOT a
        # position-closed flag — the entry market order also gets a
        # close-date stamp at fill time. Position closes are detected
        # via `_on_paper_position_update` (qty=0).
        if close_date is not None:
            return

        # Market open path. Only true market orders reach here —
        # limit/stop/stoplimit went through the pending lifecycle.
        # Multiple concurrent buys (or sells) on the same symbol each
        # spawn a separate slave position; TV nets them but the slave
        # keeps them distinct. Reverse-side places (sell into an
        # existing long) reduce/close existing tickets via the
        # position_update qty path instead.
        if is_pending_kind:
            return  # safety: pending types should never reach market path
        if not order_id:
            return
        if order_id in self.positions or order_id in self._skipped_reverse:
            return
        if self._opposing_existing(symbol, side):
            self._skipped_reverse.add(order_id)
            self._log(f"reverse-side market skipped ({symbol} {side}) "
                      f"ticket={order_id} (ws)")
            return
        meta = PositionMeta(
            symbol=symbol, side=side, volume=qty_abs,
            price=_safe_float(p.get("price") or p.get("avg_price")),
            sl=sl, tp=tp,
            pip_size=pip_size,
            order_id=order_id,
        )
        self.positions[order_id] = meta
        self.paper_position_by_symbol.setdefault(symbol, []).append(order_id)
        self._emit_open(order_id, meta)
        self._log(f"open {symbol} {side} {qty_abs:g} lot "
                  f"sl={sl} tp={tp} ticket={order_id} (ws)")

    def _on_paper_position_update(self, p: dict[str, Any]) -> None:
        # Authoritative net position snapshot per symbol. Drives close
        # (qty=0), open-when-we-missed-the-order_update (synth ticket),
        # and partial close detection. Sl/tp can live in `levels[0]`
        # (multi-bracket) or top-level (single-bracket); a MISSING
        # key preserves meta — TV emits transient frames without sl/tp
        # during a modification and treating "absent" as 0 would
        # falsely strip the level on the slave. Short qty is negative;
        # we emit abs() because the wire carries direction in `side`
        # separately. SL/TP modifies are NOT routed here — TV's
        # transient merge frames flood with sl=0 noise; the
        # order_update label="sl"|"tp" child path is authoritative.
        symbol = self._strip_exchange(p.get("symbol", ""))
        if not symbol:
            return
        symboltype = str(p.get("symboltype") or "forex")
        qty = abs(self._qty_to_lots(_safe_float(p.get("qty")), symboltype))
        side = to_side(p.get("side", ""))
        avg = _safe_float(p.get("avg_price"))

        tickets = self.paper_position_by_symbol.get(symbol, [])

        # Close: TV sets qty=0 on the position_update emitted right
        # after a full close (or full reverse). Cascade-close every
        # slave ticket we have for this symbol.
        if qty <= 0:
            if tickets:
                for t in tickets:
                    self.positions.pop(t, None)
                    self._emit_close(t, 0.0)
                self.paper_position_by_symbol.pop(symbol, None)
                self._log(f"close {symbol} tickets={','.join(tickets)} (ws/pos)")
            return

        # Resolve sl/tp with field-presence semantics: only treat as
        # "explicit 0" if the key exists in the frame.
        levels = p.get("levels") or []
        if levels and isinstance(levels[0], dict):
            lvl = levels[0]
            sl_present = "sl" in lvl
            tp_present = "tp" in lvl
            sl_new = _safe_float(lvl.get("sl"))
            tp_new = _safe_float(lvl.get("tp"))
        else:
            sl_present = "sl" in p
            tp_present = "tp" in p
            sl_new = _safe_float(p.get("sl"))
            tp_new = _safe_float(p.get("tp"))

        if not tickets:
            # Synthetic open — we missed the order_update (rare, e.g.
            # WS subscribed mid-trade). Build a stable account-keyed
            # ticket so close routes back here later.
            ticket = f"paper-{self.account_id}-{symbol}"
            meta = PositionMeta(
                symbol=symbol, side=side, volume=qty,
                price=avg,
                sl=sl_new if sl_present else 0.0,
                tp=tp_new if tp_present else 0.0,
                pip_size=self.pip_sizes.get(symbol, 0.0),
                order_id=ticket,
            )
            self.positions[ticket] = meta
            self.paper_position_by_symbol.setdefault(symbol, []).append(ticket)
            self._emit_open(ticket, meta)
            self._log(f"open {symbol} {side} {qty:g} lot "
                      f"sl={meta.sl} tp={meta.tp} ticket={ticket} (ws/pos)")
            return

        # Partial close detection: when TV's net qty drops below the
        # sum of our slave tickets, the user has placed a reverse
        # market that reduces the netted position. Match it by closing
        # slave tickets LIFO until totals align.
        total = sum(self.positions[t].volume for t in tickets if t in self.positions)
        if total - qty > 1e-6:
            self._partial_close_lifo(symbol, total - qty)

    def _handle_cmd(self, line: str) -> None:
        # cmd.jsonl carries `{op, …}` frames written by Rust's file_bridge.
        # `subscribe` drives quote streaming; `list_symbols` answers the
        # Compare panel's "what can this account trade?" probe. Other
        # ops are master-side and don't apply to a read-only TV bridge.
        line = line.strip()
        if not line:
            return
        try:
            cmd = json.loads(line)
        except Exception:
            return
        op = cmd.get("op")
        if op == "subscribe":
            self._cmd_subscribe(cmd.get("symbols") or [])
        elif op == "list_symbols":
            self._cmd_list_symbols()

    def _cmd_subscribe(self, symbols: list) -> None:
        new_subs = {str(s).upper() for s in symbols if s}
        if new_subs == self.quote_subs:
            return
        self.quote_subs = new_subs
        self._log(f"quote subs: {sorted(new_subs) if new_subs else 'none'}")
        # Replay last-known quotes so the engine doesn't have to wait
        # for the next tick to see a value (TV ticks are sparse on
        # slow markets). For each requested symbol we look for an
        # exact cache hit first; failing that, scan the cache for a
        # `EXCHANGE:SYM` or `SYM*BROKER` variant — same matching
        # tolerance as the live qsd path.
        for sym in new_subs:
            q = self.quote_cache.get(sym)
            if not q:
                for cached_sym, candidate in self.quote_cache.items():
                    bare = cached_sym
                    colon = bare.find(":")
                    if colon > 0: bare = bare[colon + 1:]
                    star = bare.find("*")
                    if star > 0: bare = bare[:star]
                    if bare == sym:
                        q = candidate
                        break
            if q and q.get("bid", 0) > 0 and q.get("ask", 0) > 0:
                self._emit({
                    "ev": "quote",
                    "symbol": sym,
                    "bid": q["bid"],
                    "ask": q["ask"],
                    "pip_size": q.get("pip_size", 0.0),
                    "ts": now_ms(),
                })

    def _cmd_list_symbols(self) -> None:
        # Defer the symbols dump by ~3s. TV streams qsd ticks lazily
        # — at the moment Cascada asks "what can this account trade?"
        # the cache is often only partly populated, especially right
        # after the bridge configured. Without the wait, users had
        # to click ↻ Symbols 2–3 times to see all feeds. The thread
        # is daemon so a Cascada restart won't block on it. Repeat
        # calls coalesce: only the latest schedule fires.
        seq = getattr(self, "_list_symbols_seq", 0) + 1
        self._list_symbols_seq = seq
        def _emit_after_wait():
            time.sleep(3.0)
            if seq != self._list_symbols_seq:
                return  # superseded by a fresher request
            symbols = sorted(self.quote_cache.keys())
            self._emit({"ev": "symbols", "symbols": symbols})
            self._log(f"list_symbols → {len(symbols)} symbol(s)")
        threading.Thread(target=_emit_after_wait,
                         name="cascada-list-symbols", daemon=True).start()
