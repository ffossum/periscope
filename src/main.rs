mod app;
mod cli;
mod diff;
mod github;
mod ui;

use app::App;
use clap::Parser;
use cli::{Cli, Command};
use diff::DiffViewer;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Diff { file }) => run_diff(file).await,
        None => run_app().await,
    }
}

async fn run_diff(file: Option<std::path::PathBuf>) -> color_eyre::Result<()> {
    let raw = diff::read_input(file)?;
    // Nothing to show (e.g. `git diff` with no changes): exit without a TUI.
    if raw.trim().is_empty() {
        return Ok(());
    }
    // crossterm reads key events from /dev/tty directly, so a piped-in diff on
    // stdin (`git diff | periscope diff`) doesn't interfere with the event loop.
    let mut viewer = DiffViewer::new(&raw);
    let mut terminal = ratatui::init();
    let result = viewer.run(&mut terminal).await;
    ratatui::restore();

    result
}

async fn run_app() -> color_eyre::Result<()> {
    let mut app = App::new().await?;
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal).await;
    ratatui::restore();

    result
}
