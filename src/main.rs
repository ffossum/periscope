mod app;
mod ui;

use app::App;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let mut terminal = ratatui::init();
    let result = App::new().run(&mut terminal).await;
    ratatui::restore();

    result
}
