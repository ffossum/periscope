use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;

pub fn draw(frame: &mut Frame, _app: &App) {
    let area = frame.area();

    let block = Block::default()
        .title(" Periscope ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let text = Text::raw("Press 'q' to quit.");
    let paragraph = Paragraph::new(text).block(block);

    frame.render_widget(paragraph, area);
}
