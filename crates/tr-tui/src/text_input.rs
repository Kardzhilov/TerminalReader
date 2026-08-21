use std::time::Instant;

use crossterm::event::KeyCode;
use unicode_width::UnicodeWidthChar;

#[derive(Debug)]
pub struct TextInput {
    value: String,
    cursor: usize,
    masked: bool,
    blink_epoch: Instant,
}

impl TextInput {
    #[must_use]
    pub fn new(value: String) -> Self {
        let cursor = value.len();
        Self {
            value,
            cursor,
            masked: false,
            blink_epoch: Instant::now(),
        }
    }

    /// Input that renders every character as `*` (for passwords).
    #[must_use]
    pub fn masked() -> Self {
        Self {
            masked: true,
            ..Self::new(String::new())
        }
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.reset_blink();
    }

    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        let changed = match key {
            KeyCode::Left => {
                if let Some(boundary) = self.previous_boundary() {
                    self.cursor = boundary;
                }
                false
            }
            KeyCode::Right => {
                if let Some(boundary) = self.next_boundary() {
                    self.cursor = boundary;
                }
                false
            }
            KeyCode::Home => {
                self.cursor = 0;
                false
            }
            KeyCode::End => {
                self.cursor = self.value.len();
                false
            }
            KeyCode::Backspace => self.delete(false),
            KeyCode::Delete => self.delete(true),
            KeyCode::Char(character) => {
                self.value.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                true
            }
            _ => false,
        };
        self.reset_blink();
        changed
    }

    pub fn click(&mut self, column_offset: u16) {
        let target = usize::from(column_offset);
        let mut column = 0;
        self.cursor = self.value.len();
        for (index, character) in self.value.char_indices() {
            if column >= target {
                self.cursor = index;
                break;
            }
            // Masked inputs render every character one column wide.
            column += if self.masked {
                1
            } else {
                UnicodeWidthChar::width(character).unwrap_or(0)
            };
        }
        self.reset_blink();
    }

    /// Render with the caret blinking, or held steady when `blink` is off
    /// (a reduced-motion preference).
    #[must_use]
    pub fn render_caret(&self, label: &str, blink: bool) -> String {
        let display = if self.masked {
            "*".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        };
        let blink_on =
            !blink || Instant::now().duration_since(self.blink_epoch).as_millis() / 500 % 2 == 0;
        if !blink_on {
            return format!("{label}{display}");
        }
        // Overlay the caret on the char under the cursor so the tail never shifts.
        let cursor_chars = self
            .value
            .get(..self.cursor)
            .map_or(0, |prefix| prefix.chars().count());
        let mut overlaid = String::with_capacity(display.len() + 4);
        let mut at_cursor = false;
        for (index, character) in display.chars().enumerate() {
            if index == cursor_chars {
                at_cursor = true;
                let width = UnicodeWidthChar::width(character).unwrap_or(1).max(1);
                overlaid.push_str(&"█".repeat(width));
            } else {
                overlaid.push(character);
            }
        }
        if !at_cursor {
            overlaid.push('█');
        }
        format!("{label}{overlaid}")
    }

    fn delete(&mut self, forward: bool) -> bool {
        let range = if forward {
            self.next_boundary().map(|next| self.cursor..next)
        } else {
            self.previous_boundary()
                .map(|previous| previous..self.cursor)
        };
        if let Some(range) = range {
            self.value.replace_range(range.clone(), "");
            self.cursor = range.start;
            true
        } else {
            false
        }
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.value
            .get(..self.cursor)
            .and_then(|prefix| prefix.char_indices().next_back().map(|(index, _)| index))
    }

    fn next_boundary(&self) -> Option<usize> {
        self.value.get(self.cursor..).and_then(|suffix| {
            suffix
                .chars()
                .next()
                .map(|character| self.cursor + character.len_utf8())
        })
    }

    fn reset_blink(&mut self) {
        self.blink_epoch = Instant::now();
    }
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new(String::new())
    }
}
