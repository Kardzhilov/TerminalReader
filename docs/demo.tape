# VHS tape for README screenshots. Regenerate with:
#   brew install vhs && cargo build --release -p terminalreader
#   sh docs/make-demo-library.sh   (downloads public-domain EPUBs)
#   vhs docs/demo.tape
# Or with Docker (works on Windows too):
#   docker run --rm -v "$PWD:/work" -w /work --entrypoint bash \
#     ghcr.io/charmbracelet/vhs -c 'sh docs/make-demo-library.sh && vhs docs/demo.tape'
#   (build a Linux binary into target/release first, e.g. with a rust container)

Output docs/screenshots/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1180
Set Height 760
Set Padding 10
Set WindowBar Colorful
Set Theme "Dracula"
Set TypingSpeed 50ms

Hide
Type "export XDG_CONFIG_HOME=/tmp/tr-demo/config XDG_STATE_HOME=/tmp/tr-demo/state"
Enter
Type "clear; ./target/release/terminalreader"
Enter
Sleep 2s
Show

# Home -> first library
Sleep 1s
Enter
Sleep 3s
Screenshot docs/screenshots/library.png
Sleep 500ms

# Open Pride and Prejudice (4th row) and page into real text
Down
Sleep 200ms
Down
Sleep 200ms
Down
Sleep 500ms
Enter
Sleep 2s
Type "]"
Sleep 300ms
Type "]"
Sleep 300ms
Type "]"
Sleep 300ms
Type "]"
Sleep 500ms
Space
Sleep 1s
Screenshot docs/screenshots/reader.png
Sleep 500ms

# Table of contents popup
Type "t"
Sleep 1.5s
Screenshot docs/screenshots/contents.png
Sleep 500ms
Escape
Sleep 500ms

# Back home: continue reading + recents populated
Escape
Sleep 1.5s
Screenshot docs/screenshots/home.png
Sleep 500ms

# Settings
Type "s"
Sleep 1.5s
Screenshot docs/screenshots/settings.png
Sleep 1s

# Cycle theme presets: gruvbox -> gruvbox-light -> dracula
Type "h"
Sleep 1s
Screenshot docs/screenshots/themes.png
Sleep 500ms
Type "h"
Sleep 1s
Type "h"
Sleep 1.5s

Escape
Sleep 500ms
Type "q"
Sleep 500ms
