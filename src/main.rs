//! Binary entry point: parse the CLI, then start the GTK application.

use clap::Parser;
use gtk4::prelude::*;

use pinlet::{app, cli, ui};

fn main() -> glib::ExitCode {
    let cli = cli::Cli::parse();

    if matches!(&cli.command, Some(cli::Command::Where)) {
        match app::data_dir() {
            Ok(dir) => println!("{}", dir.display()),
            Err(err) => eprintln!("failed to resolve data directory: {err}"),
        }
        return glib::ExitCode::SUCCESS;
    }

    let gtk_app = gtk4::Application::builder()
        .application_id("org.pinlet.Pinlet")
        .build();

    gtk_app.connect_activate(move |gtk_app| match app::App::new(gtk_app) {
        Ok(app) => {
            ui::ensure_styles();
            app.activate(&cli);
        }
        Err(err) => {
            eprintln!("failed to start Pinlet: {err}");
            gtk_app.quit();
        }
    });

    gtk_app.run()
}
