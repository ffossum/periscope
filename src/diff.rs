use std::path::{Path, PathBuf};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Read the diff from `file`, or from stdin when no file is given.
///
/// Strips ANSI escapes so the diff parses cleanly when used as git's
/// `pager.diff`, where git colorizes the stream it pipes in. periscope does its
/// own syntax highlighting, so git's coloring is just noise here.
pub fn read_input(file: Option<PathBuf>) -> color_eyre::Result<String> {
    let raw = match file {
        Some(path) => std::fs::read_to_string(path)?,
        None => std::io::read_to_string(std::io::stdin())?,
    };
    Ok(strip_ansi_escapes::strip_str(&raw))
}

/// Width of the gutter holding a line number (plus one space) on each side.
const NUM_WIDTH: usize = 4;
/// Tabs are expanded to this many spaces so columns stay aligned.
const TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, PartialEq)]
enum SideKind {
    Context,
    Removed,
    Added,
}

/// A run of text sharing a single syntax-highlight foreground color.
struct Seg {
    text: String,
    fg: Option<Color>,
}

/// One side (left or right) of a paired diff row.
struct SideLine {
    num: usize,
    segs: Vec<Seg>,
    kind: SideKind,
}

/// A single rendered row of the viewer.
enum Row {
    /// A full-width line: file metadata or a hunk header.
    Full(String, Style),
    /// A pair of cells shown side by side. Either side may be empty.
    Pair {
        left: Option<SideLine>,
        right: Option<SideLine>,
    },
}

pub struct DiffViewer {
    running: bool,
    rows: Vec<Row>,
    scroll: u16,
    viewport_height: u16,
}

impl DiffViewer {
    pub fn new(raw: &str) -> Self {
        Self {
            running: true,
            rows: parse(raw),
            scroll: 0,
            viewport_height: 0,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let mut reader = crossterm::event::EventStream::new();

        while self.running {
            terminal.draw(|frame| self.draw(frame))?;

            if let Some(Ok(Event::Key(key))) = reader.next().await
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key);
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // Account for the top and bottom border rows.
        self.viewport_height = area.height.saturating_sub(2);
        self.clamp_scroll();

        let total = self.rows.len();
        let current = (self.scroll as usize + 1).min(total.max(1));
        let title = format!(" diff — {current}/{total} ");

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        // The inner width excludes the two vertical border columns.
        let inner_width = area.width.saturating_sub(2) as usize;
        let lines: Vec<Line> = self
            .rows
            .iter()
            .map(|row| render_row(row, inner_width))
            .collect();

        let paragraph = Paragraph::new(lines).block(block).scroll((self.scroll, 0));
        frame.render_widget(paragraph, area);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let page = self.viewport_height.max(1);
        match (key.modifiers, key.code) {
            (_, KeyCode::Char('q') | KeyCode::Esc)
            | (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.running = false,
            (_, KeyCode::Char('j') | KeyCode::Down) => self.scroll_by(1),
            (_, KeyCode::Char('k') | KeyCode::Up) => self.scroll_by(-1),
            (_, KeyCode::Char('d') | KeyCode::PageDown) => self.scroll_by(page as i32),
            (_, KeyCode::Char('u') | KeyCode::PageUp) => self.scroll_by(-(page as i32)),
            (_, KeyCode::Char('g') | KeyCode::Home) => self.scroll = 0,
            (_, KeyCode::Char('G') | KeyCode::End) => self.scroll = self.max_scroll(),
            _ => {}
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).max(0) as u16;
        self.scroll = next.min(self.max_scroll());
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn max_scroll(&self) -> u16 {
        let total = self.rows.len() as u16;
        total.saturating_sub(self.viewport_height)
    }
}

/// Parse a unified diff into side-by-side rows with syntax highlighting.
fn parse(raw: &str) -> Vec<Row> {
    let syntaxes = SyntaxSet::load_defaults_newlines();
    let mut themes = ThemeSet::load_defaults();
    let theme = themes
        .themes
        .remove("base16-ocean.dark")
        .or_else(|| themes.themes.values().next().cloned())
        .expect("syntect ships at least one default theme");

    let mut rows = Vec::new();
    // Buffered runs of removed/added lines, paired when the run ends.
    let mut removed: Vec<SideLine> = Vec::new();
    let mut added: Vec<SideLine> = Vec::new();
    let mut old_ln = 0usize;
    let mut new_ln = 0usize;
    let mut in_hunk = false;
    // Syntax for the current file, and a per-hunk highlighter for each side.
    let mut syntax: Option<&SyntaxReference> = None;
    let mut old_hl: Option<HighlightLines> = None;
    let mut new_hl: Option<HighlightLines> = None;

    let flush = |rows: &mut Vec<Row>, removed: &mut Vec<SideLine>, added: &mut Vec<SideLine>| {
        let pairs = removed.len().max(added.len());
        let mut rem = removed.drain(..);
        let mut add = added.drain(..);
        for _ in 0..pairs {
            rows.push(Row::Pair {
                left: rem.next(),
                right: add.next(),
            });
        }
    };

    for line in raw.lines() {
        if line.starts_with("@@") {
            flush(&mut rows, &mut removed, &mut added);
            (old_ln, new_ln) = parse_hunk_header(line);
            in_hunk = true;
            old_hl = syntax.map(|s| HighlightLines::new(s, &theme));
            new_hl = syntax.map(|s| HighlightLines::new(s, &theme));
            rows.push(Row::Full(line.to_string(), hunk_style()));
            continue;
        }

        if is_file_meta(line) {
            flush(&mut rows, &mut removed, &mut added);
            in_hunk = false;
            if line.starts_with("diff --git") {
                syntax = None;
            } else if let Some(rest) = line.strip_prefix("+++ ") {
                syntax = syntax_for_path(&syntaxes, rest);
            }
            rows.push(Row::Full(line.to_string(), header_style()));
            continue;
        }

        if !in_hunk {
            rows.push(Row::Full(line.to_string(), Style::default()));
            continue;
        }

        match line.chars().next() {
            Some('+') => {
                let segs = highlight(&mut new_hl, &syntaxes, &expand_tabs(&line[1..]));
                added.push(SideLine {
                    num: new_ln,
                    segs,
                    kind: SideKind::Added,
                });
                new_ln += 1;
            }
            Some('-') => {
                let segs = highlight(&mut old_hl, &syntaxes, &expand_tabs(&line[1..]));
                removed.push(SideLine {
                    num: old_ln,
                    segs,
                    kind: SideKind::Removed,
                });
                old_ln += 1;
            }
            Some('\\') => {} // "\ No newline at end of file"
            _ => {
                // Context line (leading space, or an empty line).
                flush(&mut rows, &mut removed, &mut added);
                let text = expand_tabs(line.strip_prefix(' ').unwrap_or(line));
                let left = highlight(&mut old_hl, &syntaxes, &text);
                let right = highlight(&mut new_hl, &syntaxes, &text);
                rows.push(Row::Pair {
                    left: Some(SideLine {
                        num: old_ln,
                        segs: left,
                        kind: SideKind::Context,
                    }),
                    right: Some(SideLine {
                        num: new_ln,
                        segs: right,
                        kind: SideKind::Context,
                    }),
                });
                old_ln += 1;
                new_ln += 1;
            }
        }
    }

    flush(&mut rows, &mut removed, &mut added);
    rows
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
                fg: Some(Color::Rgb(
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                )),
            })
            .filter(|seg| !seg.text.is_empty())
            .collect(),
        Err(_) => vec![Seg {
            text: text.to_string(),
            fg: None,
        }],
    }
}

