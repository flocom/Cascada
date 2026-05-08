# One-shot installer for the Cascada TradingView bridge sidecar (Windows).
#
# Does, in order:
#   1. Verifies Python 3.10+ is available.
#   2. Creates an isolated venv under tv-proxy/.venv (idempotent).
#   3. Installs mitmproxy + requests into the venv.
#   4. Boots mitmdump briefly so it generates %USERPROFILE%\.mitmproxy\*.
#   5. Installs the CA cert into the current user's Trusted Root store
#      (no UAC needed for -user scope).
#   6. Launches mitmdump with the addon. The addon auto-discovers the TV
#      broker + account id from the first request the proxied browser makes.
#
# Re-run safely: every step is idempotent. Press Ctrl-C to stop the sidecar.

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$Here = $PSScriptRoot
$Venv = Join-Path $Here '.venv'
$CertDir = Join-Path $env:USERPROFILE '.mitmproxy'
$CertPem = Join-Path $CertDir 'mitmproxy-ca-cert.pem'
$CertCer = Join-Path $CertDir 'mitmproxy-ca-cert.cer'

function Log  ([string]$m) { Write-Host "→ $m" -ForegroundColor Cyan }
function Warn ([string]$m) { Write-Host "! $m" -ForegroundColor Yellow }
function Fail ([string]$m) { Write-Host "✕ $m" -ForegroundColor Red; exit 1 }

# 1. Python
$python = $null
foreach ($cand in @('python3.13','python3.12','python3.11','python3.10','python','py')) {
  $cmd = Get-Command $cand -ErrorAction SilentlyContinue
  if ($cmd) { $python = $cmd.Path; break }
}
if (-not $python) {
  Fail 'Python 3.10+ not found. Install from https://www.python.org/downloads/ (check "Add Python to PATH").'
}
$pyver = & $python -c 'import sys;print("%d.%d" % sys.version_info[:2])'
Log "Python $pyver ($python)"

# 2. venv
if (-not (Test-Path $Venv)) {
  Log "Creating venv at $Venv…"
  & $python -m venv $Venv
}
. (Join-Path $Venv 'Scripts\Activate.ps1')

# 3. deps
Log 'Installing/upgrading mitmproxy + requests…'
pip install --quiet --upgrade pip
pip install --quiet --upgrade mitmproxy requests

# 4. CA cert generation
if (-not (Test-Path $CertPem)) {
  Log 'Bootstrapping mitmproxy CA cert…'
  $proc = Start-Process -FilePath 'mitmdump' `
                        -ArgumentList '--listen-port','18080','--quiet' `
                        -PassThru -WindowStyle Hidden
  for ($i = 0; $i -lt 12; $i++) {
    if (Test-Path $CertPem) { break }
    Start-Sleep -Milliseconds 500
  }
  if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}
if (-not (Test-Path $CertPem)) {
  Fail "mitmproxy CA cert never appeared at $CertPem"
}

# 5. Cert install — user trust store, no UAC
if (Test-Path $CertCer) {
  $existing = Get-ChildItem Cert:\CurrentUser\Root -ErrorAction SilentlyContinue |
              Where-Object { $_.Subject -like '*mitmproxy*' }
  if ($existing) {
    Log 'CA cert already trusted in CurrentUser\Root'
  } else {
    Log 'Installing CA cert into CurrentUser\Root…'
    $r = certutil -addstore -f -user ROOT $CertCer 2>&1
    if ($LASTEXITCODE -eq 0) {
      Log 'CA cert installed'
    } else {
      Warn "certutil failed: $r"
      Warn "Trust manually: double-click $CertCer → Install Certificate → Current User → Trusted Root Certification Authorities"
    }
  }
} else {
  Warn "$CertCer not found — mitmproxy may have skipped the .cer export. Try mitmdump once manually."
}
Warn 'Firefox uses its own trust store — if you trade on Firefox, import the cert via Options → Certificates → Authorities'

# 6. Run
Write-Host ''
Write-Host '✓ Sidecar ready.' -ForegroundColor Green
Write-Host ''
Write-Host 'Now:'
Write-Host '  1. Set your browser HTTP/HTTPS proxy to 127.0.0.1:8080'
Write-Host '  2. Open TradingView and place any tiny trade'
Write-Host '  3. Cascada auto-discovers your TV account and shows it as a Master'
Write-Host ''
Write-Host 'Press Ctrl-C to stop the sidecar.'
Write-Host ''
mitmdump -s (Join-Path $Here 'cascada_addon.py')
