//! The sticky note window: header actions and Markdown body.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use chrono::{DateTime, Local, Utc};
use gtk4::gdk;
use gtk4::glib;
use gtk4_layer_shell::{Edge, Layer, LayerShell};

use crate::timer::cancel_source;

use crate::ui::x11;
use gtk4::{
    Button, FlowBox, HeaderBar, Label, ListBox, ListBoxRow, MenuButton, Orientation, Popover,
    ScrolledWindow, Stack, TextBuffer, TextView, ToggleButton,
};

use crate::app::SharedNote;
use crate::markdown::{source_to_preview, MarkdownStyler};
use crate::pinning::PinBackend;
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
    /// The user toggled desktop pinning.
    pub on_toggle_pin: Box<dyn Fn()>,
    /// The user asked to lock or unlock the note.
    pub on_lock_requested: Box<dyn Fn()>,
}

/// One window per note, styled like a sheet of paper.
#[derive(Clone)]
pub struct NoteWindow {
    window: gtk4::Window,
    /// True when the window lives on the desktop layer (spec §3.6).
    pinned: bool,
    /// True when pinned via an X11 desktop-type window (compositors
    /// without layer-shell, e.g. Ubuntu's mutter).
    x11_desktop: bool,
    /// Position of a pinned window — layer-shell margins, or the
    /// tracked X11 position — updated while dragging.
    margins: Rc<Cell<(i32, i32)>>,
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
        pin_backend: PinBackend,
    ) -> Self {
        let callbacks = Rc::new(callbacks);
        let color = shared.note.borrow().color.clone();
        let pinned = shared.note.borrow().is_pinned_to_desktop;

        // Position of a pinned note: X11 coordinates or layer-shell
        // margins, tracked while dragging.
        let margins = Rc::new(Cell::new((
            geometry.map_or(60, |g| g.x),
            geometry.map_or(60, |g| g.y),
        )));

        // Desktop pinning, two mechanisms in-app:
        // - an X11 desktop-type window (works under Ubuntu's mutter,
        //   whose layer-shell protocol is compiled out);
        // - the layer-shell protocol (KDE, wlroots compositors).
        let x11_desktop = pinned && pin_backend == PinBackend::X11;
        let x11_display = if x11_desktop { x11::display() } else { None };
        let window: gtk4::Window = if x11_desktop {
            let plain = gtk4::Window::builder()
                .title(shared.display_title())
                .default_width(geometry.map_or(300, |g| g.width))
                .default_height(geometry.map_or(320, |g| g.height))
                .build();
            if let Some(x11_display) = x11_display.as_ref() {
                plain.set_display(x11_display);
            }
            plain
        } else {
            gtk4::ApplicationWindow::builder()
                .application(app)
                .title(shared.display_title())
                .default_width(geometry.map_or(300, |g| g.width))
                .default_height(geometry.map_or(320, |g| g.height))
                .build()
                .upcast()
        };
        window.set_css_classes(&[color.css_class()]);

        if x11_desktop {
            let (left, top) = margins.get();
            window.connect_realize(move |window| {
                x11::apply(window, left, top);
            });
            window.connect_map(move |window| {
                x11::apply(window, left, top);
            });
        } else if pinned && pin_backend == PinBackend::LayerShell {
            window.init_layer_shell();
            window.set_layer(Layer::Background);
            window.set_anchor(Edge::Left, true);
            window.set_anchor(Edge::Top, true);
            let (left, top) = margins.get();
            window.set_margin(Edge::Left, left);
            window.set_margin(Edge::Top, top);
            window.set_exclusive_zone(0);
        }

        // Header bar with quick actions. Window controls (min/max/close) are
        // disabled: GTK appends them to the far right *after* the packed
        // buttons, which would push `delete` off the right edge. The note is
        // still closable via the tray or Alt+F4.
        let header = HeaderBar::builder().show_title_buttons(false).build();
        header.add_css_class("pinlet-header");
        // The window title is kept for the task switcher, but the header stays
        // lean: an empty title widget suppresses the header's built-in title.
        header.set_title_widget(Some(&Label::new(None)));


        let new_btn = Button::from_icon_name("list-add-symbolic");
        new_btn.set_tooltip_text(Some("New note"));
        {
            let callbacks = callbacks.clone();
            new_btn.connect_clicked(move |_| (callbacks.on_new)());
        }

        // Inline color palette: a popover of swatches. Swatches are Boxes,
        // not Buttons — a Button draws its own theme background over
        // `background-color`, which leaves the swatches looking black.
        let popover = Popover::new();
        let palette = FlowBox::builder()
            .max_children_per_line(3)
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        for swatch_color in NoteColor::PALETTE {
            let swatch = gtk4::Box::builder()
                .width_request(32)
                .height_request(32)
                .tooltip_text(swatch_color.name())
                .build();
            swatch.add_css_class(swatch_color.css_class());
            swatch.add_css_class("pinlet-swatch");
            let click = gtk4::GestureClick::new();
            {
                let shared = shared.clone();
                let window = window.clone();
                let callbacks = callbacks.clone();
                let swatch_color = swatch_color.clone();
                click.connect_released(move |_, n_press, _, _| {
                    if n_press != 1 {
                        return;
                    }
                    shared.note.borrow_mut().color = swatch_color.clone();
                    window.set_css_classes(&[swatch_color.css_class()]);
                    // Clone the body and drop the borrow before calling
                    // out: on_changed writes back into `shared.body`.
                    let body = shared.body.borrow().clone();
                    (callbacks.on_changed)(body);
                });
            }
            swatch.add_controller(click);
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
                    popover,
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

        // Desktop pinning toggle (spec §3.6).
        let pin_btn = Button::from_icon_name("view-pin-symbolic");
        if pin_backend != PinBackend::None {
            pin_btn.set_tooltip_text(Some(if pinned {
                "Unpin from desktop"
            } else {
                "Pin to desktop"
            }));
            if pinned {
                pin_btn.add_css_class("suggested-action");
            }
            let callbacks = callbacks.clone();
            pin_btn.connect_clicked(move |_| (callbacks.on_toggle_pin)());
        } else {
            pin_btn.set_sensitive(false);
            pin_btn.set_tooltip_text(Some(
                "Pinning unavailable — no layer-shell and no X11/XWayland desktop",
            ));
        }

        // Locking (spec §3.10).
        let locked = shared.note.borrow().is_locked;
        let lock_btn = Button::from_icon_name("system-lock-screen-symbolic");
        lock_btn.set_tooltip_text(Some(if locked {
            "Unlock note"
        } else {
            "Lock note"
        }));
        {
            let callbacks = callbacks.clone();
            lock_btn.connect_clicked(move |_| (callbacks.on_lock_requested)());
        }

        // Edit/Preview toggle: an eye icon. Open eye = preview, closed = edit.
        // Created here so it can sit in the header's action order; its toggled
        // handler is wired up later, once the edit/preview views exist.
        let preview_toggle = ToggleButton::new();
        preview_toggle.set_icon_name("view-conceal-symbolic");
        preview_toggle.set_tooltip_text(Some("Preview"));

        header.pack_start(&new_btn);
        header.pack_end(&delete_btn);
        header.pack_end(&lock_btn);
        header.pack_end(&pin_btn);
        header.pack_end(&preview_toggle);
        header.pack_end(&reminder_btn);
        header.pack_end(&color_btn);

        // Pinned windows have no window manager, so the header
        // doubles as a drag handle: layer margins or the X11
        // position, depending on the mechanism. Under the shell
        // extension the note is a normal window, so it keeps normal
        // window-manager dragging.
        if pinned && (x11_desktop || pin_backend == PinBackend::LayerShell) {
            let drag = gtk4::GestureDrag::new();
            // The X11 desktop window is dragged by a polling loop that reads
            // the pointer position and button state via `query_pointer`, so it
            // keeps tracking even after GTK ends its gesture (which happens
            // once the window starts moving under the pointer). Layer-shell
            // windows keep a margin-based drag.
            let drag_poll: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
            if x11_desktop {
                let window = window.clone();
                let margins = margins.clone();
                let callbacks = callbacks.clone();
                let drag_poll = drag_poll.clone();
                drag.connect_drag_begin(move |_gesture, _x, _y| {
                    if let Some(id) = drag_poll.borrow_mut().take() {
                        cancel_source(id);
                    }
                    let Some((start_px, start_py, _)) = x11::drag_pointer() else {
                        return;
                    };
                    let (start_left, start_top) = margins.get();
                    let window = window.clone();
                    let margins = margins.clone();
                    let callbacks = callbacks.clone();
                    let poll_poll = drag_poll.clone();
                    let source = glib::timeout_add_local(Duration::from_millis(16), move || {
                        let Some((px, py, held)) = x11::drag_pointer() else {
                            return glib::ControlFlow::Break;
                        };
                        if !held {
                            poll_poll.borrow_mut().take();
                            (callbacks.on_geometry_changed)();
                            return glib::ControlFlow::Break;
                        }
                        let left = start_left + (px - start_px);
                        let top = start_top + (py - start_py);
                        x11::move_to(&window, left, top);
                        margins.set((left, top));
                        glib::ControlFlow::Continue
                    });
                    *drag_poll.borrow_mut() = Some(source);
                });
            } else {
                let drag_start = Rc::new(Cell::new((0i32, 0i32)));
                {
                    let margins = margins.clone();
                    let drag_start = drag_start.clone();
                    drag.connect_drag_begin(move |_gesture, _x, _y| {
                        drag_start.set(margins.get());
                    });
                }
                {
                    let window = window.clone();
                    let margins = margins.clone();
                    let drag_start = drag_start.clone();
                    drag.connect_drag_update(move |_gesture, dx, dy| {
                        let (start_left, start_top) = drag_start.get();
                        let left = start_left + dx as i32;
                        let top = start_top + dy as i32;
                        window.set_margin(Edge::Left, left);
                        window.set_margin(Edge::Top, top);
                        margins.set((left, top));
                    });
                }
                {
                    let callbacks = callbacks.clone();
                    drag.connect_drag_end(move |_gesture, _dx, _dy| {
                        (callbacks.on_geometry_changed)();
                    });
                }
            }
            header.add_controller(drag);
        }

        // Edit mode: the raw Markdown source. Preview mode: a read-only render.
        let edit_view = TextView::builder()
            .wrap_mode(gtk4::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(12)
            .right_margin(12)
            .build();
        edit_view.add_css_class("pinlet-body");

        let preview_view = TextView::builder()
            .wrap_mode(gtk4::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(12)
            .right_margin(12)
            .editable(false)
            .cursor_visible(false)
            .build();
        preview_view.add_css_class("pinlet-body");
        let styler = MarkdownStyler::new(&preview_view.buffer());

        if locked {
            edit_view.set_editable(false);
            edit_view.buffer().set_text(
                "🔒 This note is locked.\n\n\
                 Unlock it with the lock button to view and edit its contents.",
            );
        } else {
            edit_view.buffer().set_text(&shared.body.borrow());
        }

        // The editor holds the canonical source: saving is a straight copy.
        {
            let callbacks = callbacks.clone();
            let buffer = edit_view.buffer();
            buffer.connect_changed(move |buffer| {
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), true)
                    .to_string();
                (callbacks.on_changed)(text);
            });
        }

        // Toggle between the raw editor and the rendered preview.
        let stack = Stack::new();
        stack.add_named(&edit_view, Some("edit"));
        stack.add_named(&preview_view, Some("preview"));
        stack.set_visible_child(&edit_view);

        // Wire the eye toggle to the edit/preview views now that they exist.
        {
            let edit_view = edit_view.clone();
            let preview_view = preview_view.clone();
            let stack = stack.clone();
            preview_toggle.connect_toggled(move |button| {
                if button.is_active() {
                    button.set_icon_name("view-reveal-symbolic");
                    button.set_tooltip_text(Some("Edit"));
                    let source = edit_view
                        .buffer()
                        .text(
                            &edit_view.buffer().start_iter(),
                            &edit_view.buffer().end_iter(),
                            true,
                        )
                        .to_string();
                    let preview = source_to_preview(&source);
                    preview_view.buffer().set_text(&preview);
                    styler.restyle(&preview_view.buffer());
                    stack.set_visible_child(&preview_view);
                } else {
                    button.set_icon_name("view-conceal-symbolic");
                    button.set_tooltip_text(Some("Preview"));
                    stack.set_visible_child(&edit_view);
                }
            });
        }

        // Drag & drop appends to the editor (which holds the editable
        // source), turning file URIs into Markdown links.
        let drop_target = gtk4::DropTarget::new(glib::Type::STRING, gdk::DragAction::COPY);
        {
            let buffer = edit_view.buffer();
            drop_target.connect_drop(move |_target, value, _x, _y| {
                if let Ok(text) = value.get::<String>() {
                    append_drop(&buffer, &text);
                    return true;
                }
                false
            });
        }
        edit_view.add_controller(drop_target);

        let scroller = ScrolledWindow::builder()
            .child(&stack)
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
            pinned,
            x11_desktop,
            margins,
            _custom_css: custom_css,
        }
    }

    /// Show and focus the window.
    pub fn present(&self) {
        self.window.present();
    }

    /// Restyle the window with `color` (spec Mode B applies a global
    /// color without changing the note's stored color).
    pub fn apply_color(&self, color: &NoteColor) {
        self.window.set_css_classes(&[color.css_class()]);
    }

    /// The underlying window (for dialogs parented to this note).
    pub fn window(&self) -> &gtk4::Window {
        &self.window
    }

    /// Show a transient error dialog over this window.
    pub fn show_error(&self, message: &str) {
        let dialog = adw::MessageDialog::builder()
            .heading("Pinlet")
            .body(message)
            .build();
        dialog.add_response("ok", "OK");
        dialog.set_transient_for(Some(&self.window));
        dialog.connect_response(None, |dialog, _| dialog.close());
        dialog.present();
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

    /// Whether this window lives on the desktop layer.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether the compositor actually put this window on a layer
    /// (the truth, as opposed to the requested pin state).
    pub fn is_layer_window(&self) -> bool {
        self.window.is_layer_window()
    }

    /// Whether this is an X11 desktop-type window (the layer-shell
    /// alternative used under Ubuntu's mutter).
    pub fn is_desktop_window(&self) -> bool {
        self.x11_desktop
    }

    /// Position of a pinned window: its layer-shell margins.
    pub fn pinned_position(&self) -> (f64, f64) {
        let (left, top) = self.margins.get();
        (f64::from(left), f64::from(top))
    }
}

