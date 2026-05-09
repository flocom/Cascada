"""mitmproxy addon: TradingView → Cascada bridge.

Bridges a TradingView session (PaperTrading or a TV-broker integration)
into Cascada's local file-bridge JSONL protocol so the TV account can
act as a master fanning trades out to MT4/MT5/cTrader slaves.

Run: `mitmdump -s tv-proxy/cascada_addon.py`. Auto-discovers broker_url
+ account_id from the first TV API hit (REST) or pushstream WS frame
(cross-browser). Manual pin via `--set tv_broker_url=… --set
tv_account_id=…` still wins. Wire schema lives in
`src-tauri/src/connectors/proto.rs::S2C`.

Why no `from __future__ import annotations`: PEP 563 stringified
annotations crash `dataclasses._process_class` on Python 3.12 under
python-build-standalone's Windows build. PEP 585 generics work natively.
"""

import json
import platform
import re
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

try:
    from mitmproxy import ctx, http
except ImportError:  # pragma: no cover — let `python cascada_addon.py --help` work outside mitmproxy
    ctx = None  # type: ignore
    http = None  # type: ignore


# ───────────────────────── Cascada root resolution ─────────────────

def cascada_root_default() -> Path:
    """Mirror `connectors/file_bridge::cascada_root` from the Rust core.

    macOS prefers `~/cAlgo` when it exists (cTrader's native location);
    everything else lands under `~/Documents/cAlgo/Cascada`. The TV folder
    sits at `<root>/TradingView/<login>/`.
    """
    home = Path.home()
    if platform.system() == "Darwin":
        native = home / "cAlgo"
        if native.is_dir():
            return native / "Cascada"
    candidates = [
        home / "Documents" / "cAlgo",
        home / "cAlgo",
    ]
    for c in candidates:
        if c.is_dir():
            return c / "Cascada"
    return home / "Documents" / "cAlgo" / "Cascada"


# ──────────────────────────── Wire helpers ──────────────────────────

def now_ms() -> int:
    return int(time.time() * 1000)


def to_side(s: str) -> str:
    """TV uses lowercase 'buy'/'sell'; Cascada's wire serializes Side as
    capitalised 'Buy'/'Sell'."""
    s = (s or "").strip().lower()
    return "Sell" if s == "sell" else "Buy"


# ────────────────────────────── Auto-discovery pattern ──────────────────────

# Two URL families auto-discovery latches onto, both rooted at a
# `*.tradingview.com` host:
#
#   1. TV-broker integration (FTMO, OANDA, Forex.com, …):
#         `/accounts/<id>/(state|positions|orders|executions|…)`
#      — the canonical API every external broker exposes.
#
#   2. TV PaperTrading (TV's own simulated account):
#         `papertrading.tradingview.com/trading/<verb>/<id>`
#      — completely different URL shape (`<id>` here is the account
#        number, not an order id), so we need a second pattern. Verbs
#        observed in DevTools traces: place, modify, close, cancel,
#        positions, orders, state, history, instruments.
#
# A single hit on either family lets us snap broker_url + account_id
# without the user setting any flag. Hitting both is fine: the first
# match wins (we early-out once `broker_url` is set).
_DISCOVER_BROKER_RE = re.compile(
    r'^https?://([^/]+)/accounts/([^/?]+)/'
    r'(?:state|positions|orders|executions|instruments|history|preferences|configuration)\b'
)
_DISCOVER_PAPER_RE = re.compile(
    r'^https?://(papertrading\.tradingview\.com)/trading/'
    # Verbs observed in the wild include `close_position`, `modify_position`,
    # `cancel_order` etc. — match a generic `<word>` to catch them all,
    # then require an account-id-shaped tail (digits or hex).
    r'(?:[a-z_]+)/(\d+|[A-Za-z0-9_-]+)\b'
)


# ────────────────────────────── State tracking ─────────────────────────────

@dataclass
class PositionMeta:
    symbol: str = ""
    side: str = "Buy"
    volume: float = 0.0
    price: float = 0.0
    sl: float = 0.0
    tp: float = 0.0
    pip_size: float = 0.0
    comment: str = ""
    # TV-broker emits orderId first; /executions later supplies positionId.
    # We index positions by order_id so SL/TP child deletions resolve.
    order_id: str = ""


@dataclass
class PendingMeta:
    symbol: str = ""
    side: str = "Buy"
    volume: float = 0.0
    target: float = 0.0
    sl: float = 0.0
    tp: float = 0.0
    expiry: int = 0
    order_type: str = "Limit"
    pip_size: float = 0.0
    comment: str = ""


