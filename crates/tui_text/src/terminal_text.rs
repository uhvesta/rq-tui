//! Shared terminal-text semantics.
//!
//! Cursor positions are byte offsets at extended grapheme-cluster boundaries,
//! while layout widths are terminal cells. Keeping those units explicit avoids
//! splitting emoji, flags, or combining sequences during movement and edits.

use unicode_segmentation::{GraphemeIndices, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

pub fn grapheme_indices(text: &str) -> GraphemeIndices<'_> {
    text.grapheme_indices(true)
}

pub fn cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn floor_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor == text.len() {
        return cursor;
    }
    text.grapheme_indices(true)
        .map(|(index, _)| index)
        .rfind(|index| *index <= cursor)
        .unwrap_or(0)
}

pub fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = floor_grapheme_boundary(text, cursor);
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

pub fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let cursor = floor_grapheme_boundary(text, cursor);
    text[cursor..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::{
        cell_width, floor_grapheme_boundary, next_grapheme_boundary, previous_grapheme_boundary,
    };

    #[test]
    fn boundaries_keep_combining_emoji_and_flags_atomic() {
        let text = "e\u{301} 👩\u{200d}💻 🇨🇦";
        let laptop = text.find('👩').unwrap();
        let after_laptop = laptop + "👩\u{200d}💻".len();
        assert_eq!(next_grapheme_boundary(text, laptop), after_laptop);
        assert_eq!(previous_grapheme_boundary(text, after_laptop), laptop);
        assert_eq!(
            floor_grapheme_boundary(text, laptop + '👩'.len_utf8()),
            laptop
        );
        assert_eq!(cell_width("e\u{301}"), 1);
        assert_eq!(cell_width("👩\u{200d}💻"), 2);
        assert_eq!(cell_width("🇨🇦"), 2);
    }
}
