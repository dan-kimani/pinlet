//! Binary entry point: parse the CLI, then start the GTK application.
//!
//! GtkApplication registers a DBus name, which makes Pinlet
//! single-instance: a second `pinlet …` invocation forwards its
//! command line to the running instance (the `command-line` signal)
//! and exits, so quick capture always lands in the live app.

use std::cell::RefCell;
use std::rc::Rc;

use clap::Parser;
use gtk4::gio;
use gtk4::prelude::*;

use pinlet::{app, cli, ui};

fn main() -> glib::ExitCode {
    let cli = cli::Cli::parse();

    if matches!(&cli.command, Some(cli::Command::Where)) {
        println!("{}", app::data_dir().display());
        return glib::ExitCode::SUCCESS;
    }

    let gtk_app = gtk4::Application::builder()
        .application_id("org.pinlet.Pinlet")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    // The app core is created on the first command-line delivery;
    // later deliveries hand off to it.
    let app_slot: Rc<RefCell<Option<app::App>>> = Rc::new(RefCell::new(None));

    gtk_app.connect_command_line(move |gtk_app, command_line| {
        let args: Vec<String> = command_line
            .arguments()
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        // GApplication always delivers the program name as argv[0];
        // `try_parse_from` expects its own argv[0], so drop whatever
        // arrived and prepend a placeholder, keeping the rest verbatim.
        let mut remote_args = args.into_iter();
        remote_args.next();
        // A bad remote invocation exits with its proper code instead
        // of silently opening all notes — the running app stays up
        // either way. The message itself cannot be forwarded: this
        // GTK stack predates the `print_literal` invocation API, and
        // `err.print()` would land in the primary's stderr rather
        // than the caller's terminal.
        let remote_cli =
            match cli::Cli::try_parse_from(std::iter::once("pinlet".to_owned()).chain(remote_args))
            {
                Ok(remote_cli) => remote_cli,
                Err(err) => {
                    return err.exit_code();
                }
            };

        // Clone the handle out of the slot first — the slot borrow
        // must end before the None arm stores into it.
        let existing = app_slot.borrow().clone();
        match existing {
            Some(app) => app.handle_command_line(&remote_cli),
            None => match app::App::new(gtk_app) {
                Ok(app) => {
                    ui::ensure_styles();
                    app.activate(&remote_cli);
                    *app_slot.borrow_mut() = Some(app);
                }
                Err(err) => {
                    eprintln!("failed to start Pinlet: {err}");
                    gtk_app.quit();
                }
            },
        }
        0
    });

    gtk_app.run()
}
