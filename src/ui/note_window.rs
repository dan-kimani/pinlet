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
use crate::markdown::{MarkdownStyler, source_to_preview};
use crate::pinning::PinBackend;
use crate::storage::{NoteColor, Recurrence, Reminder, WindowGeometry};
use crate::ui::colors;
use crate::ui::reminder_dialog;

/// Stylesheet shared by every note window.
pub const STYLE: &str = include_str!("style.css");

/// Click position in buffer coordinates for `TextView::iter_at_location`.
/// Gesture handlers report widget coordinates, but hit-testing wants
/// buffer coordinates — the two diverge by the scroll offset, so without
/// this a click lands above the intended spot in any scrolled note.
fn buffer_coords(view: &TextView, x: f64, y: f64) -> Option<(i32, i32)> {
    Some(view.window_to_buffer_coords(gtk4::TextWindowType::Text, x as i32, y as i32))
}

/// Whether the Ctrl modifier is currently held. Read from the given display's
/// keyboard rather than the event state, so it also works on the X11 desktop
/// windows whose separate display reports an empty event state.
fn ctrl_held(display: &gdk::Display) -> bool {
    display
        .default_seat()
        .and_then(|seat| seat.keyboard())
        .is_some_and(|keyboard| {
            keyboard
                .modifier_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
        })
}

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
    /// The user picked a due time and repetition for a new reminder.
    pub on_add_reminder: Box<dyn Fn(DateTime<Utc>, Recurrence)>,
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
    /// Dynamic provider for a custom hex color, swapped by
    /// [`NoteWindow::apply_color`]. Kept alive so the styling stays
    /// applied. Shared with the palette swatches (via `Rc`) so leaving
    /// a custom color also removes its display-wide rules.
    custom_css: Rc<RefCell<Option<gtk4::CssProvider>>>,
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

        // Dynamic custom-color provider, shared with the palette swatches
        // below so switching away from a custom color also removes the
        // stale display-wide rules it installed.
        let custom_css = Rc::new(RefCell::new(None));

        // Inline color palette: a popover of swatches. Each swatch is a flat
        // Button wrapping a colored Box — the Box renders the color reliably
        // (a Button draws its own theme background over `background-color`),
        // while the Button keeps a plain `clicked` signal.
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
            swatch.add_css_class("pinlet-swatch");
            let color = gtk4::Box::new(Orientation::Horizontal, 0);
            color.add_css_class(swatch_color.css_class());
            color.add_css_class("pinlet-swatch");
            color.set_hexpand(true);
            color.set_vexpand(true);
            swatch.set_child(Some(&color));
            {
                let shared = shared.clone();
                let window = window.clone();
                let callbacks = callbacks.clone();
                let swatch_color = swatch_color.clone();
                let custom_css = custom_css.clone();
                swatch.connect_clicked(move |_| {
                    shared.note.borrow_mut().color = swatch_color.clone();
                    // Route through the shared helper (not a bare class
                    // swap) so a previous custom color's provider is
                    // removed from the display.
                    apply_color_to(
                        &window,
                        color_display(&window, x11_desktop),
                        &custom_css,
                        &swatch_color,
                    );
                    // Clone the body and drop the borrow before calling
                    // out: on_changed writes back into `shared.body`.
                    let body = shared.body.borrow().clone();
                    (callbacks.on_changed)(body);
                });
            }
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
                    &shared, &callbacks, &window, popover,
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
        lock_btn.set_tooltip_text(Some(if locked { "Unlock note" } else { "Lock note" }));
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
                        // Margins are unsigned on the wire: clamp rather
                        // than push the window into protocol-error limbo.
                        let left = (start_left + dx as i32).max(0);
                        let top = (start_top + dy as i32).max(0);
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
        let styler = Rc::new(MarkdownStyler::new(&preview_view.buffer()));

        // Open links in the default handler on Ctrl+click, and show a pointer
        // cursor while hovering a link.
        let link_click = gtk4::GestureClick::new();
        {
            let view = preview_view.clone();
            link_click.connect_released(move |_gesture, n_press, x, y| {
                if n_press != 1 || !ctrl_held(&view.display()) {
                    return;
                }
                let Some((bx, by)) = buffer_coords(&view, x, y) else {
                    return;
                };
                let Some(iter) = view.iter_at_location(bx, by) else {
                    return;
                };
                for tag in iter.tags() {
                    let Some(name) = tag.name() else { continue };
                    let Some(url) = name.as_str().strip_prefix("link:") else {
                        continue;
                    };
                    let context =
                        gtk4::gdk::Display::default().map(|display| display.app_launch_context());
                    if let Err(err) =
                        gtk4::gio::AppInfo::launch_default_for_uri(url, context.as_ref())
                    {
                        eprintln!("failed to open link {url}: {err}");
                    }
                    break;
                }
            });
        }
        preview_view.add_controller(link_click);

        let link_hover = gtk4::EventControllerMotion::new();
        {
            let view = preview_view.clone();
            link_hover.connect_motion(move |_controller, x, y| {
                let over_link = buffer_coords(&view, x, y)
                    .and_then(|(bx, by)| view.iter_at_location(bx, by))
                    .is_some_and(|iter| {
                        iter.tags()
                            .iter()
                            .any(|tag| tag.name().is_some_and(|n| n.as_str().starts_with("link:")))
                    });
                view.set_cursor_from_name(over_link.then_some("pointer"));
            });
        }
        preview_view.add_controller(link_hover);

        // Clicking a checkbox in the preview toggles the matching task
        // item in the editable source (preview line N is source line N:
        // the checkbox transform never adds or removes newlines). The
        // edit goes through the normal change → debounced-save path.
        let checkbox_click = gtk4::GestureClick::new();
        // Left button only: middle/right presses keep their default
        // behavior (paste, context menu).
        checkbox_click.set_button(gdk::BUTTON_PRIMARY);
        {
            let edit_view = edit_view.clone();
            let preview_view = preview_view.clone();
            let styler = styler.clone();
            checkbox_click.connect_pressed(move |gesture, n_press, x, y| {
                // Ctrl+click is the link gesture; locked notes show no
                // checkboxes anyway (the glyph hit-test would miss).
                if n_press != 1 || locked || ctrl_held(&preview_view.display()) {
                    return;
                }
                let Some((bx, by)) = buffer_coords(&preview_view, x, y) else {
                    return;
                };
                let Some(iter) = preview_view.iter_at_location(bx, by) else {
                    return;
                };
                if toggle_checkbox_at(&edit_view, &preview_view, &iter) {
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                    refresh_preview(&edit_view, &preview_view, &styler);
                }
            });
        }
        preview_view.add_controller(checkbox_click);

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
            let styler = styler.clone();
            preview_toggle.connect_toggled(move |button| {
                if button.is_active() {
                    button.set_icon_name("view-reveal-symbolic");
                    button.set_tooltip_text(Some("Edit"));
                    refresh_preview(&edit_view, &preview_view, &styler);
                    stack.set_visible_child(&preview_view);
                } else {
                    button.set_icon_name("view-conceal-symbolic");
                    button.set_tooltip_text(Some("Preview"));
                    stack.set_visible_child(&edit_view);
                }
            });
        }

        // Open in preview mode when the note already has content; an empty
        // note starts in edit mode so the user can begin typing.
        if !locked && !shared.body.borrow().trim().is_empty() {
            preview_toggle.set_active(true);
        }

        // Drag & drop appends to the editor (which holds the editable
        // source), turning file URIs into Markdown links. Locked notes
        // show a placeholder, never the body — a drop must not rewrite
        // their encrypted blob with placeholder text.
        if !locked {
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
        }

        let scroller = ScrolledWindow::builder()
            .child(&stack)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .build();

        let content = gtk4::Box::new(Orientation::Vertical, 0);
        content.append(&scroller);

        window.set_titlebar(Some(&header));
        window.set_child(Some(&content));

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

        // Esc closes the note through the same path as the window
        // controls (save, geometry, deregister). Open popovers consume
        // Escape themselves first, so this only fires when none is up.
        {
            let window = window.clone();
            let esc = gtk4::ShortcutController::new();
            esc.set_scope(gtk4::ShortcutScope::Managed);
            if let Some(trigger) = gtk4::ShortcutTrigger::parse_string("Escape") {
                let window = window.clone();
                esc.add_shortcut(gtk4::Shortcut::new(
                    Some(trigger),
                    Some(gtk4::CallbackAction::new(move |_, _| {
                        window.close();
                        glib::Propagation::Stop
                    })),
                ));
            }
            window.add_controller(esc);
        }

        // The X11 desktop window lives on a different display than the
        // default one, and the surface may not exist yet — resolve the
        // right display up front so a custom color lands correctly.
        let initial_display = x11_display.or_else(gdk::Display::default);
        let this = Self {
            window,
            pinned,
            x11_desktop,
            margins,
            custom_css,
        };
        // Route through the shared helper so named and custom hex colors
        // share one path (custom colors need a dynamic provider).
        apply_color_to(&this.window, initial_display, &this.custom_css, &color);
        this
    }

    /// Show and focus the window.
    pub fn present(&self) {
        self.window.present();
    }

    /// Restyle the window with `color` (spec Mode B applies a global
    /// color without changing the note's stored color). Custom hex
    /// colors need a dynamic CSS provider: the previous one is removed
    /// from the display so stale rules don't pile up.
    pub fn apply_color(&self, color: &NoteColor) {
        apply_color_to(
            &self.window,
            color_display(&self.window, self.x11_desktop),
            &self.custom_css,
            color,
        );
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

/// Display a window's styling belongs to: its own surface's display
/// when realized (X11 desktop windows live on a different display
/// than the default), falling back to the default display and then
/// the cached X11 display.
fn color_display(window: &gtk4::Window, x11_desktop: bool) -> Option<gdk::Display> {
    window
        .surface()
        .map(|surface| surface.display())
        .or_else(gdk::Display::default)
        .or_else(|| if x11_desktop { x11::display() } else { None })
}

/// (Re)style a window: set its palette class and swap the dynamic
/// custom-color provider, removing the previous one so stale rules
/// never pile up on the display. Malformed custom colors install no
/// provider (the palette class alone still applies).
fn apply_color_to(
    window: &gtk4::Window,
    display: Option<gdk::Display>,
    slot: &RefCell<Option<gtk4::CssProvider>>,
    color: &NoteColor,
) {
    window.set_css_classes(&[color.css_class()]);
    let mut slot = slot.borrow_mut();
    if let Some(old) = slot.take() {
        if let Some(display) = display.as_ref() {
            gtk4::style_context_remove_provider_for_display(display, &old);
        }
    }
    if let NoteColor::Custom(hex) = color {
        if colors::is_valid_hex(hex) {
            let provider = gtk4::CssProvider::new();
            provider.load_from_data(&colors::css_for_custom(hex));
            if let Some(display) = display.as_ref() {
                gtk4::style_context_add_provider_for_display(
                    display,
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
            *slot = Some(provider);
        }
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
                            &shared, &callbacks, &window, &popover,
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
            reminder_dialog::present(&window, move |due, recurrence| {
                (callbacks.on_add_reminder)(due, recurrence)
            });
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
                let lower = line.to_ascii_lowercase();
                let is_image = [".png", ".jpg", ".jpeg", ".svg", ".webp"]
                    .iter()
                    .any(|ext| lower.ends_with(ext));
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

/// Re-render the preview view from the edit buffer's current source.
fn refresh_preview(edit_view: &TextView, preview_view: &TextView, styler: &MarkdownStyler) {
    let buffer = edit_view.buffer();
    let source = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string();
    let preview = source_to_preview(&source);
    preview_view.buffer().set_text(&preview);
    styler.restyle(&preview_view.buffer());
}

/// Toggle the task item on the source line matching the preview
/// position `at`. Returns true when a checkbox flipped: the preview
/// position must sit on a `☐`/`☑` glyph (placeholders and literal
/// brackets in code blocks have none), and the source line must carry
/// the corresponding marker.
fn toggle_checkbox_at(edit_view: &TextView, preview_view: &TextView, at: &gtk4::TextIter) -> bool {
    // Glyph under the cursor?
    let preview = preview_view.buffer();
    let Some(line_start) = preview.iter_at_line(at.line()) else {
        return false;
    };
    let mut line_end = line_start;
    line_end.forward_to_line_end();
    let line_text = preview.text(&line_start, &line_end, true).to_string();
    // Forgiving hit area: the glyph plus one character of slack on each
    // side, so the whole checkbox region (not just the exact glyph)
    // picks up the click. The glyph must still lead the line (past
    // indentation) — a `☐` typed literally inside task text is not a
    // checkbox.
    let idx = at.line_offset() as usize;
    let mut checked = None;
    for (n, (b, c)) in line_text.char_indices().enumerate() {
        if c != '☐' && c != '☑' {
            continue;
        }
        if !line_text[..b].trim().is_empty() || idx.abs_diff(n) > 1 {
            return false;
        }
        checked = Some(c == '☑');
        break;
    }
    let Some(checked) = checked else {
        return false;
    };

    // Same line in the editable source: flip its task marker in place
    // (one undo step), which fires the normal change → save path.
    let edit = edit_view.buffer();
    let Some(start) = edit.iter_at_line(at.line()) else {
        return false;
    };
    let mut end = start;
    end.forward_to_line_end();
    let src = edit.text(&start, &end, true).to_string();
    let Some((byte, _len)) = find_checkbox(&src) else {
        return false;
    };
    let col = src[..byte].chars().count() as i32;
    let replacement = if checked { "[ ]" } else { "[x]" };
    let mut rs = start;
    rs.forward_chars(col);
    let mut re = rs;
    re.forward_chars(3);
    edit.begin_user_action();
    edit.delete(&mut rs, &mut re);
    edit.insert(&mut rs, replacement);
    edit.end_user_action();
    true
}

/// Byte offset of the task-list marker on a source line: the
/// `[ ]`/`[x]`/`[X]` following the list bullet, or the first such
/// bracket group when the bullet doesn't parse (defensive: the
/// preview glyph proves a real task item produced this line).
fn find_checkbox(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    // Skip `-`, `*`, `+`, or an ordered-list marker (`1.` / `1)`).
    if i < bytes.len() && matches!(bytes[i], b'-' | b'*' | b'+') {
        i += 1;
    } else {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start || i >= bytes.len() || !matches!(bytes[i], b'.' | b')') {
            return first_bracket_group(line);
        }
        i += 1;
    }
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if is_checkbox_at(bytes, i) {
        return Some((i, 3));
    }
    first_bracket_group(line)
}

/// `[` + space/`x`/`X` + `]` at byte offset `i`?
fn is_checkbox_at(bytes: &[u8], i: usize) -> bool {
    bytes.len() >= i + 3
        && bytes[i] == b'['
        && matches!(bytes[i + 1], b' ' | b'x' | b'X')
        && bytes[i + 2] == b']'
}

/// First `[ ]`/`[x]`/`[X]` group anywhere (fallback for exotic
/// bullets; the preview glyph proves a task item is on this line).
fn first_bracket_group(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    (0..bytes.len())
        .find(|&i| is_checkbox_at(bytes, i))
        .map(|i| (i, 3))
}

#[cfg(test)]
mod tests {
    use super::find_checkbox;

    #[test]
    fn checkbox_found_after_bullets() {
        assert_eq!(find_checkbox("- [ ] task"), Some((2, 3)));
        assert_eq!(find_checkbox("- [x] done"), Some((2, 3)));
        assert_eq!(find_checkbox("  * [X] star"), Some((4, 3)));
        assert_eq!(find_checkbox("1. [ ] ordered"), Some((3, 3)));
        assert_eq!(find_checkbox("12) [x] paren"), Some((4, 3)));
    }

    #[test]
    fn checkbox_prefers_marker_over_later_brackets() {
        assert_eq!(find_checkbox("- [ ] a [x] b"), Some((2, 3)));
    }

    #[test]
    fn checkbox_missing_without_marker() {
        assert_eq!(find_checkbox("no box here"), None);
        assert_eq!(find_checkbox("- just a dash"), None);
        assert_eq!(find_checkbox(""), None);
    }
}
