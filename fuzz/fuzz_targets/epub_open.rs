//! Fuzz the EPUB container/OPF/XHTML parsing pipeline with arbitrary bytes.
//!
//! Run with: `cargo +nightly fuzz run epub_open` (requires cargo-fuzz).

#![no_main]

use std::io::Write;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(mut file) = tempfile::Builder::new().suffix(".epub").tempfile() else {
        return;
    };
    if file.write_all(data).is_err() {
        return;
    }
    if let Ok(mut book) = tr_epub::EpubBook::open(file.path()) {
        // Parsing chapters exercises the XHTML block extractor too.
        let chapters = book.spine.len().min(4);
        for index in 0..chapters {
            let _ = book.chapter_blocks(index);
        }
    }
});