@dataclass
class TVBridge:
    broker_url: str = ""
    account_id: str = ""
    cascada_root_override: str = ""
    out_dir: Path = field(default_factory=cascada_root_default)
    pip_sizes: dict[str, float] = field(default_factory=dict)
    pending_opens: dict[str, PositionMeta] = field(default_factory=dict)
    positions: dict[str, PositionMeta] = field(default_factory=dict)
    # PaperTrading is a netting model: each (symbol) holds at most one
    # net position, addressed by symbol — never by ticket. We bridge to
    # the ticket-based Cascada wire by remembering which order id we
    # originally emitted as the open ticket for each live symbol; later
    # modify/close requests reuse it instead of inventing a new one.
    paper_position_by_symbol: dict[str, str] = field(default_factory=dict)
    # Pending limit/stop orders broadcast to the slave wire. Keyed by TV
    # order id; values track last-emitted target/sl/tp for modify diff.
    # On fill the entry migrates into `positions`; on cancel it's popped.
    pending_paper: dict[str, "PendingMeta"] = field(default_factory=dict)
    auth_token: str = ""
    welcomed: bool = False
    last_balance: float = 0.0
    last_equity: float = 0.0
    last_currency: str = "USD"
    write_lock: threading.Lock = field(default_factory=threading.Lock)
    _skipped_hosts: set[str] = field(default_factory=set)
    # Quote streaming (Quote-compare feature). The slave-side connector
    # writes `{op:"subscribe", symbols:[…]}` to cmd.jsonl whenever the
    # set of mirrored symbols changes; we tail that file from a thread,
    # latch the set, and emit `Quote` events on every prodata `qsd`
    # frame for symbols in the set.
    quote_subs: set[str] = field(default_factory=set)
    quote_cache: dict[str, dict[str, float]] = field(default_factory=dict)
    # Pending-emit buffer. TV happily accepts pending orders that are
    # already past their trigger and fills them within milliseconds.
    # We hold each fresh `pending` event briefly (~200ms) before
    # writing it to the wire; if a `pending_fill` for the same id
    # arrives in that window we drop the buffered pending and emit a
    # plain `open` instead — the slave broker would have rejected the
    # pending placement anyway ("Invalid price") and we'd be left
    # without a mirror.
    _pending_buffer: dict[str, dict[str, Any]] = field(default_factory=dict)

    # ─── Lifecycle ──────────────────────────────────────────────────

    def configure(self, broker_url: str, account_id: str, out_root: str = "") -> None:
        self.broker_url = broker_url.rstrip("/")
        self.account_id = account_id
        root = Path(out_root) if out_root else cascada_root_default()
        self.out_dir = root / "TradingView" / account_id
        self.out_dir.mkdir(parents=True, exist_ok=True)
        # Truncate cmd.jsonl on attach so a stale Cascada session doesn't
        # bombard us with replayed commands. Same convention as Rust's
        # `file_bridge::spawn_with_dir`.
        (self.out_dir / "cmd.jsonl").write_text("", encoding="utf-8")
        self._log(f"bridge → {self.out_dir}")

    # ─── HTTP entry points (mitmproxy hooks) ────────────────────────────

    def base_path(self) -> str:
        return f"{self.broker_url}/accounts/{self.account_id}"

    def request(self, flow: "http.HTTPFlow") -> None:  # type: ignore[name-defined]
        if not (self.broker_url and self.account_id):
            self._try_auto_discover(flow)

        # Capture freshest bearer token for the TV-broker `/state` poll
        # (PaperTrading runs through the WS path and doesn't need it).
        if self.broker_url and self.broker_url in flow.request.pretty_url:
            tok = flow.request.headers.get("authorization")
            if tok:
                self.auth_token = tok

        url = flow.request.pretty_url
        method = flow.request.method

        if not self._is_relevant(flow):
            return

        if method == "DELETE":
            if "/positions/" in url:
                pid = url.rsplit("/", 1)[-1].split("?")[0]
                self._on_close(pid, dict(flow.request.urlencoded_form or {}))
            elif "/orders/" in url and (".SL." in url or ".TP." in url):
                tail = url.split("/orders/", 1)[-1].split("?", 1)[0]
                parts = tail.split(".")
                if len(parts) >= 2:
                    self._on_tpsl_delete(parts[0], parts[1].upper())

    def response(self, flow: "http.HTTPFlow") -> None:  # type: ignore[name-defined]
        if not self._is_relevant(flow):
            return

        body = flow.response.content if flow.response else None
        if not body:
            return
        try:
            data = json.loads(body.decode("utf-8"))
        except Exception:
            return

        method = flow.request.method
        url = flow.request.pretty_url

        # PaperTrading: bodies arrive as JSON despite the
        # `application/x-www-form-urlencoded` Content-Type (TV bug).
        if "papertrading.tradingview.com" in url and "/trading/" in url:
            verb = url.split("/trading/", 1)[-1].split("/", 1)[0].split("?", 1)[0]
            if method != "POST":
                return
            req_body = self._parse_paper_body(flow.request.content)
            if verb == "place":
                self._on_paper_place(req_body, data)
            elif verb == "close_position":
                self._on_paper_close(req_body, data)
            elif verb == "modify_position":
                self._on_paper_modify(req_body, data)
            # `cancel` is handled via the WS pending_cancel path.
            return

        if method == "POST" and "/orders?" in url:
            self._on_order(dict(flow.request.urlencoded_form or {}), data)
        elif method == "POST" and "/executions?" in url:
            self._on_execution(data)
        elif method == "PUT" and "/positions/" in url:
            pid = url.rsplit("/", 1)[-1].split("?")[0]
            self._on_modify(pid, dict(flow.request.urlencoded_form or {}), data)

    # ─── Auto-discovery ───────────────────────────────────────────────

    def _try_auto_discover(self, flow: "http.HTTPFlow") -> None:  # type: ignore[name-defined]
        url = flow.request.pretty_url

        # Try TV-broker pattern first (most common across integrations).
        m = _DISCOVER_BROKER_RE.match(url)
        if m:
            host, account_id = m.group(1), m.group(2)
            # Sanity filter: the request must look like it really comes
            # from TV. Accept either tradingview.com in the host, or in
            # referer/origin (covers browser XHR and service-worker /
            # server-side calls that lack those headers).
            ref = (flow.request.headers.get("referer", "")
                   + "|" + flow.request.headers.get("origin", ""))
            if "tradingview.com" not in host and "tradingview.com" not in ref:
                if host not in self._skipped_hosts:
                    self._skipped_hosts.add(host)
                    self._log(f"discovery skipped (no tradingview.com in referer/origin/host): {host} {account_id}")
                return
            self._log(f"auto-discovered TV broker={host} account={account_id}")
            self.configure(host, account_id, self.cascada_root_override)
            return

        # Fall back to PaperTrading pattern.
        m = _DISCOVER_PAPER_RE.match(url)
        if m:
            host, account_id = m.group(1), m.group(2)
            self._log(f"auto-discovered TV PaperTrading account={account_id}")
            self.configure(host, account_id, self.cascada_root_override)
            return


    # ─── Event handlers ──────────────────────────────────────────────

    def _on_order(self, req: dict[str, Any], resp: dict[str, Any]) -> None:
        d = (resp or {}).get("d") or {}
        order_id = str(d.get("orderId") or "")
        if not order_id:
            return
        symbol = (req.get("instrument") or "").upper()
        meta = PositionMeta(
            symbol=symbol,
            side=to_side(req.get("side", "")),
            volume=_safe_float(req.get("qty")),
            price=_safe_float(req.get("currentAsk") or req.get("currentBid")),
            sl=_safe_float(req.get("stopLoss")),
            tp=_safe_float(req.get("takeProfit")),
            pip_size=self.pip_sizes.get(symbol, 0.0),
            comment=str(req.get("note") or req.get("comment") or ""),
            order_id=order_id,
        )
        self.pending_opens[order_id] = meta

    def _on_execution(self, resp: dict[str, Any]) -> None:
        for ex in (resp or {}).get("d") or []:
            order_id = str(ex.get("orderId") or "")
            position_id = str(ex.get("positionId") or "")
            if not position_id:
                continue
            meta = self.pending_opens.pop(order_id, None) or self.positions.get(position_id)
            if not meta:
                continue
            price = _safe_float(ex.get("price"))
            if price:
                meta.price = price
            self.positions[position_id] = meta
            if ex.get("isClose"):
                self._emit_close(position_id, _safe_float(ex.get("profit")))
                continue
            self._emit_open(position_id, meta)

    def _on_modify(self, pid: str, req: dict[str, Any], resp: dict[str, Any]) -> None:
        if isinstance(resp, dict) and resp.get("s") == "error":
            self._log(f"TV rejected modify on {pid}: {resp.get('errmsg', 'unknown')}")
            return
        meta = self.positions.get(pid)
        if meta is None:
            meta = PositionMeta()
            self.positions[pid] = meta
        if "stopLoss" in req:
            meta.sl = _safe_float(req["stopLoss"])
        if "takeProfit" in req:
            meta.tp = _safe_float(req["takeProfit"])
        self._emit({
            "ev": "modify",
            "ticket": pid,
            "sl": meta.sl,
            "tp": meta.tp,
            "volume": meta.volume,
        })

    def _on_close(self, pid: str, _close_data: dict[str, Any]) -> None:
        # Cascada needs the realised P/L; TV doesn't send it on the DELETE
        # itself, so we leave it 0 (the engine treats missing profit as 0.0
        # and updates separately if a follow-up /executions arrives with
        # `isClose: true`).
        self._emit_close(pid, 0.0)
        self.positions.pop(pid, None)

    def _on_tpsl_delete(self, order_id: str, level: str) -> None:
        pid = ""
        for p, m in self.positions.items():
            if m.order_id == order_id:
                pid = p
                break
        if not pid:
            return
        meta = self.positions[pid]
        if level == "SL":
            meta.sl = 0.0
        elif level == "TP":
            meta.tp = 0.0
        self._emit({"ev": "modify", "ticket": pid, "sl": meta.sl, "tp": meta.tp, "ts": now_ms()})

    # ─── PaperTrading handlers ──────────────────────────────────────────────
    #
    # PaperTrading endpoints (POST):
    #   /trading/place           { symbol, type, qty, side, price, sl, tp, … }
    #   /trading/cancel          { id }
    #   /trading/close_position  { symbol, qty? }
    #   /trading/modify_position { symbol, stopLoss, takeProfit }
    # Bodies are JSON despite the misadvertised `x-www-form-urlencoded`
    # Content-Type. Symbols arrive exchange-prefixed (`OANDA:AUDCAD`)
    # and are stripped to match broker symbols on the slave side.

    def _parse_paper_body(self, raw: bytes | None) -> dict[str, Any]:
        if not raw:
            return {}
        try:
            return json.loads(raw.decode("utf-8"))
        except Exception:
            return {}

    @staticmethod
    def _strip_exchange(symbol: str) -> str:
        s = (symbol or "").upper()
        return s.split(":", 1)[1] if ":" in s else s

    @staticmethod
    def _qty_to_lots(qty: float, symboltype: str) -> float:
        # Forex on TV PaperTrading is reported in base units (1 lot =
        # 100 000), so qty=1000 → 0.01 lot. Crypto/spot are already in
        # instrument units that match MT5 lot convention (BTC qty=1.0111
        # → 1.0111 lot). Metals (XAU lot=100 oz) is TBD when we see a
        # sample.
        return qty / 100_000.0 if (symboltype or "").lower() == "forex" else qty

    def _on_paper_place(self, req: dict[str, Any], resp: dict[str, Any]) -> None:
        # PaperTrading reflows every order through /trading/place,
        # including the counter-order TV uses internally to close. We
        # only ever see the OPENING intent here because the user-driven
        # close goes through /trading/close_position first.
        symbol = self._strip_exchange(req.get("symbol", ""))
        if not symbol:
            return
        order_id = str((resp or {}).get("id") or "")
        if not order_id:
            return
        symboltype = str((resp or {}).get("symboltype") or req.get("symboltype") or "forex")
        meta = PositionMeta(
            symbol=symbol,
            side=to_side(req.get("side", "")),
            volume=self._qty_to_lots(_safe_float(req.get("qty")), symboltype),
            price=_safe_float(req.get("price")),
            sl=_safe_float(req.get("stopLoss") or req.get("sl") or req.get("stop_loss")),
            tp=_safe_float(req.get("takeProfit") or req.get("tp") or req.get("take_profit")),
            pip_size=self.pip_sizes.get(symbol, 0.0),
            comment=str(req.get("comment") or req.get("note") or ""),
            order_id=order_id,
        )
        # Limit orders are picked up via the WS pending lifecycle —
        # nothing to emit from REST.
        if str(req.get("type") or "").lower() == "limit":
            return
        self.positions[order_id] = meta
        self.paper_position_by_symbol[symbol] = order_id
        self._emit_open(order_id, meta)
        self._log(f"open {symbol} {meta.side} {meta.volume:g} lot "
                  f"sl={meta.sl} tp={meta.tp} ticket={order_id}")

    def _on_paper_close(self, req: dict[str, Any], _resp: dict[str, Any]) -> None:
        # Netting model: request identifies target by `symbol`, not ticket.
        # close_qty is in raw forex units (TV doesn't echo symboltype on
        # close — assume forex which is the only type where qty != lots).
        symbol = self._strip_exchange(req.get("symbol", ""))
        ticket = self.paper_position_by_symbol.get(symbol)
        meta = self.positions.get(ticket) if ticket else None
        if not meta:
            return
        close_qty = self._qty_to_lots(_safe_float(req.get("qty")), "forex")
        if close_qty <= 0 or close_qty >= meta.volume:
            self.positions.pop(ticket, None)
            self.paper_position_by_symbol.pop(symbol, None)
            self._emit_close(ticket, 0.0)
            self._log(f"close {symbol} ticket={ticket}")
            return
        # Partial close: close the master ticket and re-open a smaller
        # one — the wire has no native partial-close event.
        self.positions.pop(ticket, None)
        self._emit_close(ticket, 0.0)
        new_ticket = f"{ticket}-r"
        new_volume = max(meta.volume - close_qty, 0.0)
        if new_volume <= 0:
            self.paper_position_by_symbol.pop(symbol, None)
            self._log(f"close {symbol} ticket={ticket}")
            return
        new_meta = PositionMeta(
            symbol=meta.symbol, side=meta.side, volume=new_volume,
            price=meta.price, sl=meta.sl, tp=meta.tp,
            pip_size=meta.pip_size, comment=meta.comment,
            order_id=new_ticket,
        )
        self.positions[new_ticket] = new_meta
        self.paper_position_by_symbol[symbol] = new_ticket
        self._emit_open(new_ticket, new_meta)
        self._log(f"partial close {symbol} ticket={ticket} "
                  f"-> remainder {new_volume:g} lot ticket={new_ticket}")

    def _on_paper_modify(self, req: dict[str, Any], _resp: dict[str, Any]) -> None:
        symbol = self._strip_exchange(req.get("symbol", ""))
        ticket = self.paper_position_by_symbol.get(symbol)
        meta = self.positions.get(ticket) if ticket else None
        if not meta:
            return
        sl = req.get("sl") or req.get("stopLoss") or req.get("stop_loss")
        tp = req.get("tp") or req.get("takeProfit") or req.get("take_profit")
        if sl is not None:
            meta.sl = _safe_float(sl)
        if tp is not None:
            meta.tp = _safe_float(tp)
        self._emit({"ev": "modify", "ticket": ticket,
                    "sl": meta.sl, "tp": meta.tp, "ts": now_ms()})
        self._log(f"modify {symbol} ticket={ticket} sl={meta.sl} tp={meta.tp}")

    # ─── WebSocket bridge ─────────────────────────────────────────────
    #
    # TV restricts each user to one active REST session. The browser
    # the user keeps in front gets the `/trading/account` API; every
    # other client (incl. Cascada-Chrome on the same login) sees 401.
    # The WebSocket pushstream stays alive regardless and broadcasts
    # the full order/position lifecycle to every connected client of
    # the account, so we read trades off it instead of polling REST.
    def on_websocket_message(self, flow: "http.HTTPFlow") -> None:  # type: ignore[name-defined]
        ws = getattr(flow, "websocket", None)
        if ws is None or not ws.messages:
            return
        msg = ws.messages[-1]
        host = flow.request.host if flow.request else ""
        if msg.from_client or not msg.is_text:
            return
        text = (msg.content.decode("utf-8", errors="replace")
                if isinstance(msg.content, bytes) else str(msg.content))

        # prodata: chart-data WS, carries `~m~LEN~m~JSON` frames. Quotes
        # arrive as `m=qsd` payloads we surface as Quote events for the
        # symbols the slave has subscribed to.
        if "prodata.tradingview" in host or "data.tradingview" in host:
            for chunk in re.findall(r'~m~\d+~m~(\{[^~]*\})', text):
                try:
                    obj = json.loads(chunk)
                except Exception:
                    continue
                if isinstance(obj, dict) and obj.get("m") == "qsd":
                    self._on_qsd(obj.get("p") or [])
            return

        # pushstream: trade lifecycle WS.
        if "pushstream" in host:
            try:
                obj = json.loads(text)
            except Exception:
                return
            if isinstance(obj, dict):
                self._on_pushstream_frame(obj.get("text") or {})

    def _on_qsd(self, params: list) -> None:
        # qsd payload: ["qs_session", {"n":"<exchange>:<sym>", "v":{bid,ask,
        # pricescale,…}, "s":"ok"}]. `v` is a delta — bid/ask only appear
        # when they tick. Cache last-known so we emit a complete Quote
        # frame on every update for subscribed symbols.
        if len(params) < 2 or not isinstance(params[1], dict):
            return
        payload = params[1]
        symbol = self._strip_exchange(payload.get("n", ""))
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
        # pricescale=10^precision (TV convention). Forex 5-digit gives
        # pricescale=100000 → pip_size=0.0001 (one tenth of the smallest
        # quoted increment — the standard fx pip).
        ps = v.get("pricescale")
        if isinstance(ps, (int, float)) and ps > 0:
            cached["pip_size"] = 10.0 / float(ps)
        if (symbol in self.quote_subs
                and cached["bid"] > 0 and cached["ask"] > 0):
            self._emit({
                "ev": "quote",
                "symbol": symbol,
                "bid": cached["bid"],
                "ask": cached["ask"],
                "pip_size": cached["pip_size"],
                "ts": now_ms(),
            })

    def _on_pushstream_frame(self, txt: dict[str, Any]) -> None:
        # The `trading` channel envelope is `{channel, content:{m, p, accountId}}`.
        # Only `order_update` and `position_update` are routed: account_change
        # / execution / balance / journal are either redundant or fire on
        # partial closes (which would mis-emit a full close to the slave).
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

    def _on_paper_order_update(self, p: dict[str, Any]) -> None:
        # Frame shape: { id, symbol, symboltype, side, qty, type:
        # "market"|"limit"|"stop"|"stoplimit", status: "pending"|"filled"|
        # "canceled"|…, sl, tp, close-date, label, parent }. TV's
        # PaperTrading is a netting account so the entry order id is
        # reused as the position ticket for the lifetime of the position.
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

        # Child SL/TP order. TV emits one whenever the user sets, moves
        # or REMOVES a stop / take-profit on an open position. Removal
        # arrives as a child `order_update` with status=cancelled (or
        # close-date set) — the price field still carries the OLD level,
        # so we must read the cancellation signal explicitly to know the
        # level is gone. Without this, removing a TP would silently
        # leave the slave's TP intact.
        if label in ("sl", "tp"):
            parent_id = str(p.get("parent") or "")
            ticket = self._ticket_for_order_id(parent_id)
            if not ticket:
                # Parent is a pending we're tracking — TV will also
                # re-broadcast the parent with the new sl/tp, which
                # our pending branch below picks up.
                return
            meta = self.positions.get(ticket)
            if not meta:
                return
            cancelled = (any(k in status for k in ("cancel", "reject", "expir"))
                         or close_date is not None)
            new_val = 0.0 if cancelled else _safe_float(p.get("price"))
            changed = False
            if label == "sl" and new_val != meta.sl:
                meta.sl = new_val; changed = True
            elif label == "tp" and new_val != meta.tp:
                meta.tp = new_val; changed = True
            if not changed:
                return
            self._emit({"ev": "modify", "ticket": ticket,
                        "sl": meta.sl, "tp": meta.tp, "ts": now_ms()})
            self._log(f"modify {symbol} ticket={ticket} "
                      f"sl={meta.sl} tp={meta.tp} (ws)")
            return

        # Pending order lifecycle. TV reports `status="pending"` for
        # limit/stop orders that haven't filled yet; we mirror those
        # to the slave as actual pending orders (BuyLimit/SellLimit/
        # BuyStop/SellStop) instead of waiting for the fill and
        # placing a market open at the wrong price.
        is_pending_kind = order_type in ("limit", "stop", "stoplimit")
        if is_pending_kind and status == "pending" and order_id:
            order_kind = ("Limit" if order_type == "limit"
                          else "StopLimit" if order_type == "stoplimit"
                          else "Stop")
            # Wrong-side trigger detection: TV accepts orders that are
            # already past their trigger (e.g. SellStop above current bid)
            # and fills them instantly. The slave broker rejects those
            # as "Invalid price" because MT5/cTrader enforce side rules.
            # Detect via live quote cache and fall through to the
            # market open path so the slave opens at its market price.
            if self._already_triggered(symbol, side, order_kind, target):
                self._log(f"wrong-side {order_kind} → market open "
                          f"({symbol} {side} target={target}) ticket={order_id}")
            else:
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
                # the pending (e.g. with new sl/tp) before we even flushed
                # the original — just refresh the buffered payload.
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

        # Buffered-pending fast path: TV filled (or cancelled) before
        # we got a chance to emit the `pending` event to the slave.
        # On fill we emit a plain `open` so the slave opens at market
        # instead of trying to place an already-triggered pending. On
        # cancel we emit nothing — the slave never knew about it.
        buf = self._pending_buffer.pop(order_id, None) if (
                close_date is not None or
                (status and status != "pending")) else None
        if buf:
            if any(k in status for k in ("cancel", "reject", "expir")):
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
            self.paper_position_by_symbol[symbol] = order_id
            self._emit_open(order_id, meta)
            self._log(f"open {symbol} {meta.side} {meta.volume:g} lot "
                      f"sl={meta.sl} tp={meta.tp} ticket={order_id} (ws/instant-fill)")
            return

        # Pending termination — the order was tracked as pending and
        # status flipped (filled / cancelled / rejected / expired).
        # We split fill vs cancel by the status keyword; on fill we
        # also seed the position tracker so subsequent modify/close
        # for the resulting position resolve back to this ticket.
        if order_id in self.pending_paper and (
                close_date is not None or
                (status and status != "pending")):
            pending = self.pending_paper.pop(order_id)
            if any(k in status for k in ("cancel", "reject", "expir")):
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
            if symbol not in self.paper_position_by_symbol:
                meta = PositionMeta(
                    symbol=symbol, side=pending.side, volume=pending.volume,
                    price=_safe_float(p.get("price") or p.get("avg_price")),
                    sl=pending.sl, tp=pending.tp,
                    pip_size=pending.pip_size,
                    order_id=order_id,
                )
                self.positions[order_id] = meta
                self.paper_position_by_symbol[symbol] = order_id
            return

        # See note: close-date alone is the order-completed flag, NOT
        # a position-closed flag — the entry market order also gets a
        # close-date stamp at fill time. Position closes are detected
        # via `_on_paper_position_update` (qty=0).
        if close_date is not None:
            return

        # Market open path. Limit/stop orders with status != "pending"
        # were already handled above; a non-pending market order is
        # a fresh open we register with the order id as ticket.
        if symbol in self.paper_position_by_symbol:
            return  # already tracked
        if not order_id:
            return
        meta = PositionMeta(
            symbol=symbol, side=side, volume=qty_abs,
            price=_safe_float(p.get("price") or p.get("avg_price")),
            sl=sl, tp=tp,
            pip_size=pip_size,
            order_id=order_id,
        )
        self.positions[order_id] = meta
        self.paper_position_by_symbol[symbol] = order_id
        self._emit_open(order_id, meta)
        self._log(f"open {symbol} {side} {qty_abs:g} lot "
                  f"sl={sl} tp={tp} ticket={order_id} (ws)")

    def _on_paper_position_update(self, p: dict[str, Any]) -> None:
        # Authoritative net position snapshot per symbol. Drives close
        # (qty=0), open-when-we-missed-the-order_update (synth ticket),
        # and SL/TP modify. Sl/tp can live in `levels[0]` (multi-bracket)
        # or top-level (single-bracket); a MISSING key preserves meta —
        # TV emits transient frames without sl/tp during a modification
        # and treating "absent" as 0 would falsely strip the level on
        # the slave. Short qty is negative; we emit abs() because the
        # wire carries direction in `side` separately.
        symbol = self._strip_exchange(p.get("symbol", ""))
        if not symbol:
            return
        symboltype = str(p.get("symboltype") or "forex")
        qty = abs(self._qty_to_lots(_safe_float(p.get("qty")), symboltype))
        side = to_side(p.get("side", ""))
        avg = _safe_float(p.get("avg_price"))

        ticket = self.paper_position_by_symbol.get(symbol)

        # Close: TV sets qty=0 on the position_update emitted right
        # after a full close. Authoritative — partial closes keep
        # qty>0 and are surfaced via the volume diff (TODO: wire a
        # partial_close event; for now the slave keeps full size).
        if qty <= 0:
            if ticket:
                self.positions.pop(ticket, None)
                self.paper_position_by_symbol.pop(symbol, None)
                self._emit_close(ticket, 0.0)
                self._log(f"close {symbol} ticket={ticket} (ws/pos)")
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

        if ticket is None:
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
            self.paper_position_by_symbol[symbol] = ticket
            self._emit_open(ticket, meta)
            self._log(f"open {symbol} {side} {qty:g} lot "
                      f"sl={meta.sl} tp={meta.tp} ticket={ticket} (ws/pos)")
            return

        meta = self.positions.get(ticket)
        if meta is None:
            return
        sl = sl_new if sl_present else meta.sl
        tp = tp_new if tp_present else meta.tp
        if sl != meta.sl or tp != meta.tp:
            meta.sl = sl
            meta.tp = tp
            self._emit({"ev": "modify", "ticket": ticket,
                        "sl": sl, "tp": tp, "ts": now_ms()})
            self._log(f"modify {symbol} ticket={ticket} sl={sl} tp={tp} (ws/pos)")

    def _already_triggered(self, symbol: str, side: str,
                           order_kind: str, target: float) -> bool:
        # True when the requested target is on the wrong side of the
        # current quote and would fire instantly. Slave brokers reject
        # such orders ("Invalid price"), so we drop the pending and let
        # the regular market `open` path take over. Returns False when
        # we haven't seen any quote for the symbol yet — better to try
        # the pending and let the slave decide than to mis-classify.
        if target <= 0:
            return False
        q = self.quote_cache.get(symbol)
        if not q:
            return False
        bid, ask = q.get("bid", 0.0), q.get("ask", 0.0)
        if bid <= 0 or ask <= 0:
            return False
        is_sell = (side == "Sell")
        is_stop = order_kind in ("Stop", "StopLimit")
        # SellStop fires on bid <= target (price fell), SellLimit on
        # bid >= target (price rose); BuyStop on ask >= target,
        # BuyLimit on ask <= target.
        if is_sell:
            return bid <= target if is_stop else bid >= target
        return ask >= target if is_stop else ask <= target

    def _ticket_for_order_id(self, order_id: str) -> str:
        """Resolve an order id back to the ticket we're tracking. The
        SL/TP child orders point at the parent via `parent`, which is
        either the entry-order id we already keyed positions by, or a
        synthetic `paper-<account>-<symbol>` ticket from the legacy
        /trading/account poll path."""
        if not order_id:
            return ""
        if order_id in self.positions:
            return order_id
        # Fall back to symbol lookup if the position was discovered
        # via the poll (synthetic ticket) and the parent doesn't match
        # directly — the meta still carries the live order_id field.
        for ticket, meta in self.positions.items():
            if meta.order_id == order_id:
                return ticket
        return ""

    # ─── Emitters (Cascada wire format) ─────────────────────────────────────

    def _emit_open(self, ticket: str, meta: PositionMeta) -> None:
        if not self.welcomed:
            self._emit_welcome()
        self._emit({
            "ev": "open",
            "ticket": ticket,
            "symbol": meta.symbol,
            "side": meta.side,
            "volume": meta.volume,
            "price": meta.price,
            "sl": meta.sl,
            "tp": meta.tp,
            "ts": now_ms(),
            "pip_size": meta.pip_size,
            "comment": meta.comment,
        })

    def _emit_close(self, ticket: str, profit: float) -> None:
        self._emit({"ev": "close", "ticket": ticket, "profit": profit, "ts": now_ms()})

    def _emit_welcome(self) -> None:
        self.welcomed = True
        self._emit({
            "ev": "welcome",
            "balance": self.last_balance,
            "equity": self.last_equity,
            "currency": self.last_currency,
            "account": self.account_id,
        })

    def _handle_cmd(self, line: str) -> None:
        # cmd.jsonl carries `{op, …}` frames written by Rust's file_bridge.
        # Only `subscribe` is acted on (drives quote streaming); other ops
        # are master-side and don't apply to a read-only TV bridge.
        line = line.strip()
        if not line:
            return
        try:
            cmd = json.loads(line)
        except Exception:
            return
        if cmd.get("op") != "subscribe":
            return
        symbols = cmd.get("symbols") or []
        new_subs = {str(s).upper() for s in symbols if s}
        if new_subs == self.quote_subs:
            return
        self.quote_subs = new_subs
        self._log(f"quote subs: {sorted(new_subs) if new_subs else 'none'}")
        # Replay last-known quotes for each newly-subscribed symbol so
        # the engine doesn't have to wait for the next tick to see a
        # value (TV ticks are sparse on slow markets).
        for sym in new_subs:
            q = self.quote_cache.get(sym)
            if q and q.get("bid", 0) > 0 and q.get("ask", 0) > 0:
                self._emit({
                    "ev": "quote",
                    "symbol": sym,
                    "bid": q["bid"],
                    "ask": q["ask"],
                    "pip_size": q.get("pip_size", 0.0),
                    "ts": now_ms(),
                })

    def emit_heartbeat(self) -> None:
        if not self.welcomed:
            self._emit_welcome()
        self._emit({
            "ev": "heartbeat",
            "balance": self.last_balance,
            "equity": self.last_equity,
        })

    # ─── /instruments + /state pull (called from the addon `running` hook) ───

    def fetch_instruments(self) -> None:
        if not (self.broker_url and self.account_id and self.auth_token):
            return
        try:
            import requests  # local import — only needed when this runs
        except Exception:
            self._log("requests not installed; skipping /instruments sync")
            return
        url = f"https://{self.broker_url}/accounts/{self.account_id}/instruments?locale=en"
        try:
            r = requests.get(url, headers={
                "accept": "application/json",
                "authorization": self.auth_token,
                "origin": "https://www.tradingview.com",
                "referer": "https://www.tradingview.com/",
            }, timeout=8, proxies={"http": None, "https": None})
            if r.status_code != 200:
                self._log(f"/instruments status {r.status_code}")
                return
            for inst in (r.json().get("d") or []):
                name = (inst.get("name") or "").upper()
                pip = _safe_float(inst.get("pipSize"))
                if name and pip > 0:
                    self.pip_sizes[name] = pip
            self._log(f"/instruments synced {len(self.pip_sizes)} symbols")
        except Exception as e:
            self._log(f"/instruments error: {e}")

    def fetch_state(self) -> None:
        # Only used by TV-broker integrations (FTMO, OANDA, …) —
        # PaperTrading runs entirely off the WebSocket bridge now.
        if not (self.broker_url and self.account_id and self.auth_token):
            return
        if self.broker_url.endswith("papertrading.tradingview.com"):
            return
        try:
            import requests
        except Exception:
            return
        try:
            r = requests.get(
                f"https://{self.broker_url}/accounts/{self.account_id}/state",
                headers={
                    "accept": "application/json",
                    "authorization": self.auth_token,
                    "origin": "https://www.tradingview.com",
                    "referer": "https://www.tradingview.com/",
                },
                timeout=8,
                proxies={"http": None, "https": None},
            )
            if r.status_code != 200:
                return
            d = r.json().get("d") or {}
            self.last_balance = _safe_float(d.get("balance"))
            self.last_equity = _safe_float(d.get("equity") or d.get("balance"))
            cur = d.get("currency")
            if cur:
                self.last_currency = str(cur)
        except Exception:
            pass

    # ─── Internals ──────────────────────────────────────────────────────

    def _is_relevant(self, flow: "http.HTTPFlow") -> bool:  # type: ignore[name-defined]
        if not (self.broker_url and self.account_id):
            return False
        url = flow.request.pretty_url
        # TV-broker integrations ("<broker>.tradingview.com/accounts/<id>/…").
        if self.base_path() in url:
            return True
        # PaperTrading ("papertrading.tradingview.com/trading/<verb>/<id>"):
        # the account id sits at the *end* of the verb path instead of
        # inside an `/accounts/` segment, so `base_path()`'s substring
        # check would never match. Recognise this layout explicitly.
        if (self.broker_url.endswith("papertrading.tradingview.com")
                and "/trading/" in url
                and (url.rstrip("/").split("?", 1)[0].endswith(f"/{self.account_id}")
                     or f"/{self.account_id}?" in url)):
            return True
        return False

    def _emit(self, frame: dict[str, Any]) -> None:
        line = json.dumps(frame, separators=(",", ":")) + "\n"
        path = self.out_dir / "events.jsonl"
        with self.write_lock:
            with path.open("a", encoding="utf-8") as f:
                f.write(line)

    def _log(self, msg: str) -> None:
        # Direct stderr (PYTHONUNBUFFERED line-flushed) bypasses
        # mitmproxy's `termlog_verbosity=warn` filter set by tv_proxy.rs.
        sys.stderr.write(f"[cascada] {msg}\n")