/// Render a row to a single full-width `Line`, split into two columns.
fn render_row(row: &Row, width: usize) -> Line<'static> {
    match row {
        Row::Full(text, style) => Line::from(Span::styled(fit(text, width), *style)),
        Row::Pair { left, right } => {
            // One column for the center separator, the rest split evenly.
            let avail = width.saturating_sub(1);
            let left_w = avail / 2;
            let right_w = avail - left_w;

            let mut spans = cell_spans(left.as_ref(), left_w);
            spans.push(Span::styled("│".to_string(), Style::default().fg(Color::DarkGray)));
            spans.extend(cell_spans(right.as_ref(), right_w));
            Line::from(spans)
        }
    }
}

/// Build the spans for one side of a pair, fitted to `width`.
fn cell_spans(side: Option<&SideLine>, width: usize) -> Vec<Span<'static>> {
    let Some(s) = side else {
        return vec![Span::raw(" ".repeat(width))];
    };

    let bg = match s.kind {
        SideKind::Removed => Some(Color::Rgb(60, 30, 30)),
        SideKind::Added => Some(Color::Rgb(25, 50, 30)),
        SideKind::Context => None,
    };
    let marker = match s.kind {
        SideKind::Removed => '-',
        SideKind::Added => '+',
        SideKind::Context => ' ',
    };
    let gutter_fg = match s.kind {
        SideKind::Removed => Color::Red,
        SideKind::Added => Color::Green,
        SideKind::Context => Color::DarkGray,
    };

    let base = bg.map_or_else(Style::default, |b| Style::default().bg(b));

    let gutter = format!("{:>NUM_WIDTH$} {marker}", s.num);
    let mut spans = vec![Span::styled(gutter, base.fg(gutter_fg))];
    for seg in &s.segs {
        let style = match seg.fg {
            Some(fg) => base.fg(fg),
            None => base,
        };
        spans.push(Span::styled(seg.text.clone(), style));
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

fn expand_tabs(s: &str) -> String {
    s.replace('\t', &" ".repeat(TAB_WIDTH))
}

/// Look up a syntax for a diff path like `b/src/main.rs`.
fn syntax_for_path<'a>(syntaxes: &'a SyntaxSet, raw_path: &str) -> Option<&'a SyntaxReference> {
    let path = raw_path.trim();
    let path = path
        .strip_prefix("b/")
        .or_else(|| path.strip_prefix("a/"))
        .unwrap_or(path);
    if path == "/dev/null" {
        return None;
    }
    let ext = Path::new(path).extension()?.to_str()?;
    syntaxes.find_syntax_by_extension(ext)
}

/// Whether a line is file-level metadata (not part of a hunk body).
fn is_file_meta(line: &str) -> bool {
    const PREFIXES: [&str; 10] = [
        "diff --git",
        "index ",
        "--- ",
        "+++ ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "rename ",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

/// Extract the starting old and new line numbers from `@@ -a,b +c,d @@`.
fn parse_hunk_header(line: &str) -> (usize, usize) {
    let mut old = 1;
    let mut new = 1;
    for token in line.split_whitespace() {
        if let Some(rest) = token.strip_prefix('-') {
            old = rest
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
        } else if let Some(rest) = token.strip_prefix('+') {
            new = rest
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
        }
    }
    (old, new)
}

fn header_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn hunk_style() -> Style {
    Style::default().fg(Color::Cyan)
}
