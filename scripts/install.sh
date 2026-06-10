#!/bin/sh
#
# install.sh — install a prebuilt heliosdb-nano binary from GitHub Releases.
# Downloads the archive for this OS/arch, verifies it against the release's
# SHA256SUMS, and installs the binary. Idempotent: re-run safely.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/HeliosDatabase/HeliosDB-Nano/main/scripts/install.sh | sh
#   sh scripts/install.sh             # latest release
#   sh scripts/install.sh v3.39.0    # a specific release tag
#
# Environment overrides:
#   HELIOSDB_VERSION       release tag to install (e.g. v3.39.0; default: latest)
#   HELIOSDB_INSTALL_DIR   install directory
#                          (default: /usr/local/bin when root, else ~/.local/bin)
#
# Supported targets (must match .github/workflows/release.yml build matrix):
#   x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, aarch64-apple-darwin
# Windows users: download the .zip release asset manually, or use
#   cargo binstall heliosdb-nano

set -eu

REPO="HeliosDatabase/HeliosDB-Nano"
VERSION="${1:-${HELIOSDB_VERSION:-}}"

fatal() {
  echo "fatal: $*" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || fatal "curl is required"
command -v tar  >/dev/null 2>&1 || fatal "tar is required"

# ── detect platform ─────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64|amd64)  TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
      *) fatal "unsupported Linux architecture: $ARCH (use 'cargo install heliosdb-nano')" ;;
    esac
    # Prebuilt Linux binaries link glibc (built on ubuntu-24.04) — musl
    # systems (e.g. Alpine) must build from source for now.
    if [ ! -e /lib/ld-linux-x86-64.so.2 ] && [ ! -e /lib/ld-linux-aarch64.so.1 ] \
       && ldd --version 2>&1 | grep -qi musl; then
      fatal "musl libc detected — no musl binaries yet; use 'cargo install heliosdb-nano'"
    fi
    ;;
  Darwin)
    case "$ARCH" in
      arm64) TARGET="aarch64-apple-darwin" ;;
      *) fatal "unsupported macOS architecture: $ARCH (Apple Silicon only; use 'cargo install heliosdb-nano')" ;;
    esac
    ;;
  *)
    fatal "unsupported OS: $OS (Windows: download the .zip release asset or 'cargo binstall heliosdb-nano')"
    ;;
esac

# ── resolve version ─────────────────────────────────────────────────────
if [ -z "$VERSION" ]; then
  echo "==> resolving latest release"
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
             | sed -n 's/.*"tag_name"[: ]*"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$VERSION" ] || fatal "could not determine latest release from the GitHub API"
fi
case "$VERSION" in
  v*) : ;;
  *)  VERSION="v$VERSION" ;;
esac

ASSET="heliosdb-nano-${VERSION}-${TARGET}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/$VERSION"

# ── download ────────────────────────────────────────────────────────────
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

echo "==> downloading $ASSET ($VERSION)"
curl -fSL --progress-bar -o "$TMP/$ASSET" "$BASE_URL/$ASSET" \
  || fatal "download failed — does release $VERSION ship a $TARGET binary?"
curl -fsSL -o "$TMP/SHA256SUMS" "$BASE_URL/SHA256SUMS" \
  || fatal "could not download SHA256SUMS for $VERSION"

# ── verify checksum ─────────────────────────────────────────────────────
echo "==> verifying sha256"
if command -v sha256sum >/dev/null 2>&1; then
  SHA_CHECK="sha256sum -c"
elif command -v shasum >/dev/null 2>&1; then
  SHA_CHECK="shasum -a 256 -c"
else
  fatal "neither sha256sum nor shasum found — cannot verify download"
fi
( cd "$TMP" && grep "$ASSET\$" SHA256SUMS | $SHA_CHECK - ) \
  || fatal "sha256 verification FAILED for $ASSET — aborting install"

# ── extract + install ───────────────────────────────────────────────────
tar -xzf "$TMP/$ASSET" -C "$TMP" heliosdb-nano

if [ -n "${HELIOSDB_INSTALL_DIR:-}" ]; then
  INSTALL_DIR="$HELIOSDB_INSTALL_DIR"
elif [ "$(id -u)" = "0" ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="$HOME/.local/bin"
fi
mkdir -p "$INSTALL_DIR"

if command -v install >/dev/null 2>&1; then
  install -m 755 "$TMP/heliosdb-nano" "$INSTALL_DIR/heliosdb-nano"
else
  cp "$TMP/heliosdb-nano" "$INSTALL_DIR/heliosdb-nano"
  chmod 755 "$INSTALL_DIR/heliosdb-nano"
fi

# ── final pointer ───────────────────────────────────────────────────────
PATH_HINT=""
case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *) PATH_HINT="
NOTE: $INSTALL_DIR is not on your PATH. Add it with:
  export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

cat <<EOF

==============================================================
heliosdb-nano $VERSION installed to $INSTALL_DIR/heliosdb-nano
$PATH_HINT
Verify the install:
  $INSTALL_DIR/heliosdb-nano --version

First query in under a minute:
  $INSTALL_DIR/heliosdb-nano repl --data-dir ./helios-data

Or run it as a server (PostgreSQL wire protocol on :5432):
  $INSTALL_DIR/heliosdb-nano start --data-dir ./helios-data

Docs: https://github.com/$REPO
==============================================================
EOF