def _safe_float(v: Any) -> float:
    try:
        return float(v) if v not in (None, "") else 0.0
    except (ValueError, TypeError):
        return 0.0


# ────────────────────────────── mitmproxy hooks ─────────────────────────────

bridge = TVBridge()


def load(loader) -> None:  # mitmproxy: register options
    loader.add_option(
        name="tv_broker_url",
        typespec=str,
        default="",
        help="TradingView broker host (default: auto-discovered from first request)",
    )
    loader.add_option(
        name="tv_account_id",
        typespec=str,
        default="",
        help="TradingView account id (default: auto-discovered from first request)",
    )
    loader.add_option(
        name="cascada_root",
        typespec=str,
        default="",
        help="Override Cascada's data root (default: auto-resolve)",
    )


def configure(updates) -> None:  # mitmproxy: options changed
    if {"tv_broker_url", "tv_account_id", "cascada_root"} & set(updates):
        # Stash the cascada_root override so auto-discovery later picks it up.
        bridge.cascada_root_override = ctx.options.cascada_root
        broker = ctx.options.tv_broker_url
        acc = ctx.options.tv_account_id
        if broker and acc:
            bridge.configure(broker, acc, ctx.options.cascada_root)
        else:
            bridge._log("waiting for first TradingView request to auto-discover broker + account…")


