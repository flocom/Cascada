"""mitmproxy addon: TradingView → Cascada bridge.

Bridges a TradingView session (PaperTrading) into Cascada's local
file-bridge JSONL protocol so the TV account can act as a master
fanning trades out to MT4/MT5/cTrader slaves.

Run: `mitmdump -s tv-proxy/cascada_addon.py`. Auto-discovers broker_url
+ account_id from the first TV API hit (REST) or pushstream WS frame
(cross-browser). Manual pin via `--set tv_broker_url=… --set
tv_account_id=…` still wins. Wire schema: `src-tauri/src/connectors/proto.rs::S2C`.

Module layout (kept in the same directory as this file):
  - `cascada_addon.py`       — entry: helpers, dataclasses, TVBridge state,
                               base methods, mitmproxy hooks (this file)
  - `cascada_paper_rest.py`  — PaperRestMixin (REST / auto-discovery / fetch_*)
  - `cascada_paper_ws.py`    — PaperWsMixin (pushstream + qsd + cmd.jsonl)

Why no `from __future__ import annotations`: PEP 563 stringified
annotations crash `dataclasses._process_class` on Python 3.12 under
python-build-standalone's Windows build. PEP 585 generics work natively.
"""

import json
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

try:
    from mitmproxy import ctx
except ImportError:  # pragma: no cover
    ctx = None  # type: ignore

# All shared helpers and dataclasses live in `cascada_helpers.py` so
# the mixin modules can import them without round-tripping through
# this entry. mitmproxy loads us as `__main__`; a `from cascada_addon
# import …` from inside a mixin would re-execute this file as a fresh
# module and crash on circular imports. The helpers module sits at
# the bottom of the import graph and is safe for everyone to import.
#
# Eject sibling modules from sys.modules before importing so a hot
# reload of cascada_addon (mitmproxy watches this file) reloads the
# mixins too — without this, sibling-only edits stay invisible until
# mitmdump is restarted.
for _mod in ("cascada_helpers", "cascada_paper_rest", "cascada_paper_ws"):
    sys.modules.pop(_mod, None)
from cascada_helpers import (
    PositionMeta,
    PendingMeta,
    cascada_root_default,
    now_ms,
    to_side,
    _safe_float,
)
from cascada_paper_rest import PaperRestMixin
from cascada_paper_ws import PaperWsMixin


