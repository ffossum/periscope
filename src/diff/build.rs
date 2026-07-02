//! Turn parsed files into renderable [`FileDiff`]s: apply syntax highlighting,
//! pair removed/added lines side by side, and compute intra-line emphasis.

use std::path::Path;

use ratatui::style::{Color, Style};
use similar::{ChangeTag, TextDiff};
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};

use super::TAB_WIDTH;
use super::palette::{Palette, conv, load_theme};
use super::parse::{ParsedFile, ParsedRow, SideKind};

pub(super) struct Seg {
    pub(super) text: String,
    pub(super) fg: Option<Color>,
}

/// One side (left or right) of a paired diff row.
pub(super) struct SideLine {
    pub(super) num: usize,
    pub(super) segs: Vec<Seg>,
    pub(super) kind: SideKind,
    /// Per-content-char mask of intra-line changes (empty = nothing emphasized).
    /// Indexed by char position across the concatenated `segs` text.
    pub(super) emph: Vec<bool>,
}

/// A single content row within a file block.
pub(super) enum Row {
    /// A full-width line, e.g. a hunk header.
    Full(String, Style),
    /// A pair of cells shown side by side. Either side may be empty.
    Pair {
        left: Option<SideLine>,
        right: Option<SideLine>,
    },
}

/// One file's diff: a titled, bordered block of rows.
#[derive(Default)]
pub(super) struct FileDiff {
    pub(super) title: String,
    pub(super) rows: Vec<Row>,
}

impl FileDiff {
    /// Total rendered height including the top and bottom borders.
    pub(super) fn height(&self) -> u16 {
        (self.rows.len() as u16).saturating_add(2)
    }
}

/// Turn parsed files into renderable [`FileDiff`]s: apply syntax highlighting,
/// pair removed/added lines side by side, and compute intra-line emphasis.
pub(super) fn build_files(parsed: Vec<ParsedFile>) -> (Vec<FileDiff>, Palette) {
    let syntaxes = SyntaxSet::load_defaults_newlines();
    let theme = load_theme();
    let palette = Palette::from_theme(&theme);

    let files = parsed
        .into_iter()
        .map(|file| build_file(file, &syntaxes, &theme, &palette))
        .collect();
    (files, palette)
}

/// The longest content line (in chars) across every paired cell, used to clamp
/// horizontal scrolling. Full-width rows don't scroll, so they're ignored.
pub(super) fn max_line_width(files: &[FileDiff]) -> u16 {
    let side_width = |side: &Option<SideLine>| {
        side.as_ref()
            .map(|s| s.segs.iter().map(|seg| seg.text.chars().count()).sum())
            .unwrap_or(0)
    };
    let max = files
        .iter()
        .flat_map(|f| &f.rows)
        .filter_map(|row| match row {
            Row::Pair { left, right } => Some(side_width(left).max(side_width(right))),
            Row::Full(..) => None,
        })
        .max()
        .unwrap_or(0);
    max.min(u16::MAX as usize) as u16
}

/// The full content text of one side of a pair, used as the search haystack.
pub(super) fn side_text(s: &SideLine) -> String {
    s.segs.iter().map(|seg| seg.text.as_str()).collect()
}

