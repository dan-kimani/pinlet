//! Command-line interface: quick capture and diagnostics.

use clap::{Parser, Subcommand};

/// Pinlet — native sticky notes for Linux desktops.
#[derive(Debug, Parser)]
#[command(name = "pinlet", version, about)]
pub struct Cli {
    /// Start in the background: only notes pinned to the desktop open
    /// windows; everything else stays in the tray. The login autostart
    /// entry passes this so sign-in doesn't flood the desktop with
    /// unpinned notes. A manual launch omits it and opens all notes.
    #[arg(long)]
    pub background: bool,
    /// Optional command; plain `pinlet` opens all notes.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Available subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new note and open its window.
    New {
        /// Initial note text (positional: `pinlet new "buy milk"`).
        #[arg(value_name = "TEXT")]
        text: Option<String>,
        /// Initial note color (Yellow, Green, Blue, Pink, Purple, Charcoal).
        #[arg(short, long)]
        color: Option<String>,
    },
    /// Print the note repository path (the git repo) and exit.
    Where,
    /// Sync notes with the configured git remote (pull → commit → push).
    Sync,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_flag_defaults_to_off() {
        let cli = Cli::try_parse_from(["pinlet"]).unwrap();
        assert!(!cli.background);
        assert!(cli.command.is_none());
    }

    #[test]
    fn background_flag_parses_alone_and_with_subcommands() {
        let cli = Cli::try_parse_from(["pinlet", "--background"]).unwrap();
        assert!(cli.background);

        let cli = Cli::try_parse_from(["pinlet", "--background", "sync"]).unwrap();
        assert!(cli.background);
        assert!(matches!(cli.command, Some(Command::Sync)));

        let cli = Cli::try_parse_from(["pinlet", "--background", "new", "buy milk"]).unwrap();
        assert!(cli.background);
        assert!(matches!(cli.command, Some(Command::New { .. })));
    }
}
