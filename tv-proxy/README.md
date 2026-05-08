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

## In-app setup (recommended)

The Cascada desktop app manages the sidecar lifecycle for you. The addon is
embedded in the Cascada binary; nothing in this folder needs to be checked
out by end users.

1. Open Cascada → **Accounts** → **+ Connect platform** → pick
   **TradingView**.
2. Click **Install proxy**. Cascada detects Python on your `PATH`, builds an
   isolated venv at `<cascada_root>/tv-proxy/.venv/`, installs mitmproxy +
   requests, extracts the addon, and bootstraps the CA cert.
3. Click **Start proxy**. Cascada spawns `mitmdump` as a supervised child;
   stdout / stderr land in the **Logs** panel under the `tv-proxy` source.
4. Set your browser's HTTP/HTTPS proxy to `127.0.0.1:8080`, trust
   `~/.mitmproxy/mitmproxy-ca-cert.pem` once, and trade — Cascada
   auto-attaches the TV account as a Master.

The sidecar stops when you quit Cascada and re-starts on next launch as long
as a TradingView account exists in your saved state.

Python 3.10+ is required on `PATH` (we don't bundle CPython). On Windows,
install from [python.org](https://www.python.org/downloads/) with **"Add
Python to PATH"** ticked; macOS / Linux usually have it already.

## Headless / scripted setup (fallback)

If you can't run the GUI (CI, remote box, scripted deploy), the legacy
bootstrap scripts still work and do the same thing the in-app flow does:

```bash
# macOS / Linux
./tv-proxy/setup.sh

# Windows (PowerShell)
.\tv-proxy\setup.ps1
```

Re-running the script is safe — every step is idempotent. Press `Ctrl-C` to
stop the sidecar.

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
> — the proxy works for both because auto-discovery snaps the host from
> whatever request comes in first. The referer/origin guard ensures only
> tradingview.com-originated requests latch on.

## Manual install (fallback)

The bootstrap script does the steps below for you. Use this path only if
the script fails (locked-down system, no sudo, custom Python env, etc).

### 1. Install mitmproxy + requests

```bash
pip install mitmproxy requests
```

The addon needs `requests` for the `/state` and `/instruments` polls
(mitmproxy ships with its own `httpx`, but those calls go *outside*
mitmproxy's intercept loop, so we use a regular sync client).

### 2. Trust mitmproxy's CA cert

Run mitmproxy once so it generates the cert:

```bash
mitmdump
```

Then install `~/.mitmproxy/mitmproxy-ca-cert.pem` (`.cer` on Windows) in
your browser's trust store. Without this, TradingView's HTTPS calls won't
decrypt and the addon will see nothing.

- **macOS**: `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain ~/.mitmproxy/mitmproxy-ca-cert.pem`
- **Linux**: `sudo cp ~/.mitmproxy/mitmproxy-ca-cert.pem /usr/local/share/ca-certificates/mitmproxy.crt && sudo update-ca-certificates` — Chrome on Linux additionally reads `~/.pki/nssdb`, see `setup.sh` for that variant.
- **Windows**: `certutil -addstore -f -user ROOT %USERPROFILE%\.mitmproxy\mitmproxy-ca-cert.cer` (no UAC needed).
- **Firefox**: each profile has its own NSS db. Import the cert via *Options → Privacy & Security → Certificates → Authorities*.

### 3. Run the bridge

```bash
# Auto-discovery (recommended) — broker URL + account id snap from first TV request:
mitmdump -s tv-proxy/cascada_addon.py

# Manual override (skip auto-discovery, pin a specific account):
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
    --set cascada_root=/Volumes/share/cascada-data
```

### 4. Point your browser at the proxy

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
  switch brokers (TV PaperTrading → an OANDA-integrated TV), restart
  mitmdump so auto-discovery picks the new account.
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
