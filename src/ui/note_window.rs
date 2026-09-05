//! The sticky note window: header actions and Markdown body.

use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Local, Utc};
use gtk4::{
    Button, FlowBox, HeaderBar, Label, ListBox, ListBoxRow, MenuButton, Orientation, Popover,
    ScrolledWindow, TextView,
};

use crate::app::SharedNote;
use crate::storage::{NoteColor, Recurrence, Reminder, WindowGeometry};
use crate::ui::colors;
use crate::ui::reminder_dialog;

/// Stylesheet shared by every note window.
pub const STYLE: &str = include_str!("style.css");

/// Callbacks from a note window into the app core.
pub struct NoteCallbacks {
    /// The body text changed (triggers debounced save).
    pub on_changed: Box<dyn Fn(String)>,
    /// The user confirmed deletion.
    pub on_delete: Box<dyn Fn()>,
    /// The user asked for another note.
    pub on_new: Box<dyn Fn()>,
    /// The window is closing (flush pending state).
    pub on_close: Box<dyn Fn()>,
    /// The user picked a due time for a new reminder.
    pub on_add_reminder: Box<dyn Fn(DateTime<Utc>)>,
    /// The user removed reminder `index`.
    pub on_delete_reminder: Box<dyn Fn(usize)>,
    /// The window was resized or moved (debounced geometry save).
    pub on_geometry_changed: Box<dyn Fn()>,
}

/// One window per note, styled like a sheet of paper.
#[derive(Clone)]
pub struct NoteWindow {
    window: gtk4::ApplicationWindow,
    /// Kept alive so a custom hex color stays applied.
    _custom_css: Option<gtk4::CssProvider>,
}

impl NoteWindow {
    /// Build a note window and wire it to the app-core callbacks.
    pub fn new(
        app: &gtk4::Application,
        shared: Rc<SharedNote>,
        geometry: Option<WindowGeometry>,
        callbacks: NoteCallbacks,
    ) -> Self {
        let callbacks = Rc::new(callbacks);
        let color = shared.note.borrow().color.clone();

        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title(shared.display_title())
            .default_width(geometry.map_or(300, |g| g.width))
            .default_height(geometry.map_or(320, |g| g.height))
            .build();
        window.set_css_classes(&[color.css_class()]);

        // Header bar with quick actions.
        let header = HeaderBar::builder().show_title_buttons(true).build();
        header.add_css_class("pinlet-header");

        let new_btn = Button::from_icon_name("list-add-symbolic");
        new_btn.set_tooltip_text(Some("New note"));
        {
            let callbacks = callbacks.clone();
            new_btn.connect_clicked(move |_| (callbacks.on_new)());
        }

        // Inline color palette: a popover of swatches.
        let popover = Popover::new();
        let palette = FlowBox::builder()
            .max_children_per_line(3)
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        for swatch_color in NoteColor::PALETTE {
            let swatch = Button::builder()
                .width_request(32)
                .height_request(32)
                .tooltip_text(swatch_color.name())
                .build();
            swatch.add_css_class(swatch_color.css_class());
            let shared = shared.clone();
            let window = window.clone();
            let callbacks = callbacks.clone();
            swatch.connect_clicked(move |_| {
                shared.note.borrow_mut().color = swatch_color.clone();
                window.set_css_classes(&[swatch_color.css_class()]);
                let body = shared.body.borrow();
                (callbacks.on_changed)(body.clone());
            });
            palette.insert(&swatch, -1);
        }
        popover.set_child(Some(&palette));
        let color_btn = MenuButton::builder()
            .icon_name("color-select-symbolic")
            .popover(&popover)
            .tooltip_text("Note color")
            .build();

        // Reminder popover: rebuilt on every show so it always
        // reflects the note's current reminders.
        let reminders_popover = Popover::new();
        {
            let shared = shared.clone();
            let callbacks = callbacks.clone();
            let window = window.clone();
            reminders_popover.connect_show(move |popover| {
                popover.set_child(Some(&build_reminders_popover(
                    &shared,
                    &callbacks,
                    &window,
                )));
            });
        }
        let reminder_btn = MenuButton::builder()
            .icon_name("alarm-symbolic")
            .popover(&reminders_popover)
            .tooltip_text("Reminders")
            .build();

        // Delete, with confirmation.
        let delete_btn = Button::from_icon_name("user-trash-symbolic");
        delete_btn.set_tooltip_text(Some("Delete note"));
        {
            let window = window.clone();
            let callbacks = callbacks.clone();
            delete_btn.connect_clicked(move |_| {
                let dialog = adw::MessageDialog::builder()
                    .heading("Delete note?")
                    .body("The note file will be removed; its history remains in git.")
                    .build();
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("delete", "Delete");
                dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_transient_for(Some(&window));
                let callbacks = callbacks.clone();
                dialog.connect_response(None, move |dialog, response| {
                    if response == "delete" {
                        (callbacks.on_delete)();
                    }
                    dialog.close();
                });
                dialog.present();
            });
        }

        // Phase 2/3 placeholders: locking, pinning.
        for (icon, tooltip) in [
            ("system-lock-screen-symbolic", "Lock — coming soon"),
            ("view-pin-symbolic", "Pin to desktop — coming soon"),
        ] {
            let stub = Button::from_icon_name(icon);
            stub.set_sensitive(false);
            stub.set_tooltip_text(Some(tooltip));
            header.pack_end(&stub);
        }
        header.pack_end(&delete_btn);
        header.pack_end(&reminder_btn);
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

        {
            let callbacks = callbacks.clone();
            let buffer = text_view.buffer();
            buffer.connect_changed(move |buffer| {
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();
                (callbacks.on_changed)(text);
            });
        }

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

        // Geometry: notify the app core (debounced there) whenever
        // the window is resized.
        {
            let callbacks = callbacks.clone();
            let width = callbacks.clone();
            let height = callbacks.clone();
            window.connect_notify_local(Some("width"), move |_, _| (width.on_geometry_changed)());
            window.connect_notify_local(Some("height"), move |_, _| (height.on_geometry_changed)());
        }

        // Flush pending state when the window closes.
        {
            let callbacks = callbacks.clone();
            window.connect_close_request(move |_| {
                (callbacks.on_close)();
                gtk4::glib::Propagation::Proceed
            });
        }

        Self {
            window,
            _custom_css: custom_css,
        }
    }

