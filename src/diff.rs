use std::path::{Path, PathBuf};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use similar::{ChangeTag, TextDiff};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
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
/// Blank rows between adjacent file blocks.
const FILE_GAP: u16 = 1;
/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_LINES: i32 = 3;

#[derive(Clone, Copy, PartialEq, Debug)]
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
    /// Per-content-char mask of intra-line changes (empty = nothing emphasized).
    /// Indexed by char position across the concatenated `segs` text.
    emph: Vec<bool>,
}

/// A single content row within a file block.
enum Row {
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
struct FileDiff {
    title: String,
    rows: Vec<Row>,
}

impl FileDiff {
    /// Total rendered height including the top and bottom borders.
    fn height(&self) -> u16 {
        (self.rows.len() as u16).saturating_add(2)
    }
}

pub struct DiffViewer {
    running: bool,
    files: Vec<FileDiff>,
    scroll: u16,
    viewport_height: u16,
}

impl DiffViewer {
    pub fn new(raw: &str) -> Self {
        Self {
            running: true,
            files: build_files(parse(raw)),
            scroll: 0,
            viewport_height: 0,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> color_eyre::Result<()> {
        let mut reader = crossterm::event::EventStream::new();

        while self.running {
            terminal.draw(|frame| self.draw(frame))?;

            match reader.next().await {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    self.handle_key(key)
                }
                Some(Ok(Event::Mouse(mouse))) => self.handle_mouse(mouse),
                _ => {}
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.viewport_height = area.height;
        self.clamp_scroll();

        if area.width < 3 || area.height == 0 {
            return;
        }

        let scroll = self.scroll as i32;
        let viewport = area.height as i32;
        let border_style = Style::default().fg(Color::Gray);

        // Virtual top of the current file in the full stacked layout.
        let mut top = 0i32;
        for file in &self.files {
            let block_h = file.height() as i32;
            let screen_top = top - scroll;
            top += block_h + FILE_GAP as i32;

            if screen_top >= viewport {
                break; // this file and everything after is below the viewport
            }
            // The block's on-screen row span, clipped to the viewport.
            let vis0 = screen_top.max(0);
            let vis1 = (screen_top + block_h).min(viewport);
            if vis1 <= vis0 {
                continue; // fully above the viewport
            }

            // A border is drawn only when its row is actually on-screen, so a
            // box clipped at a screen edge shows no border there.
            let local_start = vis0 - screen_top;
            let local_end = vis1 - screen_top;
            let mut borders = Borders::LEFT | Borders::RIGHT;
            if local_start == 0 {
                borders |= Borders::TOP;
            }
            if local_end == block_h {
                borders |= Borders::BOTTOM;
            }

            let mut block = Block::default().borders(borders).border_style(border_style);
            if borders.contains(Borders::TOP) {
                block = block.title(format!(" {} ", file.title));
            }

            let rect = Rect::new(
                area.x,
                area.y + vis0 as u16,
                area.width,
                (vis1 - vis0) as u16,
            );
            let inner = block.inner(rect);
            frame.render_widget(block, rect);

            // Render only the content rows visible inside the borders.
            let first = local_start.max(1) - 1;
            let last = local_end.min(block_h - 1) - 1;
            let inner_w = inner.width as usize;
            let lines: Vec<Line> = file.rows[first as usize..last as usize]
                .iter()
                .map(|row| render_row(row, inner_w))
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
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

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_by(MOUSE_SCROLL_LINES),
            MouseEventKind::ScrollUp => self.scroll_by(-MOUSE_SCROLL_LINES),
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

    fn total_height(&self) -> u16 {
        let mut total: u32 = 0;
        for (i, file) in self.files.iter().enumerate() {
            total += file.height() as u32;
            if i + 1 < self.files.len() {
                total += FILE_GAP as u32;
            }
        }
        total.min(u16::MAX as u32) as u16
    }

    fn max_scroll(&self) -> u16 {
        self.total_height().saturating_sub(self.viewport_height)
    }
}

/// A single parsed line of a diff, before any styling or pairing.
enum ParsedRow {
    /// A hunk header (`@@ ... @@`).
    Hunk(String),
    /// A line shown verbatim full-width: file metadata or stray non-diff input.
    Verbatim(String),
    /// A content line tagged with its role and 1-based line numbers. `old` is
    /// set for removed/context lines, `new` for added/context lines.
    Content {
        kind: SideKind,
        old: Option<usize>,
        new: Option<usize>,
        text: String,
    },
}

/// One file's diff as plain data, before syntax highlighting and pairing.
#[derive(Default)]
struct ParsedFile {
    /// The current (new) path; becomes the block title.
    title: String,
    /// The previous path when the file was renamed, else `None`.
    rename_from: Option<String>,
    rows: Vec<ParsedRow>,
}

/// Parse a unified diff into per-file groups of plain, unstyled rows.
///
/// This step is pure text-to-data: no syntax highlighting, no side-by-side
/// pairing, no styling. Those happen in [`build_files`].
fn parse(raw: &str) -> Vec<ParsedFile> {
    let mut files: Vec<ParsedFile> = Vec::new();
    let mut cur: Option<ParsedFile> = None;
    let mut old_ln = 0usize;
    let mut new_ln = 0usize;
    let mut in_hunk = false;

    for line in raw.lines() {
        if line.starts_with("diff --git") {
            files.extend(cur.take());
            cur = Some(ParsedFile {
                title: title_from_diff_git(line),
                ..ParsedFile::default()
            });
            in_hunk = false;
            continue;
        }

        if is_file_meta(line) {
            in_hunk = false;
            if let Some(rest) = line.strip_prefix("rename from ") {
                cur.get_or_insert_with(ParsedFile::default).rename_from =
                    Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                cur.get_or_insert_with(ParsedFile::default).title = rest.trim().to_string();
            } else if let Some(rest) = line
                .strip_prefix("+++ ")
                .or_else(|| line.strip_prefix("--- "))
                && let Some(path) = clean_path(rest)
            {
                // Use the +++/--- paths to refine the title.
                let file = cur.get_or_insert_with(ParsedFile::default);
                if line.starts_with("+++ ") || file.title.is_empty() {
                    file.title = path;
                }
            }
            continue;
        }

        if line.starts_with("@@") {
            let file = cur.get_or_insert_with(ParsedFile::default);
            (old_ln, new_ln) = parse_hunk_header(line);
            in_hunk = true;
            file.rows.push(ParsedRow::Hunk(line.to_string()));
            continue;
        }

        if !in_hunk {
            // Stray content (e.g. non-diff input): show it verbatim.
            let file = cur.get_or_insert_with(ParsedFile::default);
            file.rows.push(ParsedRow::Verbatim(line.to_string()));
            continue;
        }

        let file = cur.get_or_insert_with(ParsedFile::default);
        match line.chars().next() {
            Some('+') => {
                file.rows.push(ParsedRow::Content {
                    kind: SideKind::Added,
                    old: None,
                    new: Some(new_ln),
                    text: line[1..].to_string(),
                });
                new_ln += 1;
            }
            Some('-') => {
                file.rows.push(ParsedRow::Content {
                    kind: SideKind::Removed,
                    old: Some(old_ln),
                    new: None,
                    text: line[1..].to_string(),
                });
                old_ln += 1;
            }
            Some('\\') => {} // "\ No newline at end of file"
            _ => {
                // Context line (leading space, or an empty line).
                file.rows.push(ParsedRow::Content {
                    kind: SideKind::Context,
                    old: Some(old_ln),
                    new: Some(new_ln),
                    text: line.strip_prefix(' ').unwrap_or(line).to_string(),
                });
                old_ln += 1;
                new_ln += 1;
            }
        }
    }

    files.extend(cur);
    files
}

/// Turn parsed files into renderable [`FileDiff`]s: apply syntax highlighting,
/// pair removed/added lines side by side, and compute intra-line emphasis.
fn build_files(parsed: Vec<ParsedFile>) -> Vec<FileDiff> {
    let syntaxes = SyntaxSet::load_defaults_newlines();
    let mut themes = ThemeSet::load_defaults();
    let theme = themes
        .themes
        .remove("base16-ocean.dark")
        .or_else(|| themes.themes.values().next().cloned())
        .expect("syntect ships at least one default theme");

    parsed
        .into_iter()
        .map(|file| build_file(file, &syntaxes, &theme))
        .collect()
}

/// Build one file's renderable rows from its parsed form.
fn build_file(file: ParsedFile, syntaxes: &SyntaxSet, theme: &Theme) -> FileDiff {
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
                    rows.push(blank_row(hunk_style()));
                }
                rows.push(Row::Full(format!(" {text}"), hunk_style()));
                rows.push(blank_row(hunk_style()));
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
            spans.push(Span::styled(
                "│".to_string(),
                Style::default().fg(Color::DarkGray),
            ));
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

    // `bg` paints the whole cell; `emph_bg` highlights chars that changed
    // relative to the paired line on the other side.
    let (bg, emph_bg) = match s.kind {
        SideKind::Removed => (Some(Color::Rgb(60, 30, 30)), Some(Color::Rgb(110, 45, 45))),
        SideKind::Added => (Some(Color::Rgb(25, 50, 30)), Some(Color::Rgb(35, 95, 50))),
        SideKind::Context => (None, None),
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
    let emph = emph_bg.map_or(base, |b| base.bg(b));

    let gutter = format!("{:>NUM_WIDTH$} {marker}", s.num);
    let mut spans = vec![Span::styled(gutter, base.fg(gutter_fg))];

    // Walk segs and the change mask together, breaking each syntax run wherever
    // emphasis toggles so changed character spans get the brighter background.
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
        let mut run = String::new();
        let mut run_emph = false;
        for ch in seg.text.chars() {
            let e = s.emph.get(ci).copied().unwrap_or(false);
            if !run.is_empty() && e != run_emph {
                let style = if run_emph { lit } else { plain };
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            run.push(ch);
            run_emph = e;
            ci += 1;
        }
        if !run.is_empty() {
            let style = if run_emph { lit } else { plain };
            spans.push(Span::styled(run, style));
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

fn expand_tabs(s: &str) -> String {
    s.replace('\t', &" ".repeat(TAB_WIDTH))
}

/// Strip the leading `a/` or `b/` from a diff path, or `None` for `/dev/null`.
fn clean_path(raw: &str) -> Option<String> {
    let path = raw.trim();
    let path = path
        .strip_prefix("b/")
        .or_else(|| path.strip_prefix("a/"))
        .unwrap_or(path);
    (path != "/dev/null").then(|| path.to_string())
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
        "dockerfile" => return '\u{f308}',    // nf-linux-docker
        "makefile" => return '\u{e673}',      // nf-seti-makefile
        "license" => return '\u{f0fc7}',      // nf-md-certificate
        ".gitignore" | ".gitattributes" => return '\u{e702}', // nf-dev-git
        _ => {}
    }

    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => '\u{e7a8}',                          // nf-dev-rust
        "py" => '\u{e606}',                          // nf-seti-python
        "js" | "mjs" | "cjs" => '\u{e74e}',          // nf-dev-javascript
        "ts" => '\u{e628}',                          // nf-seti-typescript
        "jsx" | "tsx" => '\u{e7ba}',                 // nf-dev-react
        "html" | "htm" => '\u{e736}',                // nf-dev-html5
        "css" => '\u{e749}',                         // nf-dev-css3
        "scss" | "sass" => '\u{e603}',               // nf-seti-sass
        "json" => '\u{e60b}',                        // nf-seti-json
        "md" | "markdown" => '\u{e73e}',             // nf-dev-markdown
        "c" | "h" => '\u{e61e}',                     // nf-custom-c
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => '\u{e61d}', // nf-custom-cpp
        "go" => '\u{e627}',                          // nf-seti-go
        "java" => '\u{e738}',                        // nf-dev-java
        "scala" | "sc" => '\u{e737}',                // nf-dev-scala
        "rb" => '\u{e739}',                          // nf-dev-ruby
        "sh" | "bash" | "zsh" | "fish" => '\u{e795}', // nf-seti-shell
        "toml" => '\u{e6b2}',                        // nf-seti-config-ish
        "yaml" | "yml" => '\u{e615}',                // nf-seti-yml
        "lock" => '\u{f023}',                        // nf-fa-lock
        "txt" => '\u{f15c}',                         // nf-fa-file_text
        _ => '\u{f15b}',                             // nf-fa-file (generic)
    }
}

/// Pull the new-file path out of a `diff --git a/<path> b/<path>` line.
fn title_from_diff_git(line: &str) -> String {
    line.split_whitespace()
        .last()
        .map(|t| t.strip_prefix("b/").unwrap_or(t).to_string())
        .unwrap_or_default()
}

/// Whether a line is file-level metadata (not part of a hunk body).
fn is_file_meta(line: &str) -> bool {
    const PREFIXES: [&str; 9] = [
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
    // Only the range section between the `@@` markers holds the numbers; the
    // trailing function context can contain stray `+`/`-` tokens (e.g. Rust's
    // `->`) that must not be mistaken for line numbers.
    let range = line.split("@@").nth(1).unwrap_or(line);
    for token in range.split_whitespace() {
        if let Some(rest) = token.strip_prefix('-') {
            if let Some(n) = rest.split(',').next().and_then(|n| n.parse().ok()) {
                old = n;
            }
        } else if let Some(rest) = token.strip_prefix('+') {
            if let Some(n) = rest.split(',').next().and_then(|n| n.parse().ok()) {
                new = n;
            }
        }
    }
    (old, new)
}

fn hunk_style() -> Style {
    // #253143
    Style::default()
        .fg(Color::Rgb(145, 152, 161))
        .bg(Color::Rgb(37, 49, 67))
}

/// A blank full-width padding row in the given style.
fn blank_row(style: Style) -> Row {
    Row::Full(String::new(), style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_header_ignores_arrow_in_context() {
        // Rust's `->` in the function-context suffix must not be mistaken for a
        // line-number token. (Regression test.)
        let (old, new) = parse_hunk_header("@@ -300,7 +300,8 @@ fn bar() -> Result<()>");
        assert_eq!((old, new), (300, 300));
    }

    #[test]
    fn hunk_header_without_counts() {
        let (old, new) = parse_hunk_header("@@ -42 +57 @@");
        assert_eq!((old, new), (42, 57));
    }

    /// Pull out only the `Content` rows for terser assertions.
    fn content(file: &ParsedFile) -> Vec<(SideKind, Option<usize>, Option<usize>, &str)> {
        file.rows
            .iter()
            .filter_map(|r| match r {
                ParsedRow::Content {
                    kind,
                    old,
                    new,
                    text,
                } => Some((*kind, *old, *new, text.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn assigns_line_numbers_from_hunk_start() {
        let raw = "\
diff --git a/foo.rs b/foo.rs
--- a/foo.rs
+++ b/foo.rs
@@ -300,3 +300,3 @@ fn bar() -> Result<()>
 ctx
-old line
+new line
";
        let files = parse(raw);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].title, "foo.rs");
        assert_eq!(
            content(&files[0]),
            vec![
                (SideKind::Context, Some(300), Some(300), "ctx"),
                (SideKind::Removed, Some(301), None, "old line"),
                (SideKind::Added, None, Some(301), "new line"),
            ],
        );
    }

    #[test]
    fn title_falls_back_to_old_path_for_deletions() {
        let raw = "\
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let files = parse(raw);
        assert_eq!(files[0].title, "gone.rs");
    }

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
        assert!(build_files(parsed)[0].title.ends_with("old/name.rs → new/name.rs"));
    }

    #[test]
    fn file_icon_by_extension_and_name() {
        assert_eq!(file_icon("src/main.rs"), '\u{e7a8}');
        assert_eq!(file_icon("Dockerfile"), '\u{f308}');
        assert_eq!(file_icon("weird.unknownext"), '\u{f15b}');
    }

    #[test]
    fn non_diff_input_is_kept_verbatim() {
        let files = parse("just some text\nnot a diff\n");
        assert_eq!(files.len(), 1);
        assert!(
            files[0]
                .rows
                .iter()
                .all(|r| matches!(r, ParsedRow::Verbatim(_)))
        );
    }
}
