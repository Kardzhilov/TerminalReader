//! Fuzz the chapter XHTML parser with arbitrary documents.
//!
//! Run with: `cargo +nightly fuzz run chapter_parse` (requires cargo-fuzz).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        // Parsing must never panic, whatever the markup looks like.
        let _ = tr_epub::parse_chapter(text);
    }
});
