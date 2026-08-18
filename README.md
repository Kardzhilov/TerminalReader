# TerminalReader

A fullscreen EPUB reader for the terminal, with KOReader-compatible reading
progress sync (kosync protocol — works with kosync.eu, sync.koreader.rocks,
and self-hosted servers).

## Features

- Fullscreen TUI reader (ratatui) with mouse support, TOC popup, and
  reflow-stable positions across terminal resizes
- Library directories with a metadata scan cache (keyed by path/size/mtime)
- First-run wizard for choosing an initial library directory
- Reading preferences: max content width, full justification, ASCII-only mode
- `NO_COLOR` respected; help overlay on `?` / `F1`
- KOReader progress sync:
  - binary (partial MD5) and filename document matching
  - xpointer ↔ reader position mapping with percentage fallback
  - automatic pull on open; prompt/silent/disabled forward/backward strategies
  - push on close/quit, page-count interval pushes, manual push (`s`) / pull (`p`)
  - 25-second debounce, visible sync status, persistent offline queue
  - credentials in the OS keyring (Secret Service / Keychain / Credential Manager)
- Optional file logging with secret redaction (`--log-file`, `[logging]` config)

## Install

From source (Rust 1.85+):

```sh
cargo install --path crates/tr-tui
```

Release artifacts for Linux (x86_64/aarch64), macOS (x86_64/aarch64), and
Windows are built by the `Release` workflow on version tags (`v*`).

On Linux, the keyring integration uses the D-Bus Secret Service; install
`libdbus-1-dev`/`pkg-config` when building from source and make sure a secret
service (GNOME Keyring, KWallet, keepassxc) is running.

## Usage

```sh
terminalreader                 # home screen (first run opens the setup wizard)
terminalreader read book.epub  # open a book directly
terminalreader library DIR     # list EPUBs under DIR
terminalreader addLibrary DIR  # persist a library directory
terminalreader hash book.epub  # print KOReader-compatible document hashes
terminalreader doctor [book]   # verify config, state, keyring, and sync server
terminalreader --log-file tr.log --log-level debug   # log with redaction
```

Configuration lives at the platform config dir (Linux:
`~/.config/terminalreader/config.toml`), state (positions, recents, scan
cache, sync queue) at the platform state dir.

### Sync setup

Settings screen (`s` from home): set the server with `u` (e.g.
`https://kosync.eu`), then `l` to log in or `r` to register. Only the derived
userkey (MD5, per the kosync protocol) is stored — in the OS keyring, never
on disk. The matching method (`c`) must match your other devices; KOReader's
default is binary.

## Verification

Automated: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
warnings`, `cargo test --workspace` (CI runs these on Linux/macOS/Windows,
plus cargo-deny and cargo-audit).

Live server round-trip tests (opt-in, never run in CI):

```sh
TR_SYNC_TEST_SERVER=https://kosync.eu \
TR_SYNC_TEST_USER=user TR_SYNC_TEST_PASSWORD=pass \
cargo test -p tr-kosync --test live -- --ignored
```

Fuzzing (requires nightly + `cargo install cargo-fuzz`):

```sh
cargo +nightly fuzz run epub_open      # EPUB container/OPF/XHTML parsing
cargo +nightly fuzz run xpointer_parse # KOReader progress strings
```

### Manual verification checklist (Linux/macOS)

- [ ] First run shows the wizard; Enter adds the directory, Esc skips, and the
      wizard does not reappear on the next start
- [ ] Library scan is fast on the second open (scan cache hit); touching an
      EPUB (`touch book.epub`) rescans only that file
- [ ] Reading preferences: max width caps the column, `j` justifies, `m`
      switches rules/image boxes to ASCII; all persist across restarts
- [ ] Resize the terminal while reading: the top-of-screen word stays anchored,
      below 60×16 the resize notice appears and recovery is clean
- [ ] `NO_COLOR=1 terminalreader` renders without color
- [ ] `?`/`F1` shows the help overlay on every screen; any key closes it
- [ ] `--log-file` writes logs and never contains the userkey/password
- [ ] Doctor reports config, libraries, keyring, queue, and server auth

### Sync interoperability checklist (real server + KOReader device)

- [ ] Register + login against kosync.eu (or self-hosted) from Settings
- [ ] `terminalreader hash book.epub` matches the document id shown by the
      server dashboard / KOReader for the identical file
- [ ] Read on a KOReader device, push, then open the same book here: the pull
      prompt appears with the device name and jumps to the right paragraph
- [ ] Read here, quit, then open on the KOReader device: KOReader offers the
      forward position
- [ ] Disable networking, turn pages, quit: queue persists; next start with
      network, the queue drains (doctor shows 0 pending)

## License

AGPL-3.0-or-later