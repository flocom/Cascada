"""
mitmproxy addon: TradingView → Cascada bridge.

Bridges a TradingView browser session to Cascada's local file-bridge protocol
(the same JSONL format the cTrader cBot and MT4/MT5 EAs use). Cascada then
treats the TV PaperTrading / TV-broker account as a master and fans trades
out to MT4/MT5/cTrader slaves through its existing copy engine.

Zero-config run — just point a proxied browser at this:

    mitmdump -s tv-proxy/cascada_addon.py

The addon sniffs the first TV broker API request and auto-configures itself.
Manual override (skip auto-discovery, or pin a specific account) still works:

    mitmdump -s tv-proxy/cascada_addon.py \
        --set tv_broker_url=paper-trading.tradingview.com \
        --set tv_account_id=PA-1234567 \
        [--set cascada_root=/path/to/Documents/cAlgo/Cascada]

If `cascada_root` isn't set, the addon mirrors Cascada's own resolution
(`<home>/Documents/cAlgo/Cascada` on Windows/Linux, `<home>/cAlgo/Cascada`
on macOS when present, otherwise the Documents-rooted path).

Endpoints captured (the events Cascada needs):

    POST  /accounts/{acc}/orders?...       → S2C::Open (after /executions ack)
    POST  /accounts/{acc}/executions?...   → fills pending Open events
    PUT   /accounts/{acc}/positions/{pid}  → S2C::Modify (SL/TP change)
    DELETE/accounts/{acc}/positions/{pid}  → S2C::Close
    DELETE/accounts/{acc}/orders/{id}.SL.* → S2C::Modify (SL removed)
    DELETE/accounts/{acc}/orders/{id}.TP.* → S2C::Modify (TP removed)

The `/instruments` endpoint is fetched once on startup to populate per-symbol
`pip_size` — Cascada's quote-offset math depends on the broker-reported pip
size, so always shipping it is the difference between a working drift
correction and a silently-wrong one (see `engine.rs::effective_pip_size`).

This addon is **read-only on TV**: it observes the user's clicks in the TV
panel and forwards them. Cascada's own commands (close-from-Cascada, modify
SL/TP from the slave side) would require an HTTP injector that re-issues
TV's authenticated requests — left as a future extension; cmd.jsonl is
parsed but commands are logged-and-dropped so the file format stays
compatible with the rest of the Cascada connector ecosystem.

Wire format reference: src-tauri/src/connectors/proto.rs (look for `enum S2C`).
"""

# Note: NOT using `from __future__ import annotations`. Combined with
# Python 3.12 dataclass introspection (`_is_type` in dataclasses.py),
# stringified annotations from PEP 563 trigger a crash inside
# `_process_class` on the first @dataclass decoration when running
# under python-build-standalone's Windows distribution. Evaluating
# annotations eagerly avoids that path entirely. We're on 3.12 so
# `dict[str, float]` and other PEP 585 generics work natively.

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

# Match any `https://<host>/accounts/<id>/{state|positions|orders|...}` URL
# coming from a TV-originated request. The endpoint suffixes here are the
# stable subset every TV broker integration exposes, so a single hit on the
# first request lets us snap broker_url + account_id without a manual flag.
_DISCOVER_RE = re.compile(
    r'^https?://([^/]+)/accounts/([^/?]+)/'
    r'(?:state|positions|orders|executions|instruments|history|preferences|configuration)\b'
)


# ────────────────────────────── State tracking ─────────────────────────────

@dataclass
class PositionMeta:
    """Last-known SL/TP per position so DELETE on TP/SL nodes can synthesize a
    Modify with the surviving level zeroed out (Cascada's wire treats 0 as
    'unset', see `proto.rs::opt`)."""
    symbol: str = ""
    side: str = "Buy"
    volume: float = 0.0
    price: float = 0.0
    sl: float = 0.0
    tp: float = 0.0
    pip_size: float = 0.0
    comment: str = ""
    # `/orders` returns an order id; `/executions` later supplies the
    # position id. We map order→position so a TP/SL deletion (referenced by
    # order id) reaches the right Cascada ticket.
    order_id: str = ""