    /// Show and focus the window.
    pub fn present(&self) {
        self.window.present();
    }

    /// Whether the window is currently visible.
    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Show or hide the window (tray "show / hide all").
    pub fn set_visible(&self, visible: bool) {
        self.window.set_visible(visible);
    }

    /// Close the window.
    pub fn close(&self) {
        self.window.close();
    }

    /// Current width in logical pixels.
    pub fn width(&self) -> i32 {
        self.window.width()
    }

    /// Current height in logical pixels.
    pub fn height(&self) -> i32 {
        self.window.height()
    }

    /// Screen position of the surface origin, where the compositor
    /// reports one (X11; on Wayland this is (0, 0)).
    pub fn position_on_screen(&self) -> (f64, f64) {
        self.window.surface_transform()
    }
}

/// Build the reminder popover content fresh (called on every show).
fn build_reminders_popover(
    shared: &Rc<SharedNote>,
    callbacks: &Rc<NoteCallbacks>,
    window: &gtk4::ApplicationWindow,
) -> gtk4::Box {
    let content = gtk4::Box::new(Orientation::Vertical, 4);

    let list = ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    for (index, reminder) in shared.note.borrow().reminders.iter().enumerate() {
        let row = ListBoxRow::new();
        let row_box = gtk4::Box::new(Orientation::Horizontal, 8);
        let label = Label::builder()
            .label(format_reminder(reminder))
            .xalign(0.0)
            .hexpand(true)
            .build();
        let delete = Button::from_icon_name("user-trash-symbolic");
        delete.set_tooltip_text(Some("Delete reminder"));
        {
            let callbacks = callbacks.clone();
            delete.connect_clicked(move |_| (callbacks.on_delete_reminder)(index));
        }
        row_box.append(&label);
        row_box.append(&delete);
        row.set_child(Some(&row_box));
        list.append(&row);
    }
    content.append(&list);

    if list.first_child().is_none() {
        let empty = Label::builder()
            .label("No reminders yet")
            .xalign(0.0)
            .build();
        empty.add_css_class("dim-label");
        content.append(&empty);
    }

    let add = Button::builder().label("Add reminder…").build();
    {
        let callbacks = callbacks.clone();
        let window = window.clone();
        add.connect_clicked(move |_| {
            let callbacks = callbacks.clone();
            reminder_dialog::present(&window, move |due| (callbacks.on_add_reminder)(due));
        });
    }
    content.append(&add);
    content
}

/// Human-readable reminder label: local due time plus recurrence.
fn format_reminder(reminder: &Reminder) -> String {
    let when = reminder.due_at.with_timezone(&Local).format("%b %e, %H:%M");
    match reminder.recurrence_rule {
        Recurrence::None => format!("{when}"),
        Recurrence::Daily => format!("{when} · daily"),
        Recurrence::Weekly => format!("{when} · weekly"),
        Recurrence::Weekdays => format!("{when} · weekdays"),
        Recurrence::Custom(n) => format!("{when} · every {n} d"),
    }
}
