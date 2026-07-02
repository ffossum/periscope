//! Render rows to ratatui [`Line`]s: two-column layout, horizontal scroll,
//! intra-line emphasis, and search-hit highlighting.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::NUM_WIDTH;
use super::build::{Row, SideLine};
use super::palette::Palette;
use super::parse::SideKind;
use super::search::MatchSide;

/// Char ranges to highlight within one row, split by where they fall.
#[derive(Default)]
pub(super) struct RowHls {
    pub(super) left: Vec<(usize, usize)>,
    pub(super) right: Vec<(usize, usize)>,
    pub(super) full: Vec<(usize, usize)>,
}

/// The background category of a content char, in increasing priority.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    Plain,
    Emph,
    Match,
    Current,
}

/// A boolean mask of length `n` with the given char ranges set true.
fn ranges_mask(n: usize, ranges: &[(usize, usize)]) -> Vec<bool> {
    let mut mask = vec![false; n];
    for &(start, end) in ranges {
        for slot in mask.iter_mut().take(end.min(n)).skip(start) {
            *slot = true;
        }
    }
    mask
}

/// Render a row to a single full-width `Line`, split into two columns.
///
/// `hscroll` slides the content of both halves left by that many columns; the
/// line-number gutters stay fixed. Full-width rows (hunk bands, verbatim text)
/// are not horizontally scrolled. `hl` carries this row's search-hit ranges and
/// `current` the focused hit (side + range) when it lands on this row.
pub(super) fn render_row(
    row: &Row,
    width: usize,
    hscroll: usize,
    hl: Option<&RowHls>,
    current: Option<(MatchSide, usize, usize)>,
    p: &Palette,
) -> Line<'static> {
    match row {
        Row::Full(text, style) => {
            let hls = hl.map_or(&[][..], |h| h.full.as_slice());
            let cur = current.and_then(|(side, s, e)| (side == MatchSide::Full).then_some((s, e)));
            if hls.is_empty() && cur.is_none() {
                Line::from(Span::styled(fit(text, width), *style))
            } else {
                full_spans(text, width, *style, hls, cur, p)
            }
        }
        Row::Pair { left, right } => {
            // One column for the center separator, the rest split evenly.
            let avail = width.saturating_sub(1);
            let left_w = avail / 2;
            let right_w = avail - left_w;

            let (lh, rh) = match hl {
                Some(h) => (h.left.as_slice(), h.right.as_slice()),
                None => (&[][..], &[][..]),
            };
            let lc = current.and_then(|(side, s, e)| (side == MatchSide::Left).then_some((s, e)));
            let rc = current.and_then(|(side, s, e)| (side == MatchSide::Right).then_some((s, e)));

            let mut spans = cell_spans(left.as_ref(), left_w, hscroll, lh, lc, p);
            spans.push(Span::styled(
                "│".to_string(),
                Style::default().fg(p.separator).bg(p.bg),
            ));
            spans.extend(cell_spans(right.as_ref(), right_w, hscroll, rh, rc, p));
            Line::from(spans)
        }
    }
}

/// Render a full-width row with search hits highlighted.
fn full_spans(
    text: &str,
    width: usize,
    base: Style,
    hls: &[(usize, usize)],
    current: Option<(usize, usize)>,
    p: &Palette,
) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let hl_mask = ranges_mask(n, hls);
    let cur_mask = ranges_mask(n, current.as_slice());
    let search = Style::default().fg(p.search_fg).bg(p.search_bg);
    let search_cur = Style::default().fg(p.search_fg).bg(p.search_current_bg);
    let style_for = |class| match class {
        Class::Current => search_cur,
        Class::Match => search,
        _ => base,
    };

    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_class = Class::Plain;
    for (i, ch) in chars.into_iter().enumerate().take(width) {
        let class = if cur_mask[i] {
            Class::Current
        } else if hl_mask[i] {
            Class::Match
        } else {
            Class::Plain
        };
        if !run.is_empty() && class != run_class {
            spans.push(Span::styled(std::mem::take(&mut run), style_for(run_class)));
        }
        run.push(ch);
        run_class = class;
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style_for(run_class)));
    }
    let used = n.min(width);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), base));
    }
    Line::from(spans)
}

