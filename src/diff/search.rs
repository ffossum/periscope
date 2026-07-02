//! Search state: patterns, the hits they produce, and the focused hit.

use regex::{Regex, RegexBuilder};

/// Which part of a row a search hit lives in.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum MatchSide {
    Left,
    Right,
    Full,
}

/// A single regex hit, located precisely enough to scroll to and highlight.
#[derive(Clone, Copy)]
pub(super) struct SearchMatch {
    pub(super) file: usize,
    pub(super) row: usize,
    pub(super) side: MatchSide,
    /// Char range within the cell/row content (gutter excluded).
    pub(super) start: usize,
    pub(super) end: usize,
    /// Absolute row in the stacked layout, for vertical scrolling.
    pub(super) vrow: u16,
}

/// An executed search: the pattern, all its hits, and the focused one.
pub(super) struct Search {
    pub(super) pattern: String,
    pub(super) matches: Vec<SearchMatch>,
    pub(super) current: usize,
}

/// Compile a user pattern with smartcase: case-insensitive unless the pattern
/// itself contains an uppercase letter.
pub(super) fn build_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let case_insensitive = !pattern.chars().any(|c| c.is_uppercase());
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
}

/// Every non-empty match of `re` in `text`, as char (not byte) ranges so they
/// line up with the char-indexed render masks.
pub(super) fn find_ranges(re: &Regex, text: &str) -> Vec<(usize, usize)> {
    re.find_iter(text)
        .filter(|m| m.start() < m.end())
        .map(|m| {
            let start = text[..m.start()].chars().count();
            let end = text[..m.end()].chars().count();
            (start, end)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_ranges_uses_char_offsets() {
        // The é is two bytes but one char; ranges must be char-based so they
        // line up with the char-indexed render masks.
        let re = build_regex("bar").unwrap();
        assert_eq!(find_ranges(&re, "éfoo barbar"), vec![(5, 8), (8, 11)]);
    }

    #[test]
    fn find_ranges_skips_empty_matches() {
        let re = build_regex("x*").unwrap();
        assert!(find_ranges(&re, "abc").is_empty());
    }

    #[test]
    fn build_regex_smartcase() {
        // All-lowercase patterns match case-insensitively...
        assert!(build_regex("todo").unwrap().is_match("TODO"));
        // ...but an uppercase letter makes the search case-sensitive.
        assert!(!build_regex("Todo").unwrap().is_match("todo"));
    }
}
