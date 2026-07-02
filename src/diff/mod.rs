//! Side-by-side diff viewer: a scrollable TUI over a parsed unified diff.
//!
//! The pipeline is [`parse`](parse::parse) (text → data) →
//! [`build_files`](build::build_files) (highlight + pair into rows) →
//! [`render_row`](render::render_row) (rows → styled lines). [`DiffViewer`]
//! owns the runtime state (scroll, search) and drives the event loop.

mod build;
mod palette;
mod parse;
mod render;
mod search;

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use regex::Regex;

use build::{FileDiff, Row, build_files, max_line_width, side_text};
use palette::Palette;
use parse::parse;
use render::{RowHls, render_row};
use search::{MatchSide, Search, SearchMatch, build_regex, find_ranges};

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
pub(super) const NUM_WIDTH: usize = 4;
/// Tabs are expanded to this many spaces so columns stay aligned.
pub(super) const TAB_WIDTH: usize = 4;
/// Blank rows between adjacent file blocks.
const FILE_GAP: u16 = 1;
/// Rows scrolled per mouse-wheel notch.
const MOUSE_SCROLL_LINES: i32 = 3;
/// Columns scrolled per horizontal-scroll keypress.
const HSCROLL_COLS: i32 = 4;
/// Gutter width per cell: a right-aligned line number, a space, and the marker.
const GUTTER_WIDTH: usize = NUM_WIDTH + 2;

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
    fn row_highlights(&self) -> HashMap<(usize, usize), RowHls> {
        let mut map: HashMap<(usize, usize), RowHls> = HashMap::new();
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
    /// Mirrors the split in [`render::render_row`], using the narrower left cell
    /// so the whole line stays reachable on both sides.
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
