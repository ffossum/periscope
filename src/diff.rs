use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};

/// Read the diff from `file`, or from stdin when no file is given.
pub fn read_input(file: Option<PathBuf>) -> color_eyre::Result<String> {
    match file {
        Some(path) => Ok(std::fs::read_to_string(path)?),
        None => Ok(std::io::read_to_string(std::io::stdin())?),
    }
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

/// One side (left or right) of a paired diff row.
struct SideLine {
    num: usize,
    text: String,
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

            if let Some(Ok(Event::Key(key))) = reader.next().await {
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key);
                }
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
        let lines: Vec<Line> = self.rows.iter().map(|row| render_row(row, inner_width)).collect();

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

/// Parse a unified diff into side-by-side rows.
fn parse(raw: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    // Buffered runs of removed/added lines, paired when the run ends.
    let mut removed: Vec<SideLine> = Vec::new();
    let mut added: Vec<SideLine> = Vec::new();
    let mut old_ln = 0usize;
    let mut new_ln = 0usize;
    let mut in_hunk = false;

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
            rows.push(Row::Full(line.to_string(), hunk_style()));
            continue;
        }

        if is_file_meta(line) {
            flush(&mut rows, &mut removed, &mut added);
            in_hunk = false;
            rows.push(Row::Full(line.to_string(), header_style()));
            continue;
        }

        if !in_hunk {
            rows.push(Row::Full(line.to_string(), Style::default()));
            continue;
        }

        match line.chars().next() {
            Some('+') => {
                added.push(SideLine {
                    num: new_ln,
                    text: expand_tabs(&line[1..]),
                    kind: SideKind::Added,
                });
                new_ln += 1;
            }
            Some('-') => {
                removed.push(SideLine {
                    num: old_ln,
                    text: expand_tabs(&line[1..]),
                    kind: SideKind::Removed,
                });
                old_ln += 1;
            }
            Some('\\') => {} // "\ No newline at end of file"
            _ => {
                // Context line (leading space, or an empty line).
                flush(&mut rows, &mut removed, &mut added);
                let text = expand_tabs(line.strip_prefix(' ').unwrap_or(line));
                rows.push(Row::Pair {
                    left: Some(SideLine {
                        num: old_ln,
                        text: text.clone(),
                        kind: SideKind::Context,
                    }),
                    right: Some(SideLine {
                        num: new_ln,
                        text,
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

/// Render a row to a single full-width `Line`, split into two columns.
fn render_row(row: &Row, width: usize) -> Line<'static> {
    match row {
        Row::Full(text, style) => Line::from(Span::styled(fit(text, width), *style)),
        Row::Pair { left, right } => {
            // One column for the center separator, the rest split evenly.
            let avail = width.saturating_sub(1);
            let left_w = avail / 2;
            let right_w = avail - left_w;

            let (lt, ls) = side_cell(left.as_ref());
            let (rt, rs) = side_cell(right.as_ref());

            Line::from(vec![
                Span::styled(fit(&lt, left_w), ls),
                Span::styled("│".to_string(), Style::default().fg(Color::DarkGray)),
                Span::styled(fit(&rt, right_w), rs),
            ])
        }
    }
}

/// Build the raw text and style for one side of a pair.
fn side_cell(side: Option<&SideLine>) -> (String, Style) {
    match side {
        None => (String::new(), Style::default()),
        Some(s) => {
            let marker = match s.kind {
                SideKind::Removed => '-',
                SideKind::Added => '+',
                SideKind::Context => ' ',
            };
            let text = format!("{:>NUM_WIDTH$} {}{}", s.num, marker, s.text);
            let style = match s.kind {
                SideKind::Removed => Style::default().fg(Color::Red),
                SideKind::Added => Style::default().fg(Color::Green),
                SideKind::Context => Style::default(),
            };
            (text, style)
        }
    }
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
            old = rest.split(',').next().and_then(|n| n.parse().ok()).unwrap_or(1);
        } else if let Some(rest) = token.strip_prefix('+') {
            new = rest.split(',').next().and_then(|n| n.parse().ok()).unwrap_or(1);
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
