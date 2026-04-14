mod app;
mod github;
mod ui;

use app::App;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let mut app = App::new().await?;
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal).await;
    ratatui::restore();

    result
}
