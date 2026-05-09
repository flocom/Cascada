"""Shared utilities for the Cascada TradingView bridge.

Imported by both `cascada_addon.py` (the mitmproxy entry point) and
the mixin modules. Sits at the bottom of the import graph — never
imports from sibling addon modules — so the circular-import trap
mitmproxy creates by loading the entry as `__main__` doesn't bite.
"""

import platform
import re
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


# ───────────────────────── Cascada root resolution ─────────────────

def cascada_root_default() -> Path:
    """Mirror `connectors/file_bridge::cascada_root` from the Rust core.

    macOS prefers `~/cAlgo` when it exists (cTrader's native location);
    everything else lands under `~/Documents/cAlgo/Cascada`. The TV
    folder sits at `<root>/TradingView/<login>/`.
    """
    home = Path.home()
    if platform.system() == "Darwin":
        native = home / "cAlgo"
        if native.is_dir():
            return native / "Cascada"
    for c in (home / "Documents" / "cAlgo", home / "cAlgo"):
        if c.is_dir():
            return c / "Cascada"
    return home / "Documents" / "cAlgo" / "Cascada"


# ──────────────────────────── Wire helpers ──────────────────────────

def now_ms() -> int:
    return int(time.time() * 1000)


def to_side(s: str) -> str:
    """TV uses lowercase 'buy'/'sell'; Cascada's wire serializes Side as
    capitalised 'Buy'/'Sell'."""
    return "Sell" if (s or "").strip().lower() == "sell" else "Buy"


def _safe_float(v: Any) -> float:
    try:
        return float(v) if v not in (None, "") else 0.0
    except (ValueError, TypeError):
        return 0.0


# ────────────────────────── Auto-discovery patterns ─────────────────
#
# Two URL families auto-discovery latches onto, both rooted at a
# `*.tradingview.com` host:
#   1. TV-broker integration (FTMO, OANDA, …):
#         `/accounts/<id>/(state|positions|orders|…)`
#   2. TV PaperTrading (TV's own simulated account):
#         `papertrading.tradingview.com/trading/<verb>/<id>`
# Only PaperTrading is supported for the actual mirror; broker
# matches log a "use cTrader instead" hint and don't configure.

_DISCOVER_BROKER_RE = re.compile(
    r'^https?://([^/]+)/accounts/([^/?]+)/'
    r'(?:state|positions|orders|executions|instruments|history|preferences|configuration)\b'
)
_DISCOVER_PAPER_RE = re.compile(
    r'^https?://(papertrading\.tradingview\.com)/trading/'
    # TV PaperTrading account IDs are numeric (e.g. `379216`). Earlier
    # this fell back to `[A-Za-z0-9_-]+` which spuriously captured
    # endpoint segments like `/trading/get_currency_rate/rate` as a
    # fake "rate" account, polluting the Cascada Accounts panel.
    r'(?:[a-z_]+)/(\d+)\b'
)


# ────────────────────────────── State dataclasses ──────────────────

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
    # TV-broker emits orderId first; /executions later supplies
    # positionId. We index positions by order_id so SL/TP child
    # deletions resolve.
    order_id: str = ""
    # Original TV data-feed marker (`OANDA:`, `*PEPPERSTONE`, …)
    # extracted from the raw symbol before stripping. Travels in
    # the wire `feed` field so engine-side quote_offsets can match
    # the source feed and not just the bare ticker.
    feed: str = ""


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
    feed: str = ""


def split_feed(symbol: str) -> tuple[str, str]:
    """Return (bare_symbol, feed_marker) extracted from a raw TV
    symbol. Feed is the literal pattern token that gets glued back
    onto the bare ticker — `EXCHANGE:` for prefix-style, `*BROKER`
    for suffix-style, empty string when neither applies. The bare
    symbol is the slave-friendly ticker (no exchange tag), the feed
    is metadata for the master-side rule engine."""
    s = (symbol or "").upper()
    if not s:
        return "", ""
    colon = s.find(":")
    if colon > 0:
        return s[colon + 1:], s[:colon] + ":"
    star = s.find("*")
    if star > 0:
        return s[:star], "*" + s[star + 1:]
    return s, ""
