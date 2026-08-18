use std::time::Instant;

use crossterm::event::KeyCode;

#[derive(Debug)]
pub struct TextInput {
    value: String,
    cursor: usize,
    blink_epoch: Instant,
}

impl TextInput {
    #[must_use]
    pub fn new(value: String) -> Self {
        let cursor = value.len();
        Self {
            value,
            cursor,
            blink_epoch: Instant::now(),
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
        self.cursor = self
            .value
            .char_indices()
            .find_map(|(index, _)| (index >= target).then_some(index))
            .unwrap_or(self.value.len());
        self.reset_blink();
    }

    #[must_use]
    pub fn render(&self, label: &str) -> String {
        let before = self.value.get(..self.cursor).unwrap_or_default();
        let after = self.value.get(self.cursor..).unwrap_or_default();
        let caret = if Instant::now().duration_since(self.blink_epoch).as_millis() / 500 % 2 == 0 {
            "|"
        } else {
            " "
        };
        format!("{label}{before}{caret}{after}")
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