def running() -> None:  # mitmproxy: addon started
    # Heartbeat + TV-broker /state poll. Idles until configure() seeds
    # broker_url + account_id; PaperTrading short-circuits inside fetch_state.
    def _state_loop():
        last_inst = 0.0
        while True:
            try:
                if bridge.broker_url and bridge.account_id:
                    bridge.fetch_state()
                    bridge.emit_heartbeat()
                    if time.time() - last_inst > 300:
                        bridge.fetch_instruments()
                        last_inst = time.time()
            except Exception:
                pass
            time.sleep(5)
    threading.Thread(target=_state_loop, name="cascada-state-poll", daemon=True).start()

    # Tail cmd.jsonl for `subscribe` commands so the slave can ask us
    # which symbols it wants Quote streams for. The file path moves
    # when configure() fires (different account → different out_dir),
    # so we reset offset whenever we observe that swap.
    def _cmd_loop():
        path: Path | None = None
        offset = 0
        while True:
            try:
                if bridge.broker_url and bridge.account_id:
                    cur = bridge.out_dir / "cmd.jsonl"
                    if cur != path:
                        path, offset = cur, 0
                    if path.exists():
                        size = path.stat().st_size
                        if size < offset:
                            offset = 0
                        if size > offset:
                            with path.open("r", encoding="utf-8") as f:
                                f.seek(offset)
                                for line in f:
                                    bridge._handle_cmd(line)
                            offset = size
            except Exception:
                pass
            time.sleep(0.5)
    threading.Thread(target=_cmd_loop, name="cascada-cmd-tail", daemon=True).start()

    # Flush the deferred-pending buffer: emit each entry that's been
    # held >200ms (TV usually fires the fill within milliseconds when
    # the pending was already past its trigger; survivors are real
    # pendings worth showing the slave).
    def _pending_flush_loop():
        while True:
            try:
                cutoff = time.time() - 0.200
                for oid in list(bridge._pending_buffer):
                    buf = bridge._pending_buffer.get(oid)
                    if not buf or buf["ts"] > cutoff:
                        continue
                    bridge._emit(buf["data"])
                    bridge._log(buf["log"])
                    bridge.pending_paper[oid] = buf["meta"]
                    bridge._pending_buffer.pop(oid, None)
            except Exception:
                pass
            time.sleep(0.05)
    threading.Thread(target=_pending_flush_loop,
                     name="cascada-pending-flush", daemon=True).start()


def request(flow):
    bridge.request(flow)


def response(flow):
    bridge.response(flow)


def websocket_message(flow):
    bridge.on_websocket_message(flow)
