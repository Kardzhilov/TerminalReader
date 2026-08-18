#!/bin/sh
# Build the demo library of public-domain EPUBs used by docs/demo.tape.
set -eu

LIB=/tmp/tr-demo/library
mkdir -p "$LIB" /tmp/tr-demo/config/terminalreader /tmp/tr-demo/state

download() {
    [ -f "$LIB/$2" ] || curl -fsSL -o "$LIB/$2" "https://www.gutenberg.org/ebooks/$1.epub3.images"
}

download 1342 pride-and-prejudice.epub
download 2701 moby-dick.epub
download 11 alice-in-wonderland.epub
download 84 frankenstein.epub
download 35 the-time-machine.epub

cat > /tmp/tr-demo/config/terminalreader/config.toml <<'EOF'
schema_version = 1

[library]
book_dirs = ["/tmp/tr-demo/library"]
EOF

echo "Demo library ready at $LIB"
