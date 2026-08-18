//! `KOReader` crengine xpointer progress strings.
//!
//! `KOReader` stores EPUB reading progress as strings such as
//! `/body/DocFragment[7]/body/div/p[3].0`: a 1-based spine fragment index,
//! an element path within the chapter, and a character offset.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XPointerStep {
    pub name: String,
    /// 1-based ordinal among same-named siblings.
    pub ordinal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XPointer {
    /// 1-based index into the spine (`KOReader` `DocFragment`).
    pub fragment: usize,
    /// Element steps below the fragment (starting at the chapter `body`).
    pub steps: Vec<XPointerStep>,
    /// Character offset within the target element's text.
    pub offset: usize,
}

impl XPointer {
    /// Render in `KOReader`'s crengine form, omitting `[1]` ordinals.
    #[must_use]
    pub fn format(&self) -> String {
        use std::fmt::Write;
        let mut out = format!("/body/DocFragment[{}]", self.fragment);
        for step in &self.steps {
            out.push('/');
            out.push_str(&step.name);
            if step.ordinal > 1 {
                let _ = write!(out, "[{}]", step.ordinal);
            }
        }
        let _ = write!(out, ".{}", self.offset);
        out
    }

    /// Parse a crengine xpointer; returns `None` for non-xpointer progress
    /// strings (page numbers, percentages).
    #[must_use]
    pub fn parse(pointer: &str) -> Option<Self> {
        let rest = pointer.strip_prefix("/body/DocFragment[")?;
        let (fragment_text, rest) = rest.split_once(']')?;
        let fragment: usize = fragment_text.parse().ok()?;
        if fragment == 0 {
            return None;
        }
        // The offset suffix is the final `.N`; missing offsets default to 0.
        let (path, offset) = match rest.rsplit_once('.') {
            Some((path, offset_text)) => (path, offset_text.parse().ok()?),
            None => (rest, 0),
        };
        let mut steps = Vec::new();
        for segment in path.split('/') {
            if segment.is_empty() {
                continue;
            }
            let (name, ordinal) = match segment.split_once('[') {
                Some((name, ordinal_rest)) => {
                    let ordinal_text = ordinal_rest.strip_suffix(']')?;
                    (name, ordinal_text.parse().ok()?)
                }
                None => (segment, 1),
            };
            if name.is_empty() || ordinal == 0 {
                return None;
            }
            steps.push(XPointerStep {
                name: name.to_owned(),
                ordinal,
            });
        }
        Some(Self {
            fragment,
            steps,
            offset,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn step(name: &str, ordinal: usize) -> XPointerStep {
        XPointerStep {
            name: name.to_owned(),
            ordinal,
        }
    }

    #[test]
    fn formats_koreader_style_pointer() {
        let pointer = XPointer {
            fragment: 7,
            steps: vec![step("body", 1), step("div", 1), step("p", 3)],
            offset: 0,
        };
        assert_eq!(pointer.format(), "/body/DocFragment[7]/body/div/p[3].0");
    }

    #[test]
    fn parses_pointer_with_and_without_ordinals() {
        let parsed = XPointer::parse("/body/DocFragment[7]/body/div[2]/p[3].15").unwrap();
        assert_eq!(parsed.fragment, 7);
        assert_eq!(
            parsed.steps,
            vec![step("body", 1), step("div", 2), step("p", 3)]
        );
        assert_eq!(parsed.offset, 15);
    }

    #[test]
    fn parses_text_node_steps() {
        let parsed = XPointer::parse("/body/DocFragment[2]/body/p/text().125").unwrap();
        assert_eq!(
            parsed.steps,
            vec![step("body", 1), step("p", 1), step("text()", 1)]
        );
        assert_eq!(parsed.offset, 125);
    }

    #[test]
    fn round_trips_through_format_and_parse() {
        let pointer = XPointer {
            fragment: 12,
            steps: vec![step("body", 1), step("section", 2), step("p", 9)],
            offset: 42,
        };
        assert_eq!(XPointer::parse(&pointer.format()).unwrap(), pointer);
    }

    #[test]
    fn rejects_non_xpointer_progress_strings() {
        assert_eq!(XPointer::parse("42"), None);
        assert_eq!(XPointer::parse("0.5312"), None);
        assert_eq!(XPointer::parse("/body/DocFragment[0]/body.0"), None);
        assert_eq!(XPointer::parse(""), None);
    }
}