/// Build the spans for one side of a pair, fitted to `width`, with the content
/// (but not the gutter) scrolled left by `hscroll` columns. `hls`/`current`
/// carry the search hits to highlight within this cell.
fn cell_spans(
    side: Option<&SideLine>,
    width: usize,
    hscroll: usize,
    hls: &[(usize, usize)],
    current: Option<(usize, usize)>,
    p: &Palette,
) -> Vec<Span<'static>> {
    let Some(s) = side else {
        return vec![Span::raw(" ".repeat(width))];
    };

    // `bg` paints the whole cell; `emph_bg` highlights chars that changed
    // relative to the paired line on the other side.
    let (bg, emph_bg) = match s.kind {
        SideKind::Removed => (Some(p.removed_bg), Some(p.removed_emph_bg)),
        SideKind::Added => (Some(p.added_bg), Some(p.added_emph_bg)),
        SideKind::Context => (Some(p.bg), Some(p.bg)),
    };
    let marker = match s.kind {
        SideKind::Removed => '-',
        SideKind::Added => '+',
        SideKind::Context => ' ',
    };
    let gutter_fg = match s.kind {
        SideKind::Removed => p.removed_gutter,
        SideKind::Added => p.added_gutter,
        SideKind::Context => p.gutter,
    };

    // Theme foreground is the default; syntax-colored segs override it below.
    let base = match bg {
        Some(b) => Style::default().fg(p.fg).bg(b),
        None => Style::default().fg(p.fg).bg(p.bg),
    };
    let emph = emph_bg.map_or(base, |b| base.bg(b));

    let gutter = format!("{:>NUM_WIDTH$} {marker}", s.num);
    let mut spans = vec![Span::styled(gutter, base.fg(gutter_fg))];

    // Per-char masks for search hits, sized to the cell's content length.
    let content_len: usize = s.segs.iter().map(|seg| seg.text.chars().count()).sum();
    let hl_mask = ranges_mask(content_len, hls);
    let cur_mask = ranges_mask(content_len, current.as_slice());
    let search = Style::default().fg(p.search_fg).bg(p.search_bg);
    let search_cur = Style::default().fg(p.search_fg).bg(p.search_current_bg);

    // Walk segs and the masks together, breaking each syntax run wherever the
    // background category changes (intra-line emphasis or a search hit).
    let mut ci = 0usize;
    for seg in &s.segs {
        let plain = match seg.fg {
            Some(fg) => base.fg(fg),
            None => base,
        };
        let lit = match seg.fg {
            Some(fg) => emph.fg(fg),
            None => emph,
        };
        let style_for = |class| match class {
            Class::Plain => plain,
            Class::Emph => lit,
            Class::Match => search,
            Class::Current => search_cur,
        };
        let mut run = String::new();
        let mut run_class = Class::Plain;
        for ch in seg.text.chars() {
            let idx = ci;
            ci += 1;
            if ci <= hscroll {
                continue; // scrolled off to the left
            }
            let class = if cur_mask[idx] {
                Class::Current
            } else if hl_mask[idx] {
                Class::Match
            } else if s.emph.get(idx).copied().unwrap_or(false) {
                Class::Emph
            } else {
                Class::Plain
            };
            if !run.is_empty() && class != run_class {
                spans.push(Span::styled(std::mem::take(&mut run), style_for(run_class)));
            }
            run.push(ch);
            run_class = class;
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, style_for(run_class)));
        }
    }
    fit_spans(spans, width, base)
}

/// Truncate or pad a sequence of spans to exactly `width` columns.
fn fit_spans(spans: Vec<Span<'static>>, width: usize, pad_style: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let count = span.content.chars().count();
        if count <= remaining {
            used += count;
            out.push(span);
        } else {
            let text: String = span.content.chars().take(remaining).collect();
            out.push(Span::styled(text, span.style));
            used = width;
        }
    }
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), pad_style));
    }
    out
}

/// Truncate or pad `s` to exactly `width` columns (char-based).
fn fit(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    let len = out.chars().count();
    if len < width {
        out.extend(std::iter::repeat_n(' ', width - len));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_mask_sets_only_covered_chars() {
        assert_eq!(
            ranges_mask(5, &[(1, 3)]),
            vec![false, true, true, false, false]
        );
        // Ranges past the end are clamped, not panicking.
        assert_eq!(ranges_mask(2, &[(1, 9)]), vec![false, true]);
    }
}