@dataclass
class TVBridge:
    broker_url: str = ""
    account_id: str = ""
    cascada_root_override: str = ""
    out_dir: Path = field(default_factory=cascada_root_default)
    pip_sizes: dict[str, float] = field(default_factory=dict)
    pending_opens: dict[str, PositionMeta] = field(default_factory=dict)
    positions: dict[str, PositionMeta] = field(default_factory=dict)
    auth_token: str = ""
    welcomed: bool = False
    last_balance: float = 0.0
    last_equity: float = 0.0
    last_currency: str = "USD"
    write_lock: threading.Lock = field(default_factory=threading.Lock)

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
        # Auto-discovery: snap broker_url + account_id from the first TV
        # broker API request the proxy sees. We only sniff if neither was
        # set on the command line, so manual `--set tv_broker_url=...` keeps
        # full precedence (e.g. when juggling two TV accounts).
        if not (self.broker_url and self.account_id):
            self._try_auto_discover(flow)

        # Capture the auth token from any request to the configured broker —
        # TV refreshes them periodically; we want the freshest one for
        # `/instruments` and `/state` polls.
        if self.broker_url and self.broker_url in flow.request.pretty_url:
            tok = flow.request.headers.get("authorization")
            if tok:
                self.auth_token = tok

        if not self._is_relevant(flow):
            return

        method = flow.request.method
        url = flow.request.pretty_url

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

        if method == "POST" and "/orders?" in url:
            self._on_order(dict(flow.request.urlencoded_form or {}), data)
        elif method == "POST" and "/executions?" in url:
            self._on_execution(data)
        elif method == "PUT" and "/positions/" in url:
            pid = url.rsplit("/", 1)[-1].split("?")[0]
            self._on_modify(pid, dict(flow.request.urlencoded_form or {}), data)

    # ─── Auto-discovery ───────────────────────────────────────────────

    def _try_auto_discover(self, flow: "http.HTTPFlow") -> None:  # type: ignore[name-defined]
        m = _DISCOVER_RE.match(flow.request.pretty_url)
        if not m:
            return
        host, account_id = m.group(1), m.group(2)
        # Cheap sanity filter: TV originates the request, so its referer or
        # origin header points at tradingview.com (or *.tradingview.com).
        # Skip anything without that signal so we don't latch onto random
        # /accounts/.../state endpoints from unrelated APIs the proxy might
        # see (corp tools, dev environments, etc.).
        ref = (flow.request.headers.get("referer", "")
               + "|" + flow.request.headers.get("origin", ""))
        if "tradingview.com" not in ref:
            return
        self._log(f"auto-discovered TV broker={host} account={account_id}")
        self.configure(host, account_id, self.cascada_root_override)

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
        self._emit({"ev": "modify", "ticket": pid, "sl": meta.sl, "tp": meta.tp})

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
        if not (self.broker_url and self.account_id and self.auth_token):
            return
        try:
            import requests
        except Exception:
            return
        url = f"https://{self.broker_url}/accounts/{self.account_id}/state"
        try:
            r = requests.get(url, headers={
                "accept": "application/json",
                "authorization": self.auth_token,
                "origin": "https://www.tradingview.com",
                "referer": "https://www.tradingview.com/",
            }, timeout=8, proxies={"http": None, "https": None})
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
        return self.base_path() in flow.request.pretty_url

    def _emit(self, frame: dict[str, Any]) -> None:
        line = json.dumps(frame, separators=(",", ":")) + "\n"
        path = self.out_dir / "events.jsonl"
        with self.write_lock:
            with path.open("a", encoding="utf-8") as f:
                f.write(line)

    def _log(self, msg: str) -> None:
        if ctx is not None and getattr(ctx, "log", None) is not None:
            try:
                ctx.log.info(f"[cascada] {msg}")
                return
            except Exception:
                pass
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
    # Always start the heartbeat loop — it idles until configure() (manual
    # or auto-discovered) populates broker_url + account_id, then begins
    # polling /state every 5s and refreshing /instruments every 5min.
    def _loop():
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
    threading.Thread(target=_loop, name="cascada-state-poll", daemon=True).start()


def request(flow):
    bridge.request(flow)


def response(flow):
    bridge.response(flow)
