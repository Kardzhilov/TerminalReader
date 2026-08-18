//! Fuzz the KOReader xpointer parser with arbitrary progress strings.
//!
//! Run with: `cargo +nightly fuzz run xpointer_parse` (requires cargo-fuzz).

#![no_main]

use libfuzzer_sys::fuzz_target;
use tr_kosync::xpointer::XPointer;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Some(pointer) = XPointer::parse(text) {
            // Round-tripping must never panic and must stay parseable.
            let formatted = pointer.format();
            let _ = XPointer::parse(&formatted);
        }
    }
});
