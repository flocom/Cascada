# TradingView master — mitmproxy sidecar

Cascada talks to cTrader / MT4 / MT5 directly through a cBot or EA. TradingView
is a browser product though, with no plugin surface to attach to, so the TV
master flow needs an external proxy that sniffs the user's session and
forwards the order events into Cascada.

This folder ships that proxy: a single-file [mitmproxy](https://mitmproxy.org/)
addon (`cascada_addon.py`) that writes to Cascada's existing file bridge.
Cascada's `tv_bridge::spawn_discovery` polls
`<cascada_root>/TradingView/<login>/` on a 3 s tick and attaches the account
as a Master automatically when the proxy starts writing. No extra Cascada
configuration required.

## What it captures

| Cascada wire event | TV API call |
| --- | --- |
| `welcome` / `heartbeat` | `GET  /state` (polled every 5 s) |
| `open` | `POST /orders` + `POST /executions` |
| `modify` (SL/TP change) | `PUT  /positions/{id}` |
| `modify` (SL/TP cleared) | `DELETE /orders/{id}.SL.*` or `.TP.*` |
| `close` | `DELETE /positions/{id}` |
| pip_size on every event | `GET  /instruments` (cached, refreshed every 5 min) |

> ⚠️ TV PaperTrading's API is **not** the same as a real broker integration
> — the proxy works for both because the URL prefix is configurable, but
> you must capture the host TV is actually talking to from your browser
> (DevTools → Network → filter by `accounts/`).

## Setup

### 1. Install

```bash
pip install mitmproxy requests
```

The addon also needs `requests` for the `/state` and `/instruments` polls
(mitmproxy ships with its own `httpx`, but those calls go *outside*
mitmproxy's intercept loop, so we use a regular sync client).

### 2. Trust mitmproxy's CA cert

Run mitmproxy once so it generates the cert:

```bash
mitmdump
```

Then install `~/.mitmproxy/mitmproxy-ca-cert.pem` in your browser's trust
store. Without this, TradingView's HTTPS calls won't decrypt and the addon
will see nothing.

### 3. Find your TV broker host + account id

1. Open TradingView in the proxied browser, log in, switch to the broker
   you want to mirror (PaperTrading or any integrated broker).
2. Open DevTools → Network tab.
3. Place a tiny test trade. Look for a request to
   `https://<broker-host>/accounts/<account-id>/orders?...` — the host and
   id you need are the bold parts.

### 4. Run the bridge

From the Cascada repo root:

```bash
mitmdump -s tv-proxy/cascada_addon.py \
    --set tv_broker_url=paper-trading.tradingview.com \
    --set tv_account_id=PA-1234567
```

Cascada (running locally) auto-discovers the new TV folder under
`<cascada_root>/TradingView/<account_id>/` after a few seconds and shows
the account in the **Accounts** tab as a Master.

To override Cascada's data root (e.g. running on a different machine and
syncing the folder over a network share):

```bash
mitmdump -s tv-proxy/cascada_addon.py \
    --set tv_broker_url=paper-trading.tradingview.com \
    --set tv_account_id=PA-1234567 \
    --set cascada_root=/Volumes/share/cascada-data
```

### 5. Point your browser at the proxy

mitmproxy's default port is `8080`. Set your browser's HTTP/HTTPS proxy to
`127.0.0.1:8080` while you trade on TV.

## Pip-difference correction

Cascada's `quote_offsets` rule field shifts SL / TP / Fixed-SL anchor by a
per-symbol pip offset, so the slave broker ends up with the same absolute
price the user intended even if TV's price feed differs from the slave's
broker by a couple of pips.

The addon **always** ships `pip_size` from TV's `/instruments` endpoint on
every Open / Pending event. This is critical — Cascada falls back to a
name-based heuristic when `pip_size` is 0 (`engine.rs::effective_pip_size`),
which guesses 0.0001 for unknown tickers and silently turns a legitimate
−0.22-pip offset into an invisible price shift on indices / crypto / exotic
suffixed symbols.

To capture the offset visually:

1. In Cascada, open the **Compare** tab.
2. Pick the TV master and your MT5 / cTrader slave on each side.
3. Add the symbol pair (`EURUSD` master / `EURUSD.r` slave for instance).
4. Click **Capture** — Cascada samples the median pip-diff over 15 s and
   offers to push the value straight into the matching copy rule (or
   create one on the fly if none exists yet).

## Limitations (v1)

- **TV → Cascada only.** Cascada does not currently re-issue orders into
  TV (would need an authenticated HTTP injector that survives TV's token
  refresh). The `cmd.jsonl` file is created and read so the protocol stays
  symmetric, but writes from Cascada are logged-and-dropped.
- **One TV session per account.** The `account_id` keys the folder; if you
  switch brokers (TV PaperTrading → an OANDA-integrated TV), use a new
  `tv_account_id` so they don't collide.
- **Partial closes** are reported as `close` (full) — Cascada doesn't
  model partial closes today; the slave receives a full close.
- **TV ToS.** mitmproxy reverse-engineers TV's broker API. Fine for
  personal use, would need TV's blessing for any commercial deployment.

## Wire format

The addon writes one JSON object per line to
`<cascada_root>/TradingView/<account_id>/events.jsonl`. The full schema
lives in [`src-tauri/src/connectors/proto.rs`](../src-tauri/src/connectors/proto.rs)
(look for `enum S2C`).

A typical session looks like:

```jsonl
{"ev":"welcome","balance":100000.0,"equity":100000.0,"currency":"USD","account":"PA-1234567"}
{"ev":"heartbeat","balance":100000.0,"equity":100000.0}
{"ev":"open","ticket":"5421","symbol":"EURUSD","side":"Buy","volume":0.1,"price":1.08531,"sl":1.08400,"tp":1.08700,"ts":1730284800000,"pip_size":0.0001,"comment":""}
{"ev":"modify","ticket":"5421","sl":1.08450,"tp":1.08700,"volume":0.1}
{"ev":"close","ticket":"5421","profit":0.0,"ts":1730284900000}
```
