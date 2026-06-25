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

pub struct DiffViewer {
    running: bool,
    lines: Vec<Line<'static>>,
    scroll: u16,
    viewport_height: u16,
}

impl DiffViewer {
    pub fn new(raw: &str) -> Self {
        let lines = raw.lines().map(style_line).collect();
        Self {
            running: true,
            lines,
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

        let total = self.lines.len();
        let current = (self.scroll as usize + 1).min(total.max(1));
        let title = format!(" diff — {current}/{total} ");

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let paragraph = Paragraph::new(self.lines.clone())
            .block(block)
            .scroll((self.scroll, 0));

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
        let total = self.lines.len() as u16;
        total.saturating_sub(self.viewport_height)
    }
}

/// Classify a diff line by its prefix and style it accordingly.
fn style_line(line: &str) -> Line<'static> {
    let style = if line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
    {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };

    Line::from(Span::styled(line.to_string(), style))
}