/// Build one file's renderable rows from its parsed form.
fn build_file(
    file: ParsedFile,
    syntaxes: &SyntaxSet,
    theme: &Theme,
    palette: &Palette,
) -> FileDiff {
    let syntax = syntax_for_title(syntaxes, &file.title);
    // Show renames as `old → new`; otherwise just the path. A file-type glyph
    // (chosen from the current path) is prepended either way.
    let name = match &file.rename_from {
        Some(from) if *from != file.title => format!("{from} → {}", file.title),
        _ => file.title.clone(),
    };
    let title = format!("{} {name}", file_icon(&file.title));
    let mut rows: Vec<Row> = Vec::new();
    // Buffered runs of removed/added lines, paired when the run ends.
    let mut removed: Vec<SideLine> = Vec::new();
    let mut added: Vec<SideLine> = Vec::new();
    // A per-hunk highlighter for each side, reset at every hunk header.
    let mut old_hl: Option<HighlightLines> = None;
    let mut new_hl: Option<HighlightLines> = None;

    let flush = |rows: &mut Vec<Row>, removed: &mut Vec<SideLine>, added: &mut Vec<SideLine>| {
        let pairs = removed.len().max(added.len());
        let mut rem = removed.drain(..);
        let mut add = added.drain(..);
        for _ in 0..pairs {
            let mut left = rem.next();
            let mut right = add.next();
            if let (Some(l), Some(r)) = (left.as_mut(), right.as_mut()) {
                mark_intra_line(l, r);
            }
            rows.push(Row::Pair { left, right });
        }
    };

    for row in file.rows {
        match row {
            ParsedRow::Hunk(text) => {
                flush(&mut rows, &mut removed, &mut added);
                old_hl = syntax.map(|s| HighlightLines::new(s, theme));
                new_hl = syntax.map(|s| HighlightLines::new(s, theme));
                // Pad the hunk header with a blank row above and below, sharing
                // the header's style so it reads as one band. The first hunk
                // gets no top pad — it already sits under the block's top border.
                if !rows.is_empty() {
                    rows.push(blank_row(hunk_style(palette)));
                }
                rows.push(Row::Full(format!(" {text}"), hunk_style(palette)));
                rows.push(blank_row(hunk_style(palette)));
            }
            ParsedRow::Verbatim(text) => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push(Row::Full(text, Style::default()));
            }
            ParsedRow::Content {
                kind: SideKind::Added,
                new,
                text,
                ..
            } => {
                let segs = highlight(&mut new_hl, syntaxes, &expand_tabs(&text));
                added.push(SideLine {
                    num: new.unwrap_or(0),
                    segs,
                    kind: SideKind::Added,
                    emph: Vec::new(),
                });
            }
            ParsedRow::Content {
                kind: SideKind::Removed,
                old,
                text,
                ..
            } => {
                let segs = highlight(&mut old_hl, syntaxes, &expand_tabs(&text));
                removed.push(SideLine {
                    num: old.unwrap_or(0),
                    segs,
                    kind: SideKind::Removed,
                    emph: Vec::new(),
                });
            }
            ParsedRow::Content {
                kind: SideKind::Context,
                old,
                new,
                text,
            } => {
                flush(&mut rows, &mut removed, &mut added);
                let expanded = expand_tabs(&text);
                let left = highlight(&mut old_hl, syntaxes, &expanded);
                let right = highlight(&mut new_hl, syntaxes, &expanded);
                rows.push(Row::Pair {
                    left: Some(SideLine {
                        num: old.unwrap_or(0),
                        segs: left,
                        kind: SideKind::Context,
                        emph: Vec::new(),
                    }),
                    right: Some(SideLine {
                        num: new.unwrap_or(0),
                        segs: right,
                        kind: SideKind::Context,
                        emph: Vec::new(),
                    }),
                });
            }
        }
    }

    flush(&mut rows, &mut removed, &mut added);
    FileDiff { title, rows }
}

/// Highlight one already-tab-expanded line into colored segments.
///
/// Falls back to a single uncolored segment when there is no syntax for the
/// file or the highlighter reports an error.
fn highlight(hl: &mut Option<HighlightLines>, syntaxes: &SyntaxSet, text: &str) -> Vec<Seg> {
    let Some(h) = hl.as_mut() else {
        return vec![Seg {
            text: text.to_string(),
            fg: None,
        }];
    };

    // syntect's default syntaxes expect a trailing newline per line.
    let line = format!("{text}\n");
    match h.highlight_line(&line, syntaxes) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, s)| Seg {
                text: s.trim_end_matches('\n').to_string(),
                fg: Some(conv(style.foreground)),
            })
            .filter(|seg| !seg.text.is_empty())
            .collect(),
        Err(_) => vec![Seg {
            text: text.to_string(),
            fg: None,
        }],
    }
}

/// Compute character-level changes between a paired removed/added line and
/// store the result as a per-char mask on each side.
///
/// Skipped when the two lines are too dissimilar (a near-total rewrite), where
/// emphasizing nearly every character would just be noise on top of the
/// add/remove line background.
fn mark_intra_line(left: &mut SideLine, right: &mut SideLine) {
    let old: String = left.segs.iter().map(|s| s.text.as_str()).collect();
    let new: String = right.segs.iter().map(|s| s.text.as_str()).collect();

    // Gauge similarity on the content alone: shared leading indentation
    // shouldn't make an otherwise-complete rewrite look similar enough to mark.
    if TextDiff::from_chars(old.trim_start(), new.trim_start()).ratio() < 0.5 {
        return;
    }

    let diff = TextDiff::from_chars(&old, &new);
    let mut lmask = vec![false; old.chars().count()];
    let mut rmask = vec![false; new.chars().count()];
    let (mut li, mut ri) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                li += 1;
                ri += 1;
            }
            ChangeTag::Delete => {
                lmask[li] = true;
                li += 1;
            }
            ChangeTag::Insert => {
                rmask[ri] = true;
                ri += 1;
            }
        }
    }

    left.emph = lmask;
    right.emph = rmask;
}

