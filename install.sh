#!/bin/sh
# wandr desktop runtime installer — Linux & macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/harryzz/wandr-host/main/install.sh | sh
#
# Downloads the latest wandr-host release binary for this platform into
# ~/.wandr/bin. Env overrides:
#   WANDR_HOME     install root            (default: ~/.wandr)
#   WANDR_VERSION  release tag to pin      (default: latest)
#
# Desktop only. Android is a rooted, ART-stripped dev target — not installed
# this way. On Windows use install.ps1.
set -eu

REPO="harryzz/wandr-host"
WANDR_HOME="${WANDR_HOME:-$HOME/.wandr}"
BIN_DIR="$WANDR_HOME/bin"

info() { printf '\033[1;32m▸\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# ── platform → release asset ─────────────────────────────────────────────────
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) asset="wandr-host-linux-x86_64" ;;
      *) err "no published build for Linux/$arch (CI ships x86_64 only) — build from source." ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) asset="wandr-host-macos-aarch64" ;;
      x86_64) err "no published build for Intel macOS (Apple Silicon only) — build from source." ;;
      *) err "unsupported macOS arch '$arch'." ;;
    esac ;;
  *) err "unsupported OS '$os' — this installer is Linux/macOS; on Windows use install.ps1." ;;
esac

# ── download base (latest, or WANDR_VERSION-pinned) ──────────────────────────
if [ "${WANDR_VERSION:-latest}" = "latest" ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$WANDR_VERSION"
fi

dl() { # dl <url> <out>
  if   command -v curl >/dev/null 2>&1; then curl -fSL --progress-bar "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -q --show-progress -O "$2" "$1"
  else err "need curl or wget."; fi
}

mkdir -p "$BIN_DIR"
tmp="$(mktemp)"
sums="$tmp.sums"
trap 'rm -f "$tmp" "$sums"' EXIT

info "downloading $asset …"
dl "$base/$asset" "$tmp" || err "download failed — no release asset '$asset' (published a release yet?)."

# ── verify against the release SHA256SUMS (best effort) ──────────────────────
if dl "$base/SHA256SUMS" "$sums" 2>/dev/null; then
  want="$(awk -v a="$asset" '$2==a {print $1}' "$sums" 2>/dev/null || true)"
  if [ -n "$want" ]; then
    if   command -v sha256sum >/dev/null 2>&1; then got="$(sha256sum "$tmp" | awk '{print $1}')"
    elif command -v shasum    >/dev/null 2>&1; then got="$(shasum -a 256 "$tmp" | awk '{print $1}')"
    else got=""; fi
    if [ -n "$got" ] && [ "$got" != "$want" ]; then err "checksum mismatch for $asset."; fi
    [ -n "$got" ] && info "checksum ok."
  fi
fi

chmod +x "$tmp"
mv "$tmp" "$BIN_DIR/wandr-host"
info "installed → $BIN_DIR/wandr-host"

# ── the `wandr` app-manager CLI (sits beside the host) ───────────────────────
RAW="https://raw.githubusercontent.com/$REPO/main"
if dl "$RAW/wandr" "$BIN_DIR/wandr.tmp" 2>/dev/null; then
  chmod +x "$BIN_DIR/wandr.tmp" && mv "$BIN_DIR/wandr.tmp" "$BIN_DIR/wandr"
  info "installed → $BIN_DIR/wandr"
else
  rm -f "$BIN_DIR/wandr.tmp"
  warn "could not fetch the 'wandr' CLI (host installed fine; grab it later from $RAW/wandr)."
fi

# ── runtime dep: GStreamer (the video DECODE backend) ────────────────────────
if command -v gst-inspect-1.0 >/dev/null 2>&1 || pkg-config --exists gstreamer-1.0 2>/dev/null; then
  :
else
  warn "GStreamer not found — video playback needs it:"
  case "$os" in
    Linux)  printf '    Debian/Ubuntu: sudo apt install libgstreamer1.0-0 gstreamer1.0-plugins-base \\\n                   gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav\n' ;;
    Darwin) printf '    macOS: brew install gstreamer\n' ;;
  esac
fi

# ── PATH hint ────────────────────────────────────────────────────────────────
case ":${PATH}:" in
  *":$BIN_DIR:"*) ;;
  *) warn "add wandr to your PATH:"
     printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.zshrc   # or ~/.bashrc\n' "$BIN_DIR" ;;
esac

printf '\ndone. next:  wandr list   →   wandr install <id>   →   wandr run <id>\n'
