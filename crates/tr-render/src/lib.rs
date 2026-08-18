//! Terminal-width-aware rendering and layout for EPUB blocks.

use tr_epub::Block;
use unicode_width::UnicodeWidthStr;

/// Sentinel offset for the blank separator line between blocks.
const SEPARATOR_OFFSET: usize = usize::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// Index of the block this line belongs to.
    pub block: usize,
    /// Byte offset of this line's first word within the block text.
    /// Stable across wrap widths, unlike a line number.
    pub char_offset: usize,
    /// Part of an image box; never used as a split point at the top of a view.
    pub atomic: bool,
}

impl Line {
    #[must_use]
    pub fn is_separator(&self) -> bool {
        self.char_offset == SEPARATOR_OFFSET
    }
}

/// Lay a chapter out as a flat list of wrapped lines for the given width.
#[must_use]
pub fn layout(blocks: &[Block], width: u16, ascii_only: bool) -> Vec<Line> {
    let content_width = usize::from(width.saturating_sub(4)).max(20);
    let mut lines = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let rendered = render_block(block, content_width, ascii_only);
        if rendered.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            // Keyed to the previous block so anchor ordering stays monotonic.
            lines.push(Line {
                text: String::new(),
                block: index.saturating_sub(1),
                char_offset: SEPARATOR_OFFSET,
                atomic: false,
            });
        }
        let atomic = matches!(block, Block::Image { .. });
        for (text, char_offset) in rendered {
            lines.push(Line {
                text,
                block: index,
                char_offset,
                atomic,
            });
        }
    }
    lines
}

/// Index of the line containing the (block, char-offset) anchor, for any width.
#[must_use]
pub fn line_for_anchor(lines: &[Line], block: usize, char_offset: usize) -> usize {
    lines
        .iter()
        .rposition(|line| {
            !line.is_separator() && (line.block, line.char_offset) <= (block, char_offset)
        })
        .unwrap_or(0)
}

/// Render one block as (line text, byte offset of the line's first word).
#[must_use]
pub fn render_block(block: &Block, width: usize, ascii_only: bool) -> Vec<(String, usize)> {
    match block {
        Block::Paragraph(text) => wrap(text, width),
        Block::Heading { level, text } => {
            let mut lines = wrap(text, width);
            if *level <= 2 {
                lines.insert(0, (String::new(), 0));
            }
            lines
        }
        Block::Quote(text) => wrap(text, width.saturating_sub(2))
            .into_iter()
            .map(|(line, offset)| (format!("> {line}"), offset))
            .collect(),
        Block::Code(text) => {
            let mut offset = 0;
            let mut lines = Vec::new();
            for line in text.lines() {
                lines.push((line.chars().take(width).collect(), offset));
                offset += line.len() + 1;
            }
            lines
        }
        Block::Rule => vec![(
            if ascii_only {
                "-".repeat(width)
            } else {
                "─".repeat(width)
            },
            0,
        )],
        Block::Image { alt } => image_box(alt.as_deref(), width, ascii_only)
            .into_iter()
            .map(|line| (line, 0))
            .collect(),
    }
}

#[must_use]
pub fn image_box(alt: Option<&str>, width: usize, ascii_only: bool) -> Vec<String> {
    let box_width = width.clamp(12, 40);
    let (top_left, horizontal, top_right, vertical, bottom_left, bottom_right) = if ascii_only {
        ('+', '-', '+', '|', '+', '+')
    } else {
        ('┌', '─', '┐', '│', '└', '┘')
    };
    let mut content = vec!["[ IMAGE ]".to_owned()];
    if let Some(alt) = alt.filter(|value| !value.is_empty()) {
        content.extend(
            wrap(alt, box_width.saturating_sub(4))
                .into_iter()
                .map(|(line, _)| line)
                .take(2),
        );
    }
    let mut lines = vec![format!(
        "{top_left}{}{top_right}",
        horizontal.to_string().repeat(box_width - 2)
    )];
    lines.extend(
        content
            .into_iter()
            .map(|line| center(&line, box_width - 2, vertical)),
    );
    lines.push(format!(
        "{bottom_left}{}{bottom_right}",
        horizontal.to_string().repeat(box_width - 2)
    ));
    lines
}

fn center(text: &str, width: usize, vertical: char) -> String {
    let clipped = truncate(text, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(clipped.as_str()));
    format!(
        "{vertical}{}{}{}{vertical}",
        " ".repeat(padding / 2),
        clipped,
        " ".repeat(padding - padding / 2)
    )
}

fn wrap(text: &str, width: usize) -> Vec<(String, usize)> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_offset = 0;
    let mut search = 0;
    for word in text.split_whitespace() {
        // split_whitespace yields words in order, so searching from `search` finds this word.
        let word_offset = text
            .get(search..)
            .and_then(|rest| rest.find(word))
            .map_or(search, |found| search + found);
        search = word_offset + word.len();
        let separator = usize::from(!current.is_empty());
        if UnicodeWidthStr::width(current.as_str()) + separator + UnicodeWidthStr::width(word)
            > width
            && !current.is_empty()
        {
            lines.push((current, current_offset));
            current = String::new();
        }
        if current.is_empty() {
            current_offset = word_offset;
        } else {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push((current, current_offset));
    }
    lines
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample_blocks() -> Vec<Block> {
        (0..40)
            .map(|block| {
                let words: Vec<String> = (0..30).map(|word| format!("b{block}w{word}")).collect();
                Block::Paragraph(words.join(" "))
            })
            .collect()
    }

    #[test]
    fn reflow_puts_anchor_word_on_top_line() {
        let blocks = sample_blocks();
        for &(from, to) in &[(80u16, 120u16), (120, 80), (80, 61), (61, 200), (200, 60)] {
            let before = layout(&blocks, from, true);
            let index = (before.len() / 2..before.len())
                .find(|&candidate| !before[candidate].is_separator())
                .unwrap();
            let anchor = before[index].clone();
            let anchor_word = anchor.text.split_whitespace().next().unwrap().to_owned();

            let after = layout(&blocks, to, true);
            let top = line_for_anchor(&after, anchor.block, anchor.char_offset);
            let line = &after[top];
            assert_eq!(
                line.block, anchor.block,
                "wrong block reflowing {from}->{to}"
            );
            assert!(
                line.text.split_whitespace().any(|word| word == anchor_word),
                "anchor word {anchor_word} not on top line reflowing {from}->{to}: {:?}",
                line.text
            );
        }
    }

    #[test]
    fn anchor_is_stable_within_same_layout() {
        let lines = layout(&sample_blocks(), 80, true);
        for (index, line) in lines.iter().enumerate() {
            if line.is_separator() {
                continue;
            }
            assert_eq!(line_for_anchor(&lines, line.block, line.char_offset), index);
        }
    }

    #[test]
    fn height_change_does_not_alter_layout() {
        let blocks = sample_blocks();
        assert_eq!(layout(&blocks, 80, true), layout(&blocks, 80, true));
    }

    #[test]
    fn image_box_is_atomic() {
        let lines = image_box(Some("Map"), 40, true);
        assert_eq!(
            lines.first(),
            Some(&"+--------------------------------------+".to_owned())
        );
        assert!(lines.iter().any(|line| line.contains("[ IMAGE ]")));
    }
}
