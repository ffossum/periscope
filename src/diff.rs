use std::path::{Path, PathBuf};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use regex::{Regex, RegexBuilder};
use similar::{ChangeTag, TextDiff};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SynColor, Theme, ThemeSet};
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
/// Columns scrolled per horizontal-scroll keypress.
const HSCROLL_COLS: i32 = 4;
/// Gutter width per cell: a right-aligned line number, a space, and the marker.
const GUTTER_WIDTH: usize = NUM_WIDTH + 2;

#[derive(Clone, Copy, PartialEq, Debug)]
enum SideKind {
    Context,
    Removed,
    Added,
}

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

/// UI colors derived from the active syntect theme's settings.
#[derive(Clone, Copy)]
struct Palette {
    fg: Color,
    bg: Color,
    darker_bg: Color,
    /// Block borders.
    border: Color,
    /// The center separator between the two columns.
    separator: Color,
    /// Line-number gutter for context lines.
    gutter: Color,
    /// Hunk header foreground and background band.
    hunk_fg: Color,
    hunk_bg: Color,
    /// Removed-side cell background, brighter intra-line emphasis, and gutter.
    removed_bg: Color,
    removed_emph_bg: Color,
    removed_gutter: Color,
    /// Added-side cell background, brighter intra-line emphasis, and gutter.
    added_bg: Color,
    added_emph_bg: Color,
    added_gutter: Color,
    /// Search-match foreground (high contrast), plus the background for other
    /// matches and the currently-selected one.
    search_fg: Color,
    search_bg: Color,
    search_current_bg: Color,
}

impl Palette {
    fn from_theme(theme: &Theme) -> Self {
        let s = &theme.settings;
        let bg = s.background.map(rgb).unwrap_or((0, 0, 0));
        let fg = s.foreground.map(rgb).unwrap_or((220, 220, 220));
        let gutter = s
            .gutter_foreground
            .map(conv)
            .unwrap_or_else(|| mix(bg, fg, 0.5));

        // Themes don't define diff add/remove colors, so tint the theme
        // background toward red/green. Blending off the background keeps the
        // tints in step with light vs dark themes.
        const RED: (u8, u8, u8) = (220, 80, 80);
        const GREEN: (u8, u8, u8) = (90, 190, 110);
        const BLUE: (u8, u8, u8) = (110, 130, 250);
        // Search hits use a fixed yellow/orange standout, dark text on top.
        const YELLOW: (u8, u8, u8) = (224, 198, 92);
        const ORANGE: (u8, u8, u8) = (240, 150, 70);

        Self {
            fg: rgb_color(fg),
            bg: rgb_color(bg),
            darker_bg: mix(bg, (0, 0, 0), 0.1),
            border: gutter,
            separator: gutter,
            gutter,
            hunk_fg: rgb_color(fg),
            // A subtle band, lighter than the background and tinted blue.
            hunk_bg: mix(bg, BLUE, 0.07),
            removed_bg: mix(bg, RED, 0.14),
            removed_emph_bg: mix(bg, RED, 0.30),
            removed_gutter: rgb_color(RED),
            added_bg: mix(bg, GREEN, 0.12),
            added_emph_bg: mix(bg, GREEN, 0.30),
            added_gutter: rgb_color(GREEN),
            search_fg: rgb_color(bg),
            search_bg: rgb_color(YELLOW),
            search_current_bg: rgb_color(ORANGE),
        }
    }
}

/// A syntect color as an `(r, g, b)` tuple.
fn rgb(c: SynColor) -> (u8, u8, u8) {
    (c.r, c.g, c.b)
}

