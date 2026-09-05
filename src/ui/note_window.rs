//! The sticky note window: header actions and Markdown body.

use std::rc::Rc;

use adw::prelude::*;
use gtk4::{
    Button, FlowBox, HeaderBar, MenuButton, Orientation, Popover, ScrolledWindow, TextView,
};

use crate::app::SharedNote;
use crate::storage::NoteColor;
use crate::ui::colors;

/// Stylesheet shared by every note window.
pub const STYLE: &str = include_str!("style.css");

/// One window per note, styled like a sheet of paper.
pub struct NoteWindow {
    window: gtk4::ApplicationWindow,
    /// Kept alive so a custom hex color stays applied.
    _custom_css: Option<gtk4::CssProvider>,
}

impl NoteWindow {
    /// Build a note window and wire it to the app-core callbacks.
    ///
    /// * `on_changed` — the body text changed (triggers debounced save).
    /// * `on_delete` — the user confirmed deletion.
    /// * `on_new` — the user asked for another note.
    /// * `on_close` — the window is closing (flush pending state).
    pub fn new(
        app: &gtk4::Application,
        shared: Rc<SharedNote>,
        on_changed: impl Fn(String) + 'static,
        on_delete: impl Fn() + 'static,
        on_new: impl Fn() + 'static,
        on_close: impl Fn() + 'static,
    ) -> Self {
        let color = shared.note.borrow().color.clone();

        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title(shared.display_title())
            .default_width(300)
            .default_height(320)
            .build();
        window.set_css_classes(&[color.css_class()]);

        // Header bar with quick actions.
        let header = HeaderBar::builder().show_title_buttons(true).build();
        header.add_css_class("pinlet-header");

        let new_btn = Button::from_icon_name("list-add-symbolic");
        new_btn.set_tooltip_text(Some("New note"));
        let on_new = Rc::new(on_new);
        new_btn.connect_clicked(move |_| (on_new)());

        // Inline color palette: a popover of swatches.
        let popover = Popover::new();
        let palette = FlowBox::builder()
            .max_children_per_line(3)
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let on_changed = Rc::new(on_changed);
        for swatch_color in NoteColor::PALETTE {
            let swatch = Button::builder()
                .width_request(32)
                .height_request(32)
                .tooltip_text(swatch_color.name())
                .build();
            swatch.add_css_class(swatch_color.css_class());
            let shared = shared.clone();
            let window = window.clone();
            let on_changed = on_changed.clone();
            swatch.connect_clicked(move |_| {
                shared.note.borrow_mut().color = swatch_color.clone();
                window.set_css_classes(&[swatch_color.css_class()]);
                let body = shared.body.borrow();
                (on_changed)(body.clone());
            });
            palette.insert(&swatch, -1);
        }
        popover.set_child(Some(&palette));
        let color_btn = MenuButton::builder()
            .icon_name("color-select-symbolic")
            .popover(&popover)
            .tooltip_text("Note color")
            .build();

        // Delete, with confirmation.
        let delete_btn = Button::from_icon_name("user-trash-symbolic");
        delete_btn.set_tooltip_text(Some("Delete note"));
        {
            let window = window.clone();
            let on_delete = Rc::new(on_delete);
            delete_btn.connect_clicked(move |_| {
                let dialog = adw::MessageDialog::builder()
                    .heading("Delete note?")
                    .body("The note file will be removed; its history remains in git.")
                    .build();
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("delete", "Delete");
                dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                let window = window.clone();
                let on_delete = on_delete.clone();
                dialog.set_transient_for(Some(&window));
                dialog.connect_response(None, move |dialog, response| {
                    if response == "delete" {
                        (on_delete)();
                        window.close();
                    }
                    dialog.close();
                });
                dialog.present();
            });
        }

        // Phase 2/3 placeholders: reminders, locking, pinning.
        for (icon, tooltip) in [
            ("alarm-symbolic", "Reminders — coming soon"),
            ("system-lock-screen-symbolic", "Lock — coming soon"),
            ("view-pin-symbolic", "Pin to desktop — coming soon"),
        ] {
            let stub = Button::from_icon_name(icon);
            stub.set_sensitive(false);
            stub.set_tooltip_text(Some(tooltip));
            header.pack_end(&stub);
        }
        header.pack_end(&delete_btn);
        header.pack_start(&color_btn);
        header.pack_start(&new_btn);

        // Markdown body.
        let text_view = TextView::builder()
            .wrap_mode(gtk4::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(12)
            .right_margin(12)
            .build();
        text_view.add_css_class("pinlet-body");
        text_view.buffer().set_text(&shared.body.borrow());

        let buffer = text_view.buffer();
        buffer.connect_changed(move |buffer| {
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            (on_changed)(text);
        });

        let scroller = ScrolledWindow::builder()
            .child(&text_view)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .build();

        let content = gtk4::Box::new(Orientation::Vertical, 0);
        content.append(&scroller);

        window.set_titlebar(Some(&header));
        window.set_child(Some(&content));

        // A custom hex color needs a dynamic provider.
        let custom_css = match &color {
            NoteColor::Custom(hex) => {
                let provider = gtk4::CssProvider::new();
                provider.load_from_data(&colors::css_for_custom(hex));
                if let Some(display) = gtk4::gdk::Display::default() {
                    gtk4::style_context_add_provider_for_display(
                        &display,
                        &provider,
                        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
                    );
                }
                Some(provider)
            }
            _ => None,
        };

        // Flush pending state when the window closes.
        let on_close = Rc::new(on_close);
        window.connect_close_request(move |_| {
            (on_close)();
            gtk4::glib::Propagation::Proceed
        });

        Self {
            window,
            _custom_css: custom_css,
        }
    }

    /// Show and focus the window.
    pub fn present(&self) {
        self.window.present();
    }
}