@dataclass
class TVBridge(PaperRestMixin, PaperWsMixin):
    broker_url: str = ""
    account_id: str = ""
    cascada_root_override: str = ""
    out_dir: Path = field(default_factory=cascada_root_default)
    pip_sizes: dict[str, float] = field(default_factory=dict)
    pending_opens: dict[str, PositionMeta] = field(default_factory=dict)
    positions: dict[str, PositionMeta] = field(default_factory=dict)
    # Multiple slave tickets per symbol because TV's PaperTrading is a
    # netting account (1 net position per symbol) but copy trading
    # benefits from a 1:1 entry→slave-position mapping: 2 successive
    # buys in TV produce 2 separate slave positions, not just an
    # update of the first one's sl/tp.
    paper_position_by_symbol: dict[str, list[str]] = field(default_factory=dict)
    # Order ids already classified as reverse-side (and skipped): TV
    # re-broadcasts the same order_update through multiple statuses
    # so we'd log the skip twice without this set.
    _skipped_reverse: set[str] = field(default_factory=set)
    # Pending limit/stop orders broadcast to the slave wire. Keyed by
    # TV order id; values track last-emitted target/sl/tp for modify
    # diff. On fill the entry migrates into `positions`.
    pending_paper: dict[str, PendingMeta] = field(default_factory=dict)
    auth_token: str = ""
    welcomed: bool = False
    last_balance: float = 0.0
    last_equity: float = 0.0
    last_currency: str = "USD"
    write_lock: threading.Lock = field(default_factory=threading.Lock)
    _skipped_hosts: set[str] = field(default_factory=set)
    # Quote streaming. The slave-side connector writes `{op:"subscribe",
    # symbols:[…]}` to cmd.jsonl whenever the set of mirrored symbols
    # changes; we tail that file from a thread, latch the set, and
    # emit `Quote` events on every prodata `qsd` frame.
    quote_subs: set[str] = field(default_factory=set)
    quote_cache: dict[str, dict[str, float]] = field(default_factory=dict)
    # Pending-emit buffer. TV happily accepts pending orders that are
    # already past their trigger and fills them within milliseconds.
    # We hold each fresh `pending` event briefly (~200ms) before
    # writing it to the wire; if a fill arrives in that window we
    # drop the buffered pending and emit a plain `open` instead.
    _pending_buffer: dict[str, dict[str, Any]] = field(default_factory=dict)

    # ─── Lifecycle ──────────────────────────────────────────────────

    def configure(self, broker_url: str, account_id: str, out_root: str = "") -> None:
        # PaperTrading account ids are numeric (e.g. 379216). Anything
        # else (`rate`, `feed`, …) is a misfired auto-discovery from a
        # non-account endpoint — silently drop it so we don't pollute
        # the file-bridge directory with phantom accounts that Cascada
        # later auto-recreates from the persisted state.
        if not (account_id and account_id.isdigit()):
            self._log(f"configure rejected non-numeric account_id={account_id!r}")
            return
        self.broker_url = broker_url.rstrip("/")
        self.account_id = account_id
        root = Path(out_root) if out_root else cascada_root_default()
        self.out_dir = root / "TradingView" / account_id
        self.out_dir.mkdir(parents=True, exist_ok=True)
        # Truncate cmd.jsonl on attach so a stale Cascada session
        # doesn't bombard us with replayed commands. Same convention
        # as Rust's `file_bridge::spawn_with_dir`.
        (self.out_dir / "cmd.jsonl").write_text("", encoding="utf-8")
        self._log(f"bridge → {self.out_dir}")

    def base_path(self) -> str:
        return f"{self.broker_url}/accounts/{self.account_id}"

    # ─── Internals shared by REST + WS handlers ─────────────────────

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

    def _ticket_for_order_id(self, order_id: str) -> str:
        """Resolve an order id back to the ticket we're tracking."""
        if not order_id:
            return ""
        if order_id in self.positions:
            return order_id
        for ticket, meta in self.positions.items():
            if meta.order_id == order_id:
                return ticket
        return ""

    def _opposing_existing(self, symbol: str, side: str) -> bool:
        # True when at least one tracked position on `symbol` has the
        # opposite side. TV PaperTrading nets buy + sell into one
        # position; if the user "closes" a long by placing a short
        # market order, /trading/place fires for the counter-order and
        # we'd erroneously open a slave position. Reverse-side places
        # must be ignored — the position_update qty drives the slave
        # close path instead.
        for t in self.paper_position_by_symbol.get(symbol, []):
            ref = self.positions.get(t)
            if ref and ref.side != side:
                return True
        return False

    def _partial_close_lifo(self, symbol: str, reduce_by: float) -> None:
        # Reduce slave-side total volume on `symbol` by `reduce_by`
        # lots, popping tickets LIFO. Tickets that fit inside the
        # remainder are closed entirely; the last partially-touched
        # ticket is closed + re-opened smaller (the wire has no
        # native partial_close event).
        tickets = self.paper_position_by_symbol.get(symbol)
        if not tickets:
            return
        remaining = reduce_by
        while remaining > 1e-6 and tickets:
            ticket = tickets[-1]
            meta = self.positions.get(ticket)
            if meta is None:
                tickets.pop()
                continue
            if meta.volume <= remaining + 1e-6:
                self.positions.pop(ticket, None)
                self._emit_close(ticket, 0.0)
                self._log(f"close {symbol} ticket={ticket} (ws/partial)")
                tickets.pop()
                remaining -= meta.volume
            else:
                self.positions.pop(ticket, None)
                self._emit_close(ticket, 0.0)
                new_volume = meta.volume - remaining
                new_ticket = f"{ticket}-r"
                new_meta = PositionMeta(
                    symbol=meta.symbol, side=meta.side, volume=new_volume,
                    price=meta.price, sl=meta.sl, tp=meta.tp,
                    pip_size=meta.pip_size, comment=meta.comment,
                    order_id=new_ticket,
                )
                self.positions[new_ticket] = new_meta
                tickets[-1] = new_ticket
                self._emit_open(new_ticket, new_meta)
                self._log(f"partial {symbol} ticket={ticket} "
                          f"-> remainder {new_volume:g} ticket={new_ticket} (ws/partial)")
                remaining = 0
        if not tickets:
            self.paper_position_by_symbol.pop(symbol, None)

    def _is_relevant(self, flow) -> bool:  # type: ignore[no-untyped-def]
        if not (self.broker_url and self.account_id):
            return False
        url = flow.request.pretty_url
        # TV-broker integrations ("<broker>.tradingview.com/accounts/<id>/…").
        if self.base_path() in url:
            return True
        # PaperTrading ("papertrading.tradingview.com/trading/<verb>/<id>"):
        # the account id sits at the *end* of the verb path instead
        # of inside an `/accounts/` segment.
        if (self.broker_url.endswith("papertrading.tradingview.com")
                and "/trading/" in url
                and (url.rstrip("/").split("?", 1)[0].endswith(f"/{self.account_id}")
                     or f"/{self.account_id}?" in url)):
            return True
        return False

    # ─── Wire emit + log ────────────────────────────────────────────

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


# ────────────────────────────── mitmproxy hooks ─────────────────────────────

bridge = TVBridge()


def load(loader) -> None:
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


def configure(updates) -> None:
    if {"tv_broker_url", "tv_account_id", "cascada_root"} & set(updates):
        bridge.cascada_root_override = ctx.options.cascada_root
        broker = ctx.options.tv_broker_url
        acc = ctx.options.tv_account_id
        if broker and acc:
            bridge.configure(broker, acc, ctx.options.cascada_root)
        else:
            bridge._log("waiting for first TradingView request to auto-discover broker + account…")


def running() -> None:
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
    # which symbols it wants Quote streams for.
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
    # held >200ms. TV usually fires the fill within milliseconds when
    # the pending was already past its trigger; survivors are real
    # pendings worth showing the slave.
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
