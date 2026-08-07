# wandr desktop runtime installer — Windows (PowerShell 5.1+).
#
#   irm https://raw.githubusercontent.com/harryzz/wandr-host/main/install.ps1 | iex
#
# Downloads the latest wandr-host release binary into %LOCALAPPDATA%\wandr\bin.
# Env overrides:
#   $env:WANDR_HOME     install root       (default: %LOCALAPPDATA%\wandr)
#   $env:WANDR_VERSION  release tag to pin (default: latest)
#
# Desktop only. Android is a rooted, ART-stripped dev target — not installed this
# way. On Linux/macOS use install.sh.

$ErrorActionPreference = 'Stop'
# PowerShell 5.1 defaults to old TLS; GitHub requires 1.2+.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo      = 'harryzz/wandr-host'
$WandrHome = if ($env:WANDR_HOME) { $env:WANDR_HOME } else { Join-Path $env:LOCALAPPDATA 'wandr' }
$BinDir    = Join-Path $WandrHome 'bin'

function Info($m) { Write-Host "> $m"  -ForegroundColor Green }
function Warn($m) { Write-Host "! $m"  -ForegroundColor Yellow }
function Die($m)  { Write-Host "x $m"  -ForegroundColor Red; exit 1 }

# --- platform -> release asset ----------------------------------------------
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ($arch) {
  'AMD64' { $asset = 'wandr-host-windows-x86_64.exe' }
  'ARM64' { $asset = 'wandr-host-windows-x86_64.exe'; Warn 'ARM64 Windows: using the x64 build via emulation.' }
  default { Die "no published build for Windows/$arch (CI ships x86_64 only) - build from source." }
}

# --- download base (latest, or WANDR_VERSION-pinned) ------------------------
$ver  = if ($env:WANDR_VERSION) { $env:WANDR_VERSION } else { 'latest' }
$base = if ($ver -eq 'latest') { "https://github.com/$Repo/releases/latest/download" }
        else                   { "https://github.com/$Repo/releases/download/$ver" }

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$tmp = Join-Path $env:TEMP ("wandr-host-" + [guid]::NewGuid().ToString() + ".exe")

Info "downloading $asset ..."
try { Invoke-WebRequest -Uri "$base/$asset" -OutFile $tmp -UseBasicParsing }
catch { Die "download failed - no release asset '$asset' (published a release yet?)." }

# --- verify against the release SHA256SUMS (best effort) --------------------
try {
  $sums = (Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing).Content
  $line = $sums -split "`n" | Where-Object { $_ -match ("\s" + [regex]::Escape($asset) + "\s*$") } | Select-Object -First 1
  if ($line) {
    $want = ($line -split '\s+')[0].ToLower()
    $got  = (Get-FileHash -Algorithm SHA256 -Path $tmp).Hash.ToLower()
    if ($got -ne $want) { Die "checksum mismatch for $asset." }
    Info "checksum ok."
  }
} catch { }

$dest = Join-Path $BinDir 'wandr-host.exe'
Move-Item -Force -Path $tmp -Destination $dest
Info "installed -> $dest"

# --- the `wandr` app-manager CLI (sits beside the host) ---------------------
$raw = "https://raw.githubusercontent.com/$Repo/main"
foreach ($f in 'wandr.ps1', 'wandr.cmd') {
  try {
    Invoke-WebRequest -Uri "$raw/$f" -OutFile (Join-Path $BinDir $f) -UseBasicParsing
    Info "installed -> $BinDir\$f"
  } catch { Warn "could not fetch $f (host installed fine; grab it later from $raw/$f)." }
}

# --- runtime dep: GStreamer (the video DECODE backend) ----------------------
if (-not (Get-Command gst-inspect-1.0.exe -ErrorAction SilentlyContinue) -and -not $env:GSTREAMER_1_0_ROOT_MSVC_X86_64) {
  Warn "GStreamer not found - video playback needs it:"
  Write-Host "    winget install GStreamer.GStreamer   (or the MSVC 64-bit runtime + devel from gstreamer.freedesktop.org)"
}

# --- PATH (user scope) ------------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $BinDir) {
  [Environment]::SetEnvironmentVariable('Path', "$BinDir;$userPath", 'User')
  Warn "added $BinDir to your PATH - open a NEW terminal to pick it up."
}

Write-Host ""
Write-Host "done. next:  wandr list   ->   wandr install <id>   ->   wandr run <id>"
