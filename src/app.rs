use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::{DefaultTerminal, Frame};

use crate::ui;

pub struct App {
    pub running: bool,
}

impl App {
    pub fn new() -> Self {
        Self { running: true }
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

    fn draw(&self, frame: &mut Frame) {
        ui::draw(frame, self);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (_, KeyCode::Char('q')) | (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.running = false;
            }
            _ => {}
        }
    }
}
