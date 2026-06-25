use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::widgets::ListState;
use ratatui::{DefaultTerminal, Frame};

use crate::github::{self, PullRequest};
use crate::ui;

pub struct App {
    pub running: bool,
    pub prs: Vec<PullRequest>,
    pub list_state: ListState,
}

impl App {
    pub async fn new() -> color_eyre::Result<Self> {
        let prs = github::fetch_prs().await?;
        let mut list_state = ListState::default();
        if !prs.is_empty() {
            list_state.select(Some(0));
        }
        Ok(Self {
            running: true,
            prs,
            list_state,
        })
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
        ui::draw(frame, self);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (_, KeyCode::Char('q')) | (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.running = false;
            }
            (_, KeyCode::Char('j') | KeyCode::Down) => self.select_next(),
            (_, KeyCode::Char('k') | KeyCode::Up) => self.select_prev(),
            _ => {}
        }
    }

    fn select_next(&mut self) {
        if self.prs.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) if i >= self.prs.len() - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        if self.prs.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(0) | None => self.prs.len() - 1,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(i));
    }
}
