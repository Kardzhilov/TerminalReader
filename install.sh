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
checksums_url="https://github.com/$REPO/releases/download/$tag/SHA256SUMS.txt"

# --- Download and install --------------------------------------------------
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

say "Downloading $asset ($tag)…"
fetch_to "$url" "$tmpdir/$asset" || fail "download failed: $url"
fetch_to "$checksums_url" "$tmpdir/SHA256SUMS.txt" || fail "could not download release checksums"

expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1 }' "$tmpdir/SHA256SUMS.txt")
[ "$(printf '%s\n' "$expected" | sed '/^$/d' | wc -l | tr -d ' ')" = "1" ] ||
    fail "no unique checksum found for $asset"
printf '%s' "$expected" | grep -Eq '^[0-9A-Fa-f]{64}$' || fail "invalid checksum for $asset"
actual=$(sha256sum "$tmpdir/$asset" 2>/dev/null | awk '{print $1}' || true)
if [ -z "$actual" ] && command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmpdir/$asset" | awk '{print $1}')
fi
[ -n "$actual" ] || fail "sha256sum or shasum is required to verify downloads"
[ "$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')" = "$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')" ] ||
    fail "checksum mismatch for $asset"
tar xzf "$tmpdir/$asset" -C "$tmpdir"

src="$tmpdir/terminalreader-${tag}-${target}/$BINARY"
[ -f "$src" ] || fail "archive did not contain the $BINARY binary"

mkdir -p "$INSTALL_DIR"
destination="$INSTALL_DIR/$BINARY"
staged="$tmpdir/$BINARY.new"
backup="$tmpdir/$BINARY.previous"
cp "$src" "$staged"
chmod 755 "$staged"
had_previous=0
if [ -e "$destination" ]; then
    mv "$destination" "$backup" || fail "could not stage the existing installation"
    had_previous=1
fi
if ! mv "$staged" "$destination"; then
    if [ "$had_previous" -eq 1 ]; then
        mv "$backup" "$destination" || fail "replacement failed and rollback also failed"
    fi
    fail "could not replace $destination"
fi
if [ "$had_previous" -eq 1 ]; then rm -f "$backup"; fi
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