fn expand_tabs(s: &str) -> String {
    s.replace('\t', &" ".repeat(TAB_WIDTH))
}

/// Look up a syntax from a cleaned file title like `src/main.rs`.
fn syntax_for_title<'a>(syntaxes: &'a SyntaxSet, title: &str) -> Option<&'a SyntaxReference> {
    let ext = Path::new(title).extension()?.to_str()?;
    syntaxes.find_syntax_by_extension(ext)
}

/// A Nerd Font glyph for a file, chosen by filename then extension.
///
/// Codepoints are from the Nerd Fonts private-use range; the comments name the
/// glyph class so they're easy to tweak. Falls back to a generic file glyph.
fn file_icon(path: &str) -> char {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    // A few well-known extensionless files matched by whole name.
    match name.to_ascii_lowercase().as_str() {
        "dockerfile" => return '\u{f308}', // nf-linux-docker
        "makefile" => return '\u{e673}',   // nf-seti-makefile
        "license" => return '\u{f0fc7}',   // nf-md-certificate
        ".gitignore" | ".gitattributes" => return '\u{e702}', // nf-dev-git
        _ => {}
    }

    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => '\u{e7a8}',                                // nf-dev-rust
        "py" => '\u{e606}',                                // nf-seti-python
        "js" | "mjs" | "cjs" => '\u{e74e}',                // nf-dev-javascript
        "ts" => '\u{e628}',                                // nf-seti-typescript
        "jsx" | "tsx" => '\u{e7ba}',                       // nf-dev-react
        "html" | "htm" => '\u{e736}',                      // nf-dev-html5
        "css" => '\u{e749}',                               // nf-dev-css3
        "scss" | "sass" => '\u{e603}',                     // nf-seti-sass
        "json" => '\u{e60b}',                              // nf-seti-json
        "md" | "markdown" => '\u{e73e}',                   // nf-dev-markdown
        "c" | "h" => '\u{e61e}',                           // nf-custom-c
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => '\u{e61d}', // nf-custom-cpp
        "go" => '\u{e627}',                                // nf-seti-go
        "java" => '\u{e738}',                              // nf-dev-java
        "scala" | "sc" => '\u{e737}',                      // nf-dev-scala
        "rb" => '\u{e739}',                                // nf-dev-ruby
        "sh" | "bash" | "zsh" | "fish" => '\u{e795}',      // nf-seti-shell
        "toml" => '\u{e6b2}',                              // nf-seti-config-ish
        "yaml" | "yml" => '\u{e615}',                      // nf-seti-yml
        "lock" => '\u{f023}',                              // nf-fa-lock
        "txt" => '\u{f15c}',                               // nf-fa-file_text
        _ => '\u{f15b}',                                   // nf-fa-file (generic)
    }
}

fn hunk_style(p: &Palette) -> Style {
    Style::default().fg(p.hunk_fg).bg(p.hunk_bg)
}

/// A blank full-width padding row in the given style.
fn blank_row(style: Style) -> Row {
    Row::Full(String::new(), style)
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse;
    use super::*;

    #[test]
    fn rename_title_shows_arrow() {
        let raw = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 92%
rename from old/name.rs
rename to new/name.rs
--- a/old/name.rs
+++ b/new/name.rs
@@ -1 +1 @@
-a
+b
";
        let parsed = parse(raw);
        assert_eq!(parsed[0].rename_from.as_deref(), Some("old/name.rs"));
        assert_eq!(parsed[0].title, "new/name.rs");
        // Title carries a leading file-type glyph, then the rename arrow.
        assert!(
            build_files(parsed).0[0]
                .title
                .ends_with("old/name.rs → new/name.rs")
        );
    }

    #[test]
    fn file_icon_by_extension_and_name() {
        assert_eq!(file_icon("src/main.rs"), '\u{e7a8}');
        assert_eq!(file_icon("Dockerfile"), '\u{f308}');
        assert_eq!(file_icon("weird.unknownext"), '\u{f15b}');
    }
}
