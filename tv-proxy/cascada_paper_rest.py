"""REST-driven handlers for the TV PaperTrading bridge.

Mixed into the main TVBridge class via inheritance. mitmproxy's
`request()` and `response()` hooks dispatch to the per-verb handlers
here; auto-discovery scans every TV API hit to set broker_url +
account_id without manual config; `fetch_*` are pulled by the
heartbeat thread for TV-broker integrations (PaperTrading runs off
the WS bridge entirely, so most of these short-circuit on it).
"""

import json
from typing import Any

from cascada_helpers import (
    PositionMeta,
    _DISCOVER_BROKER_RE,
    _DISCOVER_PAPER_RE,
    _safe_float,
    now_ms,
    to_side,
)


class PaperRestMixin:
    # ─── HTTP entry points (mitmproxy hooks) ────────────────────────

    def request(self, flow) -> None:  # type: ignore[no-untyped-def]
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

    def response(self, flow) -> None:  # type: ignore[no-untyped-def]
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
            elif verb == "account" and isinstance(data, dict):
                # Initial balance/equity snapshot — TV's frontend polls
                # /trading/account every few seconds. The WS pushstream
                # only sends `balance_update` after trade activity, so
                # without this hook the heartbeat ships balance=0 to
                # the Cascada UI for users who just sit on a chart.
                bal = _safe_float(data.get("balance"))
                if bal:
                    self.last_balance = bal
                    self.last_equity = bal
                cur = data.get("currency")
                if cur:
                    self.last_currency = str(cur)
            # `cancel` is handled via the WS pending_cancel path.
            return

        if method == "POST" and "/orders?" in url:
            self._on_order(dict(flow.request.urlencoded_form or {}), data)
        elif method == "POST" and "/executions?" in url:
            self._on_execution(data)
        elif method == "PUT" and "/positions/" in url:
            pid = url.rsplit("/", 1)[-1].split("?")[0]
            self._on_modify(pid, dict(flow.request.urlencoded_form or {}), data)

    # ─── Auto-discovery ─────────────────────────────────────────────

    def _try_auto_discover(self, flow) -> None:  # type: ignore[no-untyped-def]
        url = flow.request.pretty_url
        # PaperTrading only. Real TV-broker integrations (FTMO, OANDA,
        # …) are intentionally unsupported here — their session/auth/
        # quote layer differs enough that mirroring trades reliably
        # would need broker-specific work; users on those brokers
        # should connect via cTrader instead.
        m = _DISCOVER_PAPER_RE.match(url)
        if m:
            host, account_id = m.group(1), m.group(2)
            self._log(f"auto-discovered TV PaperTrading account={account_id}")
            self.configure(host, account_id, self.cascada_root_override)
            return

        m = _DISCOVER_BROKER_RE.match(url)
        if m:
            host, account_id = m.group(1), m.group(2)
            ref = (flow.request.headers.get("referer", "")
                   + "|" + flow.request.headers.get("origin", ""))
            if "tradingview.com" not in host and "tradingview.com" not in ref:
                return
            if host not in self._skipped_hosts:
                self._skipped_hosts.add(host)
                self._log(f"TV-broker integration detected ({host}, account {account_id}) "
                          f"— not supported, use cTrader for non-paper accounts")

    # ─── TV-broker event handlers ───────────────────────────────────

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

    # ─── PaperTrading REST handlers ─────────────────────────────────
    #
    # PaperTrading endpoints (POST):
    #   /trading/place           { symbol, type, qty, side, price, sl, tp, … }
    #   /trading/cancel          { id }
    #   /trading/close_position  { symbol, qty? }
    #   /trading/modify_position { symbol, stopLoss, takeProfit }
    # Bodies arrive as JSON despite the misadvertised
    # `x-www-form-urlencoded` Content-Type. Symbols arrive
    # exchange-prefixed (`OANDA:AUDCAD`); we strip the prefix.

    def _parse_paper_body(self, raw):
        if not raw:
            return {}
        try:
            return json.loads(raw.decode("utf-8"))
        except Exception:
            return {}

    def _on_paper_place(self, req: dict[str, Any], resp: dict[str, Any]) -> None:
        symbol = self._strip_exchange(req.get("symbol", ""))
        if not symbol:
            return
        order_id = str((resp or {}).get("id") or "")
        if not order_id:
            return
        # Limit orders are picked up via the WS pending lifecycle.
        if str(req.get("type") or "").lower() == "limit":
            return
        side = to_side(req.get("side", ""))
        if self._opposing_existing(symbol, side):
            self._log(f"reverse-side place skipped ({symbol} {side}) "
                      f"ticket={order_id} — closing existing position(s)")
            return
        symboltype = str((resp or {}).get("symboltype") or req.get("symboltype") or "forex")
        meta = PositionMeta(
            symbol=symbol,
            side=side,
            volume=self._qty_to_lots(_safe_float(req.get("qty")), symboltype),
            price=_safe_float(req.get("price")),
            sl=_safe_float(req.get("stopLoss") or req.get("sl") or req.get("stop_loss")),
            tp=_safe_float(req.get("takeProfit") or req.get("tp") or req.get("take_profit")),
            pip_size=self.pip_sizes.get(symbol, 0.0),
            comment=str(req.get("comment") or req.get("note") or ""),
            order_id=order_id,
        )
        self.positions[order_id] = meta
        self.paper_position_by_symbol.setdefault(symbol, []).append(order_id)
        self._emit_open(order_id, meta)
        self._log(f"open {symbol} {meta.side} {meta.volume:g} lot "
                  f"sl={meta.sl} tp={meta.tp} ticket={order_id}")

    def _on_paper_close(self, req: dict[str, Any], _resp: dict[str, Any]) -> None:
        # Netting model: request identifies target by `symbol`, not ticket.
        # close_qty is in raw forex units (TV doesn't echo symboltype on
        # close — assume forex which is the only type where qty != lots).
        symbol = self._strip_exchange(req.get("symbol", ""))
        tickets = self.paper_position_by_symbol.get(symbol, [])
        if not tickets:
            return
        close_qty = self._qty_to_lots(_safe_float(req.get("qty")), "forex")
        total = sum(self.positions[t].volume for t in tickets if t in self.positions)
        if close_qty <= 0 or close_qty >= total:
            for t in tickets:
                self.positions.pop(t, None)
                self._emit_close(t, 0.0)
            self.paper_position_by_symbol.pop(symbol, None)
            self._log(f"close {symbol} tickets={','.join(tickets)}")
            return
        # Partial close: pop the youngest ticket; emit close + re-open
        # the remainder under a fresh ticket — wire has no native
        # partial-close event.
        ticket = tickets[-1]
        meta = self.positions.pop(ticket, None)
        tickets.pop()
        if meta is None:
            if not tickets:
                self.paper_position_by_symbol.pop(symbol, None)
            return
        self._emit_close(ticket, 0.0)
        remainder = max(meta.volume - close_qty, 0.0)
        if remainder <= 0:
            if not tickets:
                self.paper_position_by_symbol.pop(symbol, None)
            self._log(f"close {symbol} ticket={ticket}")
            return
        new_ticket = f"{ticket}-r"
        new_meta = PositionMeta(
            symbol=meta.symbol, side=meta.side, volume=remainder,
            price=meta.price, sl=meta.sl, tp=meta.tp,
            pip_size=meta.pip_size, comment=meta.comment,
            order_id=new_ticket,
        )
        self.positions[new_ticket] = new_meta
        tickets.append(new_ticket)
        self._emit_open(new_ticket, new_meta)
        self._log(f"partial close {symbol} ticket={ticket} "
                  f"-> remainder {remainder:g} lot ticket={new_ticket}")

    def _on_paper_modify(self, req: dict[str, Any], _resp: dict[str, Any]) -> None:
        symbol = self._strip_exchange(req.get("symbol", ""))
        tickets = self.paper_position_by_symbol.get(symbol, [])
        if not tickets:
            return
        sl_raw = req.get("sl") or req.get("stopLoss") or req.get("stop_loss")
        tp_raw = req.get("tp") or req.get("takeProfit") or req.get("take_profit")
        # Apply to every slave ticket — TV's netted sl/tp targets the
        # whole position, so each mirrored ticket should track it.
        for ticket in tickets:
            meta = self.positions.get(ticket)
            if not meta:
                continue
            if sl_raw is not None:
                meta.sl = _safe_float(sl_raw)
            if tp_raw is not None:
                meta.tp = _safe_float(tp_raw)
            self._emit({"ev": "modify", "ticket": ticket,
                        "sl": meta.sl, "tp": meta.tp, "ts": now_ms()})
            self._log(f"modify {symbol} ticket={ticket} sl={meta.sl} tp={meta.tp}")

    # ─── /instruments + /state pulls (called from the running hook) ─

    def fetch_instruments(self) -> None:
        if not (self.broker_url and self.account_id and self.auth_token):
            return
        try:
            import requests
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
        # PaperTrading runs entirely off the WebSocket bridge.
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
