//! Quick-find window (spec §3.8): `Ctrl+Shift+F`, type, pick a note.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::gdk;
use gtk4::glib::{self, SourceId};
use gtk4::{
    Box as GtkBox, Entry, Label, ListBox, ListBoxRow, Orientation,
};
use uuid::Uuid;

use crate::search::SearchHit;
use crate::timer::cancel_source;

/// Debounce for search-as-you-type.
const QUERY_DEBOUNCE: Duration = Duration::from_millis(150);

/// One search window per application; shown and hidden on demand.
pub struct SearchWindow {
    window: adw::Window,
    entry: Entry,
    list: ListBox,
    /// Pending debounced query timer.
    source: Rc<RefCell<Option<SourceId>>>,
}

impl SearchWindow {
    /// Build the window; `on_query` runs debounced on input and
    /// `on_open` fires when a result row is activated.
    pub fn new(
        app: &gtk4::Application,
        on_query: impl Fn(String) + 'static,
        on_open: impl Fn(Uuid) + 'static,
    ) -> Rc<Self> {
        let entry = Entry::builder()
            .placeholder_text("Search notes…")
            .hexpand(true)
            .build();

        let list = ListBox::builder()
            .selection_mode(gtk4::SelectionMode::Single)
            .vexpand(true)
            .build();

        let scroller = gtk4::ScrolledWindow::builder()
            .child(&list)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .build();

        let content = GtkBox::new(Orientation::Vertical, 0);
        content.append(&entry);
        content.append(&scroller);

        let window = adw::Window::builder()
            .application(app)
            .title("Find notes")
            .default_width(420)
            .default_height(480)
            .content(&content)
            .build();

        let this = Rc::new(Self {
            window,
            entry,
            list,
            source: Rc::new(RefCell::new(None)),
        });

        // Search as you type, debounced.
        {
            let source_cell = this.source.clone();
            let on_query = Rc::new(on_query);
            this.entry.connect_changed(move |entry| {
                let text = entry.text().to_string();
                if let Some(source) = source_cell.borrow_mut().take() {
                    cancel_source(source);
                }
                let query = on_query.clone();
                let source = glib::timeout_add_local_once(QUERY_DEBOUNCE, move || query(text));
                *source_cell.borrow_mut() = Some(source);
            });
        }

        // Activating a row focuses the note; Escape hides the window.
        {
            let on_open = Rc::new(on_open);
            this.list.connect_row_activated(move |_, row| {
                if let Ok(id) = row.widget_name().parse::<Uuid>() {
                    (on_open)(id);
                }
            });

            let key = gtk4::EventControllerKey::new();
            let window = this.window.clone();
            key.connect_key_pressed(move |_, keyval, _, _| {
                if keyval == gdk::Key::Escape {
                    window.set_visible(false);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            this.window.add_controller(key);
        }

        this
    }

    /// Replace the result list.
    pub fn set_results(&self, hits: &[SearchHit]) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        for hit in hits {
            let title = Label::builder().label(&hit.title).xalign(0.0).build();
            title.add_css_class("heading");

            let content = GtkBox::new(Orientation::Vertical, 2);
            content.append(&title);
            if !hit.snippet.is_empty() {
                let snippet = Label::builder()
                    .label(&hit.snippet)
                    .xalign(0.0)
                    .wrap(true)
                    .build();
                snippet.add_css_class("dim-label");
                content.append(&snippet);
            }

            let row = ListBoxRow::new();
            row.set_child(Some(&content));
            // The note id travels as the row's widget name.
            row.set_widget_name(&hit.id.to_string());
            self.list.append(&row);
        }
    }

    /// Show, focus, and select the current query.
    pub fn present(&self) {
        self.window.present();
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    /// Hide without destroying, so state survives.
    pub fn hide(&self) {
        self.window.set_visible(false);
    }
}