/// Build the reminder popover content fresh (called on every show).
fn build_reminders_popover(
    shared: &Rc<SharedNote>,
    callbacks: &Rc<NoteCallbacks>,
    window: &gtk4::Window,
    popover: &gtk4::Popover,
) -> gtk4::Box {
    let content = gtk4::Box::new(Orientation::Vertical, 4);

    let list = ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    for (index, reminder) in shared.note.borrow().reminders.iter().enumerate() {
        let reminder_label = format_reminder(reminder);
        let row = ListBoxRow::new();
        let row_box = gtk4::Box::new(Orientation::Horizontal, 8);
        let label = Label::builder()
            .label(&reminder_label)
            .xalign(0.0)
            .hexpand(true)
            .build();
        let delete = Button::from_icon_name("user-trash-symbolic");
        delete.set_tooltip_text(Some("Delete reminder"));
        {
            let shared = shared.clone();
            let callbacks = callbacks.clone();
            let window = window.clone();
            let popover = popover.clone();
            let reminder_label = reminder_label.clone();
            delete.connect_clicked(move |_| {
                let dialog = adw::MessageDialog::builder()
                    .heading("Delete reminder?")
                    .body(&reminder_label)
                    .build();
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("delete", "Delete");
                dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_transient_for(Some(&window));
                let shared = shared.clone();
                let callbacks = callbacks.clone();
                let window = window.clone();
                let popover = popover.clone();
                dialog.connect_response(None, move |dialog, response| {
                    if response == "delete" {
                        (callbacks.on_delete_reminder)(index);
                        // Rebuild the list in place so the deletion is visible.
                        popover.set_child(Some(&build_reminders_popover(
                            &shared,
                            &callbacks,
                            &window,
                            &popover,
                        )));
                    }
                    dialog.close();
                });
                dialog.present();
            });
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

/// Append dropped text to the end of the buffer: image file URIs
/// become Markdown image links, other URIs become links, and plain
/// text is appended as-is.
fn append_drop(buffer: &TextBuffer, text: &str) {
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with("file://") {
                let is_image = [".png", ".jpg", ".jpeg", ".svg", ".webp"]
                    .iter()
                    .any(|ext| line.ends_with(ext));
                if is_image {
                    format!("![image]({line})")
                } else {
                    format!("[file]({line})")
                }
            } else {
                line.to_owned()
            }
        })
        .collect();

    let mut end = buffer.end_iter();
    let mut inserted = String::new();
    if !end.starts_line() {
        inserted.push('\n');
    }
    inserted.push_str(&lines.join("\n"));
    inserted.push('\n');

    buffer.begin_user_action();
    buffer.insert(&mut end, &inserted);
    buffer.end_user_action();
    let end = buffer.end_iter();
    buffer.place_cursor(&end);
}
