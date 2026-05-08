#!/usr/bin/env bash
# One-shot installer for the Cascada TradingView bridge sidecar (macOS / Linux).
#
# Does, in order:
#   1. Verifies Python 3.10+ is available.
#   2. Creates an isolated venv under tv-proxy/.venv (idempotent).
#   3. Installs mitmproxy + requests into the venv.
#   4. Boots mitmdump briefly so it generates ~/.mitmproxy/mitmproxy-ca-cert.*
#      on first run.
#   5. Best-effort installs the CA cert into the OS trust store (will prompt
#      for sudo on macOS / Linux). Skip-and-continue if the user declines.
#   6. Launches mitmdump with the addon. The addon auto-discovers the TV
#      broker + account id from the first request the proxied browser makes.
#
# Re-run safely: every step is idempotent. Press Ctrl-C to stop the sidecar.

set -euo pipefail

cd "$(dirname "$0")"
HERE="$(pwd)"
VENV="$HERE/.venv"
CERT_DIR="$HOME/.mitmproxy"
CERT_PEM="$CERT_DIR/mitmproxy-ca-cert.pem"

log() { printf '\033[36m→\033[0m %s\n' "$*"; }
warn() { printf '\033[33m!\033[0m %s\n' "$*" >&2; }
fail() { printf '\033[31m✕\033[0m %s\n' "$*" >&2; exit 1; }

# 1. Python
if command -v python3.13 >/dev/null 2>&1; then PY=python3.13
elif command -v python3.12 >/dev/null 2>&1; then PY=python3.12
elif command -v python3.11 >/dev/null 2>&1; then PY=python3.11
elif command -v python3.10 >/dev/null 2>&1; then PY=python3.10
elif command -v python3 >/dev/null 2>&1; then PY=python3
else
  fail "Python 3.10+ not found. Install via brew (macOS) or your package manager (apt/dnf/pacman)."
fi
PYVER=$("$PY" -c 'import sys;print("%d.%d" % sys.version_info[:2])')
log "Python $PYVER ($("$PY" -c 'import sys;print(sys.executable)'))"

# 2. venv
if [ ! -d "$VENV" ]; then
  log "Creating venv at $VENV…"
  "$PY" -m venv "$VENV"
fi
# shellcheck disable=SC1091
source "$VENV/bin/activate"

# 3. deps
log "Installing/upgrading mitmproxy + requests…"
pip install --quiet --upgrade pip
pip install --quiet --upgrade mitmproxy requests

# 4. CA cert generation (mitmdump creates it on first run)
if [ ! -f "$CERT_PEM" ]; then
  log "Bootstrapping mitmproxy CA cert…"
  # Run mitmdump on a throwaway port for ~3s so it generates ~/.mitmproxy/*
  # without binding 8080 (which would conflict with the real run below).
  mitmdump --listen-port 18080 --quiet >/dev/null 2>&1 &
  PID=$!
  for _ in 1 2 3 4 5 6; do
    [ -f "$CERT_PEM" ] && break
    sleep 0.5
  done
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
fi
[ -f "$CERT_PEM" ] || fail "mitmproxy CA cert never appeared at $CERT_PEM"

# 5. Cert install (best-effort; user can decline by pressing Ctrl-C through sudo)
OS="$(uname -s)"
if [ "$OS" = "Darwin" ]; then
  if security find-certificate -c mitmproxy /Library/Keychains/System.keychain >/dev/null 2>&1; then
    log "CA cert already trusted in System keychain"
  else
    log "Installing CA cert into macOS System keychain (sudo prompt)…"
    if sudo security add-trusted-cert -d -r trustRoot \
         -k /Library/Keychains/System.keychain "$CERT_PEM"; then
      log "CA cert installed"
    else
      warn "Cert install skipped — trust manually in Keychain Access > System > add $CERT_PEM > Always Trust"
    fi
  fi
elif [ "$OS" = "Linux" ]; then
  SYS_DST="/usr/local/share/ca-certificates/mitmproxy.crt"
  if [ -f "$SYS_DST" ]; then
    log "CA cert already in system trust store"
  elif command -v update-ca-certificates >/dev/null 2>&1; then
    log "Installing CA cert into /usr/local/share/ca-certificates (sudo prompt)…"
    if sudo cp "$CERT_PEM" "$SYS_DST" && sudo update-ca-certificates >/dev/null; then
      log "CA cert installed system-wide"
    else
      warn "System trust install skipped"
    fi
  fi
  # Chrome on Linux uses NSS — add to user nssdb if present.
  NSSDB="$HOME/.pki/nssdb"
  if command -v certutil >/dev/null 2>&1 && [ -d "$NSSDB" ]; then
    if ! certutil -d "sql:$NSSDB" -L -n mitmproxy >/dev/null 2>&1; then
      log "Adding cert to Chrome NSS db ($NSSDB)…"
      certutil -d "sql:$NSSDB" -A -t "C,," -n mitmproxy -i "$CERT_PEM" \
        || warn "NSS db add failed"
    fi
  fi
  warn "Firefox uses its own NSS db per profile — if you trade on Firefox, import $CERT_PEM via Settings → Certificates → Authorities"
fi

# 6. Run
cat <<EOF

\033[32m✓ Sidecar ready.\033[0m

Now:
  1. Set your browser's HTTP/HTTPS proxy to 127.0.0.1:8080
  2. Open TradingView and place any tiny trade
  3. Cascada auto-discovers your TV account and shows it as a Master

Press Ctrl-C to stop the sidecar.

EOF
exec mitmdump -s "$HERE/cascada_addon.py"
