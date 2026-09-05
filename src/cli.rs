//! Command-line interface: quick capture and diagnostics.

use clap::{Parser, Subcommand};

/// Pinlet — native sticky notes for Linux desktops.
#[derive(Debug, Parser)]
#[command(name = "pinlet", version, about)]
pub struct Cli {
    /// Optional command; plain `pinlet` opens all notes.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Available subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new note and open its window.
    New {
        /// Initial note text.
        #[arg(short, long)]
        text: Option<String>,
        /// Initial note color (Yellow, Green, Blue, Pink, Purple, Charcoal).
        #[arg(short, long)]
        color: Option<String>,
    },
    /// Print the note repository path (the git repo) and exit.
    Where,
}
