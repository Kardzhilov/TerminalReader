#!/bin/sh
# TerminalReader installer: downloads the latest release binary for this
# machine and installs it to ~/.local/bin (override with TR_INSTALL_DIR).
#
#   curl -fsSL https://raw.githubusercontent.com/Kardzhilov/TerminalReader/main/install.sh | sh

set -eu

REPO="Kardzhilov/TerminalReader"
INSTALL_DIR="${TR_INSTALL_DIR:-$HOME/.local/bin}"
BINARY="terminalreader"

say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- Detect platform -------------------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$os" in
    Linux) os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) fail "unsupported operating system: $os (Windows: download the .zip from https://github.com/$REPO/releases/latest)" ;;
esac
case "$arch" in
    x86_64 | amd64) arch_part="x86_64" ;;
    aarch64 | arm64) arch_part="aarch64" ;;
    *) fail "unsupported architecture: $arch" ;;
esac
target="${arch_part}-${os_part}"

# --- Find the download tool ------------------------------------------------
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1"; }
    fetch_to() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO- "$1"; }
    fetch_to() { wget -qO "$2" "$1"; }
else
    fail "curl or wget is required"
fi

# --- Resolve the latest release tag ----------------------------------------
say "Looking up the latest release of $REPO…"
tag=$(fetch "https://api.github.com/repos/$REPO/releases/latest" |
    sed -n 's/^[[:space:]]*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)
[ -n "$tag" ] || fail "could not determine the latest release tag"

asset="terminalreader-${tag}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"

# --- Download and install --------------------------------------------------
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

say "Downloading $asset ($tag)…"
fetch_to "$url" "$tmpdir/$asset" || fail "download failed: $url"
tar xzf "$tmpdir/$asset" -C "$tmpdir"

src="$tmpdir/terminalreader-${tag}-${target}/$BINARY"
[ -f "$src" ] || fail "archive did not contain the $BINARY binary"

mkdir -p "$INSTALL_DIR"
install -m 755 "$src" "$INSTALL_DIR/$BINARY"
say "Installed $BINARY $tag to $INSTALL_DIR/$BINARY"

# --- PATH hint --------------------------------------------------------------
case ":$PATH:" in
    *":$INSTALL_DIR:"*) say "Run '$BINARY' to get started." ;;
    *)
        say ""
        say "note: $INSTALL_DIR is not on your PATH. Add it with:"
        say "  export PATH=\"\$PATH:$INSTALL_DIR\""
        say "then run '$BINARY'."
        ;;
esac
