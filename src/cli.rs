use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "periscope", about = "A TUI for reviewing GitHub pull requests")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Render a git diff in a paging view.
    ///
    /// Reads the diff from the given file, or from stdin if no file is given
    /// (e.g. `git diff | periscope diff`).
    Diff {
        /// Path to a diff file. Reads stdin when omitted.
        file: Option<PathBuf>,
    },
}
