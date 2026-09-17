//! Inline images over the reader's `[ IMAGE ]` boxes via terminal graphics
//! protocols (kitty, iTerm2, sixel, or half-block fallback).
//!
//! Compiled only with the `inline-images` cargo feature; without a capable
//! terminal the text placeholder stays as-is.

use ratatui::{Frame, layout::Rect};
use ratatui_image::{Image, Resize, picker::Picker, protocol::Protocol};
use std::{io::Cursor, path::PathBuf};
use unicode_width::UnicodeWidthStr;

use super::ReaderScreen;

/// Renders the first visible image of the page with the terminal's graphics
/// protocol, on top of its placeholder box.
pub struct InlineImages {
    picker: Option<Picker>,
    current: Option<Cached>,
}

struct Cached {
    book_path: PathBuf,
    chapter: usize,
    block: usize,
    href: String,
    document_digest: Option<String>,
    area: Rect,
    protocol: Option<Protocol>,
}

impl std::fmt::Debug for InlineImages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InlineImages")
            .field("picker", &self.picker.is_some())
            .finish_non_exhaustive()
    }
}

impl InlineImages {
    const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

    /// Query the terminal for its graphics protocol and font size.
    pub fn detect() -> Self {
        Self {
            picker: Picker::from_query_stdio().ok(),
            current: None,
        }
    }

    /// Draw the first visible image over its placeholder box, reusing the
    /// encoded protocol while the page does not change.
    pub fn render(&mut self, frame: &mut Frame, reader: &mut ReaderScreen, inner: Rect) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let Some((block, area)) = image_box_area(reader, inner) else {
            self.current = None;
            return;
        };
        let href = image_href(reader, block);
        let reusable = self.current.as_ref().is_some_and(|cached| {
            cached.book_path == reader.path
                && cached.chapter == reader.chapter_index
                && cached.block == block
                && cached.href == href
                && cached.document_digest == reader.document_digest
                && cached.area == area
        });
        if !reusable {
            self.current = Some(Cached {
                book_path: reader.path.clone(),
                chapter: reader.chapter_index,
                block,
                href,
                document_digest: reader.document_digest.clone(),
                area,
                protocol: load_protocol(picker, reader, block, area),
            });
        }
        if let Some(cached) = &self.current
            && let Some(protocol) = &cached.protocol
        {
            frame.render_widget(Image::new(protocol), cached.area);
        }
    }
}

/// The on-screen rectangle of the first fully or partially visible image box
/// that has a source, and the index of its block.
fn image_box_area(reader: &ReaderScreen, inner: Rect) -> Option<(usize, Rect)> {
    let mut start = None;
    let mut rows = 0_u16;
    let mut block = 0;
    let mut width = 0_u16;
    for (row, line) in reader.visible_lines().iter().enumerate() {
        let Ok(row) = u16::try_from(row) else { break };
        if row >= inner.height {
            break;
        }
        let is_image = line.atomic
            && matches!(
                reader.blocks.get(line.block),
                Some(tr_epub::Block::Image { href: Some(_), .. })
            );
        match (start, is_image) {
            (None, true) => {
                start = Some(row);
                block = line.block;
                rows = 1;
                width = u16::try_from(UnicodeWidthStr::width(line.text.as_str())).unwrap_or(0);
            }
            (Some(_), true) if line.block == block => rows += 1,
            (Some(_), _) => break,
            (None, false) => {}
        }
    }
    let start = start?;
    if rows < 2 || width < 4 {
        return None;
    }
    Some((
        block,
        Rect::new(inner.x, inner.y + start, width.min(inner.width), rows),
    ))
}

/// Decode the image behind `block` and encode it for the terminal, fitted
/// into `area`.
fn load_protocol(
    picker: &mut Picker,
    reader: &mut ReaderScreen,
    block: usize,
    area: Rect,
) -> Option<Protocol> {
    let href = image_href(reader, block);
    if href.is_empty() {
        return None;
    }
    let (_, bytes) = reader
        .book
        .resource_bytes(reader.chapter_index, &href)
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(InlineImages::MAX_IMAGE_PIXELS.saturating_mul(4));
    let reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels > InlineImages::MAX_IMAGE_PIXELS {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    picker
        .new_protocol(decoded, area.as_size(), Resize::Fit(None))
        .ok()
}

fn image_href(reader: &ReaderScreen, block: usize) -> String {
    match reader.blocks.get(block) {
        Some(tr_epub::Block::Image {
            href: Some(href), ..
        }) => href.clone(),
        _ => String::new(),
    }
}