/// A syntect color as a ratatui [`Color`].
fn conv(c: SynColor) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// An `(r, g, b)` tuple as a ratatui [`Color`].
fn rgb_color((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// Linear blend of two colors; `t` is the weight given to `b`.
fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let lerp = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Color::Rgb(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// Which part of a row a search hit lives in.
#[derive(Clone, Copy, PartialEq, Debug)]
enum MatchSide {
    Left,
    Right,
    Full,
}

/// A single regex hit, located precisely enough to scroll to and highlight.
#[derive(Clone, Copy)]
struct SearchMatch {
    file: usize,
    row: usize,
    side: MatchSide,
    /// Char range within the cell/row content (gutter excluded).
    start: usize,
    end: usize,
    /// Absolute row in the stacked layout, for vertical scrolling.
    vrow: u16,
}

/// An executed search: the pattern, all its hits, and the focused one.
struct Search {
    pattern: String,
    matches: Vec<SearchMatch>,
    current: usize,
}

/// Char ranges to highlight within one row, split by where they fall.
#[derive(Default)]
struct RowHls {
    left: Vec<(usize, usize)>,
    right: Vec<(usize, usize)>,
    full: Vec<(usize, usize)>,
}

pub struct DiffViewer {
    running: bool,
    files: Vec<FileDiff>,
    palette: Palette,
    scroll: u16,
    /// Horizontal scroll offset in content columns, shared by both halves.
    hscroll: u16,
    /// Longest content line across every cell, used to clamp `hscroll`.
    max_line_width: u16,
    viewport_height: u16,
    viewport_width: u16,
    /// Some while typing a search pattern at the bottom prompt (`/`).
    input: Option<String>,
    /// The most recent executed search, kept for `n`/`N` and highlighting.
    search: Option<Search>,
    /// A transient one-line message (errors, "not found") shown at the bottom.
    status: Option<String>,
}

impl DiffViewer {
    pub fn new(raw: &str) -> Self {
        let (files, palette) = build_files(parse(raw));
        let max_line_width = max_line_width(&files);
        Self {
            running: true,
            files,
            palette,
            scroll: 0,
            hscroll: 0,
            max_line_width,
            viewport_height: 0,
            viewport_width: 0,
            input: None,
            search: None,
            status: None,
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
        let full = frame.area();

        // Reserve the bottom row for the search prompt or a status message.
        let show_status = self.input.is_some() || self.status.is_some();
        let area = if show_status && full.height > 0 {
            Rect::new(full.x, full.y, full.width, full.height - 1)
        } else {
            full
        };

        self.viewport_height = area.height;
        self.viewport_width = area.width;
        self.clamp_scroll();

        if show_status {
            self.draw_status(frame, full);
        }

        if area.width < 3 || area.height == 0 {
            return;
        }

        // The focused match, and a per-row lookup of all hits to highlight.
        let current = self.search.as_ref().and_then(|s| s.matches.get(s.current));
        let row_hls = self.row_highlights();

        let scroll = self.scroll as i32;
        let viewport = area.height as i32;
        let border_style = Style::default().fg(self.palette.border).bg(self.palette.bg);

        // Virtual top of the current file in the full stacked layout.
        let mut top = 0i32;
        for (fi, file) in self.files.iter().enumerate() {
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

            let mut block = Block::default()
                .borders(borders)
                .border_style(border_style)
                .bg(self.palette.darker_bg);
            if borders.contains(Borders::TOP) {
                block = block.title(Line::from(Span::styled(
                    format!(" {} ", file.title),
                    Style::default().fg(self.palette.fg).bg(self.palette.bg),
                )));
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
                .enumerate()
                .map(|(off, row)| {
                    let ri = first as usize + off;
                    let hl = row_hls.get(&(fi, ri));
                    // The focused match's range, if it falls on this row.
                    let cur = current
                        .filter(|m| m.file == fi && m.row == ri)
                        .map(|m| (m.side, m.start, m.end));
                    render_row(row, inner_w, self.hscroll as usize, hl, cur, &self.palette)
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }

    /// Render the bottom prompt: `/pattern` while typing, else the status text.
    fn draw_status(&self, frame: &mut Frame, full: Rect) {
        if full.height == 0 || full.width == 0 {
            return;
        }
        let row = Rect::new(full.x, full.y + full.height - 1, full.width, 1);
        let style = Style::default().fg(self.palette.fg).bg(self.palette.bg);
        if let Some(buf) = &self.input {
            let text = format!("/{buf}");
            let cursor_x = full.x + 1 + buf.chars().count() as u16;
            frame.render_widget(Paragraph::new(Line::styled(text, style)), row);
            frame.set_cursor_position((cursor_x.min(full.x + full.width - 1), row.y));
        } else if let Some(msg) = &self.status {
            frame.render_widget(Paragraph::new(Line::styled(msg.clone(), style)), row);
        }
    }

    /// Group every current-search hit by `(file, row)` for fast lookup while
    /// rendering. Returns an empty map when no search is active.
    fn row_highlights(&self) -> std::collections::HashMap<(usize, usize), RowHls> {
        let mut map: std::collections::HashMap<(usize, usize), RowHls> =
            std::collections::HashMap::new();
        let Some(search) = &self.search else {
            return map;
        };
        for m in &search.matches {
            let e = map.entry((m.file, m.row)).or_default();
            let dst = match m.side {
                MatchSide::Left => &mut e.left,
                MatchSide::Right => &mut e.right,
                MatchSide::Full => &mut e.full,
            };
            dst.push((m.start, m.end));
        }
        map
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // While typing a pattern, all keys belong to the prompt.
        if self.input.is_some() {
            self.handle_search_input(key);
            return;
        }

        // Any normal-mode key clears a lingering status message.
        self.status = None;

        let page = self.viewport_height.max(1);
        match (key.modifiers, key.code) {
            (_, KeyCode::Char('q')) | (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.running = false
            }
            // Esc clears the active search and its highlights, but never quits.
            (_, KeyCode::Esc) => self.search = None,
            (_, KeyCode::Char('j') | KeyCode::Down) => self.scroll_by(1),
            (_, KeyCode::Char('k') | KeyCode::Up) => self.scroll_by(-1),
            (_, KeyCode::Char('d') | KeyCode::PageDown) => self.scroll_by(page as i32),
            (_, KeyCode::Char('u') | KeyCode::PageUp) => self.scroll_by(-(page as i32)),
            (_, KeyCode::Char('h') | KeyCode::Left) => self.hscroll_by(-HSCROLL_COLS),
            (_, KeyCode::Char('l') | KeyCode::Right) => self.hscroll_by(HSCROLL_COLS),
            (_, KeyCode::Char('g') | KeyCode::Home) => self.scroll = 0,
            (_, KeyCode::Char('G') | KeyCode::End) => self.scroll = self.max_scroll(),
            (_, KeyCode::Char('0')) => self.hscroll = 0,
            (_, KeyCode::Char('/')) => self.input = Some(String::new()),
            (_, KeyCode::Char('n')) => self.step_match(1),
            (_, KeyCode::Char('N')) => self.step_match(-1),
            _ => {}
        }
    }

    /// Edit the bottom search prompt: Enter runs it, Esc cancels, Backspace
    /// deletes, printable keys append.
    fn handle_search_input(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.running = false,
            (_, KeyCode::Esc) => self.input = None,
            (_, KeyCode::Enter) => {
                if let Some(pattern) = self.input.take() {
                    self.execute_search(pattern);
                }
            }
            (_, KeyCode::Backspace) => {
                if let Some(buf) = self.input.as_mut() {
                    buf.pop();
                }
            }
            (_, KeyCode::Char(c)) => {
                if let Some(buf) = self.input.as_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
    }

    /// Compile and run `pattern`, focusing the first hit at or after the current
    /// scroll position. Records errors / no-match to the status line.
    fn execute_search(&mut self, pattern: String) {
        if pattern.is_empty() {
            self.search = None;
            return;
        }
        let re = match build_regex(&pattern) {
            Ok(re) => re,
            Err(_) => {
                self.status = Some(format!("Invalid regex: {pattern}"));
                return;
            }
        };
        let matches = self.find_matches(&re);
        if matches.is_empty() {
            self.status = Some(format!("Pattern not found: {pattern}"));
            self.search = Some(Search {
                pattern,
                matches,
                current: 0,
            });
            return;
        }
        let current = matches
            .iter()
            .position(|m| m.vrow >= self.scroll)
            .unwrap_or(0);
        self.search = Some(Search {
            pattern,
            matches,
            current,
        });
        self.reveal_current();
    }

    /// Find every regex hit across all rows, in reading order (top to bottom,
    /// left cell before right).
    fn find_matches(&self, re: &Regex) -> Vec<SearchMatch> {
        let mut out = Vec::new();
        let mut top: u16 = 0;
        for (fi, file) in self.files.iter().enumerate() {
            for (ri, row) in file.rows.iter().enumerate() {
                let vrow = top + 1 + ri as u16; // +1 skips the block's top border
                let mut push = |side, ranges: Vec<(usize, usize)>| {
                    for (start, end) in ranges {
                        out.push(SearchMatch {
                            file: fi,
                            row: ri,
                            side,
                            start,
                            end,
                            vrow,
                        });
                    }
                };
                match row {
                    Row::Full(text, _) => push(MatchSide::Full, find_ranges(re, text)),
                    Row::Pair { left, right } => {
                        if let Some(l) = left {
                            push(MatchSide::Left, find_ranges(re, &side_text(l)));
                        }
                        if let Some(r) = right {
                            push(MatchSide::Right, find_ranges(re, &side_text(r)));
                        }
                    }
                }
            }
            top = top.saturating_add(file.height()).saturating_add(FILE_GAP);
        }
        out
    }

    /// Move to the next (`dir == 1`) or previous (`dir == -1`) match, wrapping.
    fn step_match(&mut self, dir: i32) {
        let Some(search) = self.search.as_mut() else {
            self.status = Some("No previous search".to_string());
            return;
        };
        let n = search.matches.len();
        if n == 0 {
            self.status = Some(format!("Pattern not found: {}", search.pattern));
            return;
        }
        let cur = search.current as i32;
        search.current = (cur + dir).rem_euclid(n as i32) as usize;
        self.reveal_current();
    }

    /// Scroll so the focused match is on screen, vertically and horizontally.
    fn reveal_current(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        let Some(m) = search.matches.get(search.current).copied() else {
            return;
        };

        // Vertical: a few rows of context above the hit.
        const MARGIN: u16 = 3;
        self.scroll = m.vrow.saturating_sub(MARGIN).min(self.max_scroll());

        // Horizontal: only paired cells scroll; reveal the hit if it's off-screen.
        if m.side != MatchSide::Full {
            let cols = self.content_cols();
            let h = self.hscroll as usize;
            if cols > 0 && (m.start < h || m.end > h + cols) {
                self.hscroll = (m.start.saturating_sub(2) as u16).min(self.max_hscroll());
            }
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Holding Shift turns the vertical wheel into horizontal scroll, the
        // usual convention for panning content wider than the viewport.
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            MouseEventKind::ScrollDown if shift => self.hscroll_by(MOUSE_SCROLL_LINES),
            MouseEventKind::ScrollUp if shift => self.hscroll_by(-MOUSE_SCROLL_LINES),
            MouseEventKind::ScrollDown => self.scroll_by(MOUSE_SCROLL_LINES),
            MouseEventKind::ScrollUp => self.scroll_by(-MOUSE_SCROLL_LINES),
            // Mice with a horizontal wheel (or terminals that map Shift+wheel
            // to these) scroll the columns directly.
            MouseEventKind::ScrollRight => self.hscroll_by(MOUSE_SCROLL_LINES),
            MouseEventKind::ScrollLeft => self.hscroll_by(-MOUSE_SCROLL_LINES),
            _ => {}
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).max(0) as u16;
        self.scroll = next.min(self.max_scroll());
    }

    fn hscroll_by(&mut self, delta: i32) {
        let next = (self.hscroll as i32 + delta).max(0) as u16;
        self.hscroll = next.min(self.max_hscroll());
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
        self.hscroll = self.hscroll.min(self.max_hscroll());
    }

    /// Content columns visible in a single cell, given the current viewport.
    ///
    /// Mirrors the split in [`render_row`], using the narrower left cell so the
    /// whole line stays reachable on both sides.
    fn content_cols(&self) -> usize {
        let inner_w = (self.viewport_width as usize).saturating_sub(2); // L/R borders
        let avail = inner_w.saturating_sub(1); // center separator
        let left_w = avail / 2;
        left_w.saturating_sub(GUTTER_WIDTH)
    }

    fn max_hscroll(&self) -> u16 {
        (self.max_line_width as usize).saturating_sub(self.content_cols()) as u16
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
fn build_files(parsed: Vec<ParsedFile>) -> (Vec<FileDiff>, Palette) {
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
fn max_line_width(files: &[FileDiff]) -> u16 {
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

/// Load the syntax-highlighting theme.
///
/// The Enki-Tokyo-Night `.tmTheme` is embedded at compile time so it loads
/// regardless of the working directory (periscope runs as git's `pager.diff`).
fn load_theme() -> Theme {
    const ENKI_TOKYO_NIGHT: &str = include_str!("../themes/Enki-Tokyo-Night.tmTheme");
    let mut cursor = std::io::Cursor::new(ENKI_TOKYO_NIGHT.as_bytes());
    ThemeSet::load_from_reader(&mut cursor).expect("bundled Enki-Tokyo-Night theme parses")
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
fn render_row(
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

fn expand_tabs(s: &str) -> String {
    s.replace('\t', &" ".repeat(TAB_WIDTH))
}

/// The full content text of one side of a pair, used as the search haystack.
fn side_text(s: &SideLine) -> String {
    s.segs.iter().map(|seg| seg.text.as_str()).collect()
}

/// Every non-empty match of `re` in `text`, as char (not byte) ranges so they
/// line up with the char-indexed render masks.
fn find_ranges(re: &Regex, text: &str) -> Vec<(usize, usize)> {
    re.find_iter(text)
        .filter(|m| m.start() < m.end())
        .map(|m| {
            let start = text[..m.start()].chars().count();
            let end = text[..m.end()].chars().count();
            (start, end)
        })
        .collect()
}

/// Compile a user pattern with smartcase: case-insensitive unless the pattern
/// itself contains an uppercase letter.
fn build_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let case_insensitive = !pattern.chars().any(|c| c.is_uppercase());
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
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
        } else if let Some(rest) = token.strip_prefix('+')
            && let Some(n) = rest.split(',').next().and_then(|n| n.parse().ok())
        {
            new = n;
        }
    }
    (old, new)
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
