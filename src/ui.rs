use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;

use crate::app::App;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let block = Block::default()
        .title(" Periscope ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    if app.prs.is_empty() {
        let items = vec![ListItem::new("No open PRs assigned to you.")];
        let list = List::new(items).block(block);
        frame.render_widget(list, area);
        return;
    }

    let items: Vec<ListItem> = app
        .prs
        .iter()
        .map(|pr| {
            let line = Line::raw(format!(
                "{}#{} {} ({})",
                pr.repository.name_with_owner, pr.number, pr.title, pr.author.login,
            ));
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut app.list_state);
}
