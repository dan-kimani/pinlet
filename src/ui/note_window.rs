//! The sticky note window: header actions and Markdown body.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
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
use crate::storage::{Note, NoteColor, Recurrence, Reminder, WindowGeometry};
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
    /// The user added or removed tags.
    pub on_tags_changed: Box<dyn Fn(Vec<String>)>,
    /// The user changed the per-note text scale (`None` = global).
    pub on_font_scale: Box<dyn Fn(Option<f32>)>,
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
    /// Per-window text-scale class and provider, swapped by
    /// [`NoteWindow::apply_font_scale`]. The class is unique per
    /// window so one note's scale never restyles another's.
    font_class: String,
    font_css: Rc<RefCell<Option<gtk4::CssProvider>>>,
    /// Global scale the per-note override falls back to. Updated
    /// live when Preferences changes it.
    font_base: Rc<Cell<f32>>,
    /// Global typeface, resolved from settings (empty follows the
    /// system monospace font). Updated live when Preferences
    /// changes it.
    font_family: Rc<RefCell<String>>,
    /// The reset button doubles as the scale indicator ("110%").
    font_reset: Button,
    /// Tag pill colors, shadowing the settings map — updated live
    /// through [`NoteWindow::set_tag_colors`].
    tag_colors: Rc<RefCell<HashMap<String, String>>>,
    /// Footer pills, rebuilt when tags or their colors change.
    tagbar: gtk4::Box,
    /// Plus button and inline entry for new tags; hidden at the cap.
    tag_add: Button,
    tag_entry: gtk4::Entry,
    /// The note's live model (tags and per-note scale are read here).
    shared: Rc<SharedNote>,
    /// App-core callbacks, reused when pills rebuild themselves.
    callbacks: Rc<NoteCallbacks>,
}

impl NoteWindow {
    /// Build a note window and wire it to the app-core callbacks.
    /// `tag_colors` seeds the pill colors from settings; `font_base`
    /// is the global text scale the note falls back to, and
    /// `font_family` the global typeface (empty follows the system
    /// monospace font).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app: &gtk4::Application,
        shared: Rc<SharedNote>,
        geometry: Option<WindowGeometry>,
        callbacks: NoteCallbacks,
        pin_backend: PinBackend,
        tag_colors: HashMap<String, String>,
        font_base: f32,
        font_family: String,
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

        // Restored (or default) size, re-applied after the pin setup
        // below: initializing the layer shell can reset the builder's
        // default size, leaving pinned notes at their condensed
        // minimum height instead of the saved one.
        let (init_width, init_height) = (
            geometry.map_or(300, |g| g.width),
            geometry.map_or(320, |g| g.height),
        );

        // Desktop pinning, two mechanisms in-app:
        // - an X11 desktop-type window (works under Ubuntu's mutter,
        //   whose layer-shell protocol is compiled out);
        // - the layer-shell protocol (KDE, wlroots compositors).
        let x11_desktop = pinned && pin_backend == PinBackend::X11;
        let x11_display = if x11_desktop { x11::display() } else { None };
        let window: gtk4::Window = if x11_desktop {
            let plain = gtk4::Window::builder()
                .title(shared.display_title())
                .default_width(init_width)
                .default_height(init_height)
                .build();
            if let Some(x11_display) = x11_display.as_ref() {
                plain.set_display(x11_display);
            }
            plain
        } else {
            gtk4::ApplicationWindow::builder()
                .application(app)
                .title(shared.display_title())
                .default_width(init_width)
                .default_height(init_height)
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
        if pinned {
            // Re-assert the restored size after the pin setup above:
            // the layer-shell (and X11 desktop) initialization can
            // drop the builder's default size, which otherwise leaves
            // the note at its condensed minimum height on login.
            window.set_default_size(init_width, init_height);
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

        // Per-window text class: unique per window (a static
        // counter) so one note's scale and typeface never restyle
        // another's. Created before the palette so swatch picks can
        // keep it when they restyle the window.
        static FONT_CLASS_NEXT: AtomicU32 = AtomicU32::new(1);
        let font_class = format!(
            "pinlet-font-{}",
            FONT_CLASS_NEXT.fetch_add(1, Ordering::Relaxed)
        );
        window.add_css_class(&font_class);

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
                let font_class = font_class.clone();
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
                        &font_class,
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

        // Trash, with confirmation. The note leaves the window and
        // every list, but stays restorable from the tray Trash menu.
        let delete_btn = Button::from_icon_name("user-trash-symbolic");
        delete_btn.set_tooltip_text(Some("Move to trash"));
        {
            let window = window.clone();
            let callbacks = callbacks.clone();
            delete_btn.connect_clicked(move |_| {
                let dialog = adw::MessageDialog::builder()
                    .heading("Move note to trash?")
                    .body("Restore it from the tray Trash menu, or empty the trash to delete it forever. Its history remains in git either way.")
                    .build();
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("trash", "Move to trash");
                dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_transient_for(Some(&window));
                let callbacks = callbacks.clone();
                dialog.connect_response(None, move |dialog, response| {
                    if response == "trash" {
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
        // Capture phase: the press must be claimed before the TextView's
        // own handlers place the cursor — otherwise the wholesale buffer
        // replacement below strands a selection from the note's start.
        let checkbox_click = gtk4::GestureClick::new();
        // Left button only: middle/right presses keep their default
        // behavior (paste, context menu).
        checkbox_click.set_button(gdk::BUTTON_PRIMARY);
        checkbox_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
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
                    // Collapse any selection the press left behind.
                    let buffer = preview_view.buffer();
                    buffer.place_cursor(&buffer.start_iter());
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

        let font_css = Rc::new(RefCell::new(None));
        let font_base = Rc::new(Cell::new(clamp_font_scale(font_base)));
        let font_family = Rc::new(RefCell::new(crate::fonts::resolve_font_family(
            &font_family,
        )));

        // Tag pills plus an inline entry, living in the footer and
        // rebuilt when tags or their colors change. Locked notes show
        // a placeholder, never metadata — no tags.
        let tag_colors = Rc::new(RefCell::new(tag_colors));
        let tagbar = gtk4::Box::new(Orientation::Horizontal, 4);
        tagbar.add_css_class("pinlet-tagbar");
        tagbar.set_hexpand(true);
        let tag_entry = gtk4::Entry::builder()
            .placeholder_text("Add tag…")
            .tooltip_text("Add a tag (Enter)")
            .max_length(Note::MAX_TAG_LEN as i32)
            .width_chars(12)
            .visible(false)
            .build();
        let tag_add = Button::from_icon_name("list-add-symbolic");
        tag_add.add_css_class("pinlet-tag-add");
        tag_add.set_tooltip_text(Some("Add tag"));
        if !locked {
            rebuild_tagbar(
                &tagbar,
                &tag_add,
                &tag_entry,
                &shared,
                &tag_colors,
                &callbacks,
            );
            {
                let entry = tag_entry.clone();
                tag_add.connect_clicked(move |_| {
                    use gtk4::prelude::WidgetExt;
                    let show = !WidgetExt::is_visible(&entry);
                    entry.set_visible(show);
                    if show {
                        entry.grab_focus();
                    }
                });
            }
            let entry = tag_entry.clone();
            let bar = tagbar.clone();
            let add = tag_add.clone();
            let shared = shared.clone();
            let colors = tag_colors.clone();
            let callbacks = callbacks.clone();
            tag_entry.connect_activate(move |_| {
                // A leading hash is decoration, not part of the name.
                let raw = entry.text().to_string();
                let raw = raw.trim_start_matches('#');
                let Some(tag) = Note::normalize_tag(raw) else {
                    return;
                };
                let mut tags = shared.note.borrow().tags.clone();
                if !tags.iter().any(|existing| existing == &tag) {
                    tags.push(tag);
                    (callbacks.on_tags_changed)(tags);
                    rebuild_tagbar(&bar, &add, &entry, &shared, &colors, &callbacks);
                }
                entry.set_text("");
            });
        }

        // Markdown toolbar (edit mode only) with text-size controls.
        let toolbar = gtk4::Box::new(Orientation::Horizontal, 2);
        toolbar.add_css_class("pinlet-toolbar");
        let font_reset = Button::builder()
            .tooltip_text("Reset to the global text size (Ctrl+0)")
            .build();
        if !locked {
            build_toolbar(
                &toolbar,
                &edit_view,
                &shared,
                &font_base,
                &font_reset,
                &callbacks,
            );
        }

        // Footer under the editor: tag pills plus the inline entry
        // on the left, word/character counts on the right.
        let footer = gtk4::Box::new(Orientation::Horizontal, 0);
        footer.add_css_class("pinlet-footer");
        footer.append(&tag_add);
        footer.append(&tag_entry);
        footer.append(&tagbar);
        let count_label = Label::builder().xalign(1.0).build();
        footer.append(&count_label);
        if !locked {
            let buffer = edit_view.buffer();
            update_counts(&buffer, &count_label);
            buffer.connect_changed(move |buffer| {
                update_counts(buffer, &count_label);
            });
        }

        // Toggle between the raw editor and the rendered preview.
        let stack = Stack::new();
        stack.add_named(&edit_view, Some("edit"));
        stack.add_named(&preview_view, Some("preview"));
        stack.set_visible_child(&edit_view);

        // Wire the eye toggle to the edit/preview views now that they exist.
        // The toolbar only makes sense while editing.
        {
            let edit_view = edit_view.clone();
            let preview_view = preview_view.clone();
            let stack = stack.clone();
            let styler = styler.clone();
            let toolbar = toolbar.clone();
            preview_toggle.connect_toggled(move |button| {
                if button.is_active() {
                    button.set_icon_name("view-reveal-symbolic");
                    button.set_tooltip_text(Some("Edit"));
                    refresh_preview(&edit_view, &preview_view, &styler);
                    stack.set_visible_child(&preview_view);
                    toolbar.set_visible(false);
                } else {
                    button.set_icon_name("view-conceal-symbolic");
                    button.set_tooltip_text(Some("Preview"));
                    stack.set_visible_child(&edit_view);
                    toolbar.set_visible(true);
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

            // Clicking a task marker in the editor flips it in place
            // (one undo step), mirroring the preview checkboxes.
            // Capture phase, like the preview gesture: claim the press
            // before the TextView places the cursor or starts a drag.
            let edit_checkbox = gtk4::GestureClick::new();
            edit_checkbox.set_button(gdk::BUTTON_PRIMARY);
            edit_checkbox.set_propagation_phase(gtk4::PropagationPhase::Capture);
            {
                let view = edit_view.clone();
                let buffer = edit_view.buffer();
                edit_checkbox.connect_pressed(move |gesture, n_press, x, y| {
                    if n_press != 1 {
                        return;
                    }
                    let Some((bx, by)) = buffer_coords(&view, x, y) else {
                        return;
                    };
                    let Some(iter) = view.iter_at_location(bx, by) else {
                        return;
                    };
                    if toggle_editor_checkbox(&buffer, iter.line(), iter.line_offset()) {
                        gesture.set_state(gtk4::EventSequenceState::Claimed);
                    }
                });
            }
            edit_view.add_controller(edit_checkbox);

            wire_list_continuation(&edit_view.buffer());
        }

        let scroller = ScrolledWindow::builder()
            .child(&stack)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vexpand(true)
            .build();

        let content = gtk4::Box::new(Orientation::Vertical, 0);
        if !locked {
            content.append(&toolbar);
        }
        content.append(&scroller);
        if !locked {
            content.append(&footer);
        }

        window.set_titlebar(Some(&header));
        window.set_child(Some(&content));

        // Geometry: notify the app core (debounced there) whenever
        // the window is resized — but only once mapped. Allocations
        // before the first map are transient (often zero/condensed),
        // and reporting them would clobber the restored size, which
        // is exactly the squashed-pins-on-login report.
        let mapped = Rc::new(Cell::new(false));
        {
            let mapped = mapped.clone();
            window.connect_map(move |_| {
                mapped.set(true);
            });
        }
        {
            let width_cb = callbacks.clone();
            let width_mapped = mapped.clone();
            window.connect_notify_local(Some("width"), move |_, _| {
                if width_mapped.get() {
                    (width_cb.on_geometry_changed)();
                }
            });
            let height_cb = callbacks.clone();
            let height_mapped = mapped.clone();
            window.connect_notify_local(Some("height"), move |_, _| {
                if height_mapped.get() {
                    (height_cb.on_geometry_changed)();
                }
            });
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
        // Ctrl+plus/minus/0 adjust the per-note text size the same way
        // the toolbar buttons do (locked notes have no toolbar, so no
        // shortcuts either).
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
            if !locked {
                for (keys, step) in [("<Control>plus", 0.1f32), ("<Control>minus", -0.1f32)] {
                    if let Some(trigger) = gtk4::ShortcutTrigger::parse_string(keys) {
                        let shared = shared.clone();
                        let font_base = font_base.clone();
                        let callbacks = callbacks.clone();
                        esc.add_shortcut(gtk4::Shortcut::new(
                            Some(trigger),
                            Some(gtk4::CallbackAction::new(move |_, _| {
                                nudge_font_scale(&shared, &font_base, &callbacks, step);
                                glib::Propagation::Stop
                            })),
                        ));
                    }
                }
                if let Some(trigger) = gtk4::ShortcutTrigger::parse_string("<Control>0") {
                    let callbacks = callbacks.clone();
                    esc.add_shortcut(gtk4::Shortcut::new(
                        Some(trigger),
                        Some(gtk4::CallbackAction::new(move |_, _| {
                            (callbacks.on_font_scale)(None);
                            glib::Propagation::Stop
                        })),
                    ));
                }
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
            font_class,
            font_css,
            font_base,
            font_family,
            font_reset,
            tag_colors,
            tagbar,
            tag_add,
            tag_entry,
            shared,
            callbacks,
        };
        // Initial text scale: the note's override, else the global.
        this.apply_font_scale(this.effective_scale());
        // Route through the shared helper so named and custom hex colors
        // share one path (custom colors need a dynamic provider).
        apply_color_to(
            &this.window,
            initial_display,
            &this.custom_css,
            &color,
            &this.font_class,
        );
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
            &self.font_class,
        );
    }

    /// Text scale actually in force: the note's override, if any,
    /// else the global scale.
    pub fn effective_scale(&self) -> f32 {
        clamp_font_scale(
            self.shared
                .note
                .borrow()
                .font_scale
                .unwrap_or_else(|| self.font_base.get()),
        )
    }

    /// Restyle the window's text at `scale`, swapping the previous
    /// provider. The toolbar reset button doubles as the indicator.
    pub fn apply_font_scale(&self, scale: f32) {
        let scale = clamp_font_scale(scale);
        swap_font_provider(
            &self.window,
            self.x11_desktop,
            &self.font_css,
            &self.font_class,
            scale,
            &self.font_family.borrow(),
        );
        self.font_reset
            .set_label(&format!("{}%", (scale * 100.0).round() as i32));
    }

    /// The global scale changed: remember it and re-derive this
    /// note's effective scale.
    pub fn set_base_scale(&self, base: f32) {
        self.font_base.set(clamp_font_scale(base));
        self.apply_font_scale(self.effective_scale());
    }

    /// The global typeface changed: remember it (empty follows the
    /// system monospace font) and restyle this note.
    pub fn set_font_family(&self, family: &str) {
        *self.font_family.borrow_mut() = crate::fonts::resolve_font_family(family);
        self.apply_font_scale(self.effective_scale());
    }

    /// The settings tag-color map changed: shadow it and repaint pills.
    pub fn set_tag_colors(&self, colors: &HashMap<String, String>) {
        *self.tag_colors.borrow_mut() = colors.clone();
        rebuild_tagbar(
            &self.tagbar,
            &self.tag_add,
            &self.tag_entry,
            &self.shared,
            &self.tag_colors,
            &self.callbacks,
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
/// provider (the palette class alone still applies). `set_css_classes`
/// replaces the whole class list, so the per-window font class is
/// restored right after — without it the size/typeface rule stops
/// matching and the note loses both.
fn apply_color_to(
    window: &gtk4::Window,
    display: Option<gdk::Display>,
    slot: &RefCell<Option<gtk4::CssProvider>>,
    color: &NoteColor,
    font_class: &str,
) {
    window.set_css_classes(&[color.css_class()]);
    window.add_css_class(font_class);
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

/// Clamp a text scale into the supported range; garbage (NaN,
/// infinite, hand-edited settings) falls back to 1.0.
pub fn clamp_font_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(0.5, 3.0)
    } else {
        1.0
    }
}

/// The CSS restyling one note's text: family AND size live on the
/// `textview` widget node, where the text layout picks them up
/// (GTK's own `.monospace { font-family: monospace; }` rule works
/// the same way). Size is repeated on the inner `text` node so its
/// specified value stays in step.
pub(crate) fn font_css_rule(class: &str, scale: f32, family: &str) -> String {
    format!(
        ".{class} textview.pinlet-body {{ font-family: '{}', monospace; font-size: {:.1}pt; }}\n.{class} textview.pinlet-body text {{ font-size: {:.1}pt; }}",
        crate::fonts::css_escape_family(family),
        14.0 * scale,
        14.0 * scale,
    )
}

/// Swap the window's text provider for one rendering `scale` in
/// `family` on its unique font class (specificity beats the shared
/// stylesheet's body rule without touching other windows). A generic
/// `monospace` fallback keeps the rule sane when the family is
/// later uninstalled.
fn swap_font_provider(
    window: &gtk4::Window,
    x11_desktop: bool,
    slot: &RefCell<Option<gtk4::CssProvider>>,
    class: &str,
    scale: f32,
    family: &str,
) {
    if let Some(old) = slot.borrow_mut().take() {
        if let Some(display) = color_display(window, x11_desktop) {
            gtk4::style_context_remove_provider_for_display(&display, &old);
        }
    }
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&font_css_rule(class, scale, family));
    if let Some(display) = color_display(window, x11_desktop) {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    *slot.borrow_mut() = Some(provider);
}

/// Step the effective scale by `delta` and store it as the note's
/// override. Tenths keep repeated presses exact. Shared by the
/// toolbar buttons and the keyboard shortcuts.
fn nudge_font_scale(
    shared: &Rc<SharedNote>,
    base: &Rc<Cell<f32>>,
    callbacks: &Rc<NoteCallbacks>,
    delta: f32,
) {
    let current = shared
        .note
        .borrow()
        .font_scale
        .unwrap_or_else(|| base.get());
    let stepped = ((current * 10.0).round() + delta * 10.0).round() / 10.0;
    (callbacks.on_font_scale)(Some(clamp_font_scale(stepped)));
}

/// Build the Markdown toolbar: format buttons, then text-size controls.
fn build_toolbar(
    toolbar: &gtk4::Box,
    edit_view: &TextView,
    shared: &Rc<SharedNote>,
    base: &Rc<Cell<f32>>,
    reset: &Button,
    callbacks: &Rc<NoteCallbacks>,
) {
    let buffer = edit_view.buffer();
    let wrap = |label: &str, tooltip: &str, pre: &'static str, suf: &'static str| {
        let button = Button::with_label(label);
        button.set_tooltip_text(Some(tooltip));
        let buffer = buffer.clone();
        button.connect_clicked(move |_| wrap_selection(&buffer, pre, suf));
        toolbar.append(&button);
    };
    wrap("B", "Bold", "**", "**");
    wrap("I", "Italic", "*", "*");
    wrap("S", "Strikethrough", "~~", "~~");
    wrap("🔗", "Link", "[", "](https://)");

    let prefix = |label: &str, tooltip: &str, marker: &'static str| {
        let button = Button::with_label(label);
        button.set_tooltip_text(Some(tooltip));
        let buffer = buffer.clone();
        button.connect_clicked(move |_| {
            replace_lines(&buffer, |text| toggle_prefix_all(text, marker));
        });
        toolbar.append(&button);
    };
    prefix("H", "Heading", "## ");
    prefix(">", "Quote", "> ");
    prefix("•", "Bullet list", "- ");

    // Code: backticks for one line, a fence for several.
    {
        let code = Button::with_label("</>");
        code.set_tooltip_text(Some("Code"));
        let buffer = buffer.clone();
        code.connect_clicked(move |_| {
            let multiline = buffer
                .selection_bounds()
                .is_some_and(|(start, end)| start.line() != end.line());
            if multiline {
                wrap_selection(&buffer, "```\n", "\n```");
            } else {
                wrap_selection(&buffer, "`", "`");
            }
        });
        toolbar.append(&code);
    }

    // Task list: toggle the covered lines between task and plain.
    {
        let task = Button::with_label("☑");
        task.set_tooltip_text(Some("Checklist item"));
        let buffer = buffer.clone();
        task.connect_clicked(move |_| {
            replace_lines(&buffer, toggle_task_all);
        });
        toolbar.append(&task);
    }

    toolbar.append(&gtk4::Separator::new(Orientation::Vertical));

    let spacer = gtk4::Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    toolbar.append(&spacer);

    // Per-note text size: smaller, reset-to-global (doubling as the
    // current-scale indicator), larger.
    {
        let smaller = Button::with_label("A−");
        smaller.set_tooltip_text(Some("Smaller text (Ctrl+-)"));
        let shared = shared.clone();
        let base = base.clone();
        let callbacks = callbacks.clone();
        smaller.connect_clicked(move |_| nudge_font_scale(&shared, &base, &callbacks, -0.1));
        toolbar.append(&smaller);
    }
    {
        let callbacks = callbacks.clone();
        reset.connect_clicked(move |_| (callbacks.on_font_scale)(None));
        toolbar.append(reset);
    }
    {
        let larger = Button::with_label("A+");
        larger.set_tooltip_text(Some("Larger text (Ctrl++)"));
        let shared = shared.clone();
        let base = base.clone();
        let callbacks = callbacks.clone();
        larger.connect_clicked(move |_| nudge_font_scale(&shared, &base, &callbacks, 0.1));
        toolbar.append(&larger);
    }
}

/// Wrap the selection (or an empty cursor spot) in `pre`/`suf` in one
/// undo step. With no selection the cursor lands between the two.
fn wrap_selection(buffer: &TextBuffer, pre: &str, suf: &str) {
    buffer.begin_user_action();
    if let Some((mut start, mut end)) = buffer.selection_bounds() {
        let selected = buffer.text(&start, &end, true).to_string();
        buffer.delete(&mut start, &mut end);
        buffer.insert(&mut start, &format!("{pre}{selected}{suf}"));
    } else {
        let mut at = buffer.iter_at_mark(&buffer.get_insert());
        buffer.insert(&mut at, &format!("{pre}{suf}"));
        at.backward_chars(suf.chars().count() as i32);
        buffer.place_cursor(&at);
    }
    buffer.end_user_action();
}

/// Apply `f` to the full lines covered by the selection (or the
/// cursor line), replacing them in one undo step. A selection ending
/// exactly at a line start excludes that line.
fn replace_lines(buffer: &TextBuffer, f: impl FnOnce(&str) -> String) {
    let (start, end) = match buffer.selection_bounds() {
        Some((start, end)) => (start, end),
        None => {
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            (cursor, cursor)
        }
    };
    let mut from = start;
    from.set_line_offset(0);
    let mut to = end;
    if to.line() != from.line() && to.starts_line() {
        to.backward_char();
    }
    to.forward_to_line_end();
    let text = buffer.text(&from, &to, true).to_string();
    buffer.begin_user_action();
    buffer.delete(&mut from, &mut to);
    buffer.insert(&mut from, &f(&text));
    buffer.end_user_action();
}

/// Toggle `prefix` on every non-empty line: strip it when all carry
/// it, otherwise add it to the lines missing it.
fn toggle_prefix_all(text: &str, prefix: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let targets: Vec<&&str> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if targets.is_empty() {
        return text.to_owned();
    }
    if targets.iter().all(|line| line.starts_with(prefix)) {
        lines
            .iter()
            .map(|line| line.strip_prefix(prefix).unwrap_or(line).to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        lines
            .iter()
            .map(|line| {
                if line.trim().is_empty() || line.starts_with(prefix) {
                    (*line).to_owned()
                } else {
                    format!("{prefix}{line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Toggle one source line between task item and plain line: a task
/// marker is stripped (keeping the bullet), a bare bullet gains
/// `[ ]`, anything else becomes a `- [ ]` task.
fn toggle_task_line(line: &str) -> String {
    if let Some((byte, _)) = find_checkbox(line) {
        let mut out = line.to_owned();
        let take = if line[byte + 3..].starts_with(' ') {
            4
        } else {
            3
        };
        out.replace_range(byte..byte + take, "");
        out
    } else {
        let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
        let (indent, rest) = line.split_at(indent_len);
        if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
            format!("{indent}{}[ ] {}", &rest[..2], &rest[2..])
        } else {
            format!("{indent}- [ ] {rest}")
        }
    }
}

/// Toggle task state across lines, mirroring [`toggle_prefix_all`]:
/// strip when every non-empty line is a task, otherwise complete the
/// lines missing a marker.
fn toggle_task_all(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let targets: Vec<&&str> = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if targets.is_empty() {
        return text.to_owned();
    }
    if targets.iter().all(|line| find_checkbox(line).is_some()) {
        lines
            .iter()
            .map(|line| toggle_task_line(line))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        lines
            .iter()
            .map(|line| {
                if line.trim().is_empty() || find_checkbox(line).is_some() {
                    (*line).to_owned()
                } else {
                    toggle_task_line(line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
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

/// Words and characters in `text` for the footer.
fn count_words(text: &str) -> (usize, usize) {
    (text.split_whitespace().count(), text.chars().count())
}

/// Refresh the footer label from the buffer's current text.
fn update_counts(buffer: &TextBuffer, label: &Label) {
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string();
    let (words, chars) = count_words(&text);
    label.set_text(&format!("{words} words · {chars} characters"));
}

/// Rebuild the footer pills from the note's tags. The plus button
/// and inline entry are separate and survive rebuilds, so typing
/// focus is never lost; both hide once the note carries
/// [`Note::MAX_TAGS`] tags.
fn rebuild_tagbar(
    tagbar: &gtk4::Box,
    tag_add: &Button,
    tag_entry: &gtk4::Entry,
    shared: &Rc<SharedNote>,
    colors: &Rc<RefCell<HashMap<String, String>>>,
    callbacks: &Rc<NoteCallbacks>,
) {
    while let Some(child) = tagbar.first_child() {
        tagbar.remove(&child);
    }
    let tags = shared.note.borrow().tags.clone();
    for tag in &tags {
        tagbar.append(&tag_pill(
            tag, tagbar, tag_add, tag_entry, shared, colors, callbacks,
        ));
    }
    let capped = tags.len() >= Note::MAX_TAGS;
    tag_add.set_visible(!capped);
    if capped {
        tag_entry.set_text("");
        tag_entry.set_visible(false);
    }
}

/// One tag pill: `#name` opens its color swatches, `×` removes it.
fn tag_pill(
    tag: &str,
    tagbar: &gtk4::Box,
    tag_add: &Button,
    tag_entry: &gtk4::Entry,
    shared: &Rc<SharedNote>,
    colors: &Rc<RefCell<HashMap<String, String>>>,
    callbacks: &Rc<NoteCallbacks>,
) -> gtk4::Box {
    let pill = gtk4::Box::new(Orientation::Horizontal, 0);
    pill.add_css_class("pinlet-tag");
    let color = colors
        .borrow()
        .get(tag)
        .cloned()
        .unwrap_or_else(|| colors::default_tag_color(tag).to_owned());
    pill.add_css_class(&colors::tag_css_class(&color));
    let name = Label::new(Some(&format!("#{tag}")));
    pill.append(&name);

    let remove = Button::with_label("×");
    remove.add_css_class("pinlet-tag-x");
    remove.set_tooltip_text(Some("Remove tag"));
    {
        let tag = tag.to_owned();
        let bar = tagbar.clone();
        let add = tag_add.clone();
        let entry = tag_entry.clone();
        let shared = shared.clone();
        let colors = colors.clone();
        let callbacks = callbacks.clone();
        remove.connect_clicked(move |_| {
            let tags: Vec<String> = shared
                .note
                .borrow()
                .tags
                .iter()
                .filter(|existing| *existing != &tag)
                .cloned()
                .collect();
            (callbacks.on_tags_changed)(tags);
            rebuild_tagbar(&bar, &add, &entry, &shared, &colors, &callbacks);
        });
    }
    pill.append(&remove);
    pill
}

/// Flip the task marker on edit-buffer `line` when `offset` sits on
/// (or next to) it — the editor mirror of the preview click. One
/// undo step; false when no marker is there.
fn toggle_editor_checkbox(buffer: &TextBuffer, line: i32, offset: i32) -> bool {
    let Some(line_start) = buffer.iter_at_line(line) else {
        return false;
    };
    let mut line_end = line_start;
    line_end.forward_to_line_end();
    let text = buffer.text(&line_start, &line_end, true).to_string();
    let Some((byte, _)) = find_checkbox(&text) else {
        return false;
    };
    let col = text[..byte].chars().count() as i32;
    if offset.abs_diff(col) > 1 {
        return false;
    }
    let checked = text.as_bytes()[byte + 1] != b' ';
    let replacement = if checked { "[ ]" } else { "[x]" };
    let mut from = line_start;
    from.forward_chars(col);
    let mut to = from;
    to.forward_chars(3);
    buffer.begin_user_action();
    buffer.delete(&mut from, &mut to);
    buffer.insert(&mut from, replacement);
    buffer.end_user_action();
    true
}

/// What Enter does on the split list-item line: continue it, remove
/// an empty item, or nothing for plain lines.
enum EnterAction {
    /// Insert this prefix on the new line.
    Continue(String),
    /// The item is empty: remove its whole line.
    RemoveItem,
    /// Not a continued list: leave the newline alone.
    Nothing,
}

/// An ordered-list bullet (`12.` / `3)` + blank): its number, the
/// delimiter, and the bullet length through the delimiter.
fn ordered_marker(rest: &str) -> Option<(u64, char, usize)> {
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    let delim = *rest
        .as_bytes()
        .get(digits)
        .filter(|byte| **byte == b'.' || **byte == b')')?;
    if !rest[digits + 1..].starts_with([' ', '\t']) {
        return None;
    }
    let number: u64 = rest[..digits].parse().ok()?;
    Some((number, delim as char, digits + 1))
}

/// Classify the line Enter just split. Ordered items continue with
/// the next number — following lines keep theirs; only the fresh
/// line is numbered.
fn enter_action(line: &str) -> EnterAction {
    let indent_len = line.len() - line.trim_start_matches([' ', '\t']).len();
    let (indent, rest) = line.split_at(indent_len);
    if let Some((number, delim, through_delim)) = ordered_marker(rest) {
        let next = number.saturating_add(1);
        let bytes = line.as_bytes();
        let mut content_start = indent_len + through_delim;
        while bytes
            .get(content_start)
            .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
        {
            content_start += 1;
        }
        if is_checkbox_at(bytes, content_start) {
            if line[content_start + 3..].trim().is_empty() {
                return EnterAction::RemoveItem;
            }
            return EnterAction::Continue(format!("{indent}{next}{delim} [ ] "));
        }
        if line[content_start..].trim().is_empty() {
            return EnterAction::RemoveItem;
        }
        return EnterAction::Continue(format!("{indent}{next}{delim} "));
    }
    if let Some((byte, _)) = find_checkbox(line) {
        let content = line[byte + 3..].trim();
        if content.is_empty() {
            return EnterAction::RemoveItem;
        }
        let mut prefix = line[..byte].to_owned();
        prefix.push_str("[ ] ");
        return EnterAction::Continue(prefix);
    }
    if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        if rest[2..].trim().is_empty() {
            return EnterAction::RemoveItem;
        }
        return EnterAction::Continue(format!("{indent}{} ", &rest[..1]));
    }
    EnterAction::Nothing
}

/// Wire Enter continuation on an edit buffer: Enter on a task,
/// bullet, or ordered item continues it on the next line; Enter on
/// an empty item removes the item instead. The continuation runs on
/// idle, not inside the emission: mutating the buffer synchronously
/// invalidates iterators GTK still holds for the in-flight keypress
/// ("Invalid text buffer iterator"). The position crosses into the
/// idle in a mark, resolved back to a line number there. `pub(crate)`
/// so the regression scenario can drive the real wiring headlessly.
pub(crate) fn wire_list_continuation(buffer: &TextBuffer) {
    buffer.connect_insert_text(move |buffer, _location, text| {
        if text != "\n" {
            return;
        }
        // Right gravity: the mark rides past the newline to the
        // fresh line's start.
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let mark = buffer.create_mark(None, &cursor, false);
        let cont = buffer.clone();
        glib::idle_add_local_once(move || {
            let at = cont.iter_at_mark(&mark);
            cont.delete_mark(&mark);
            continue_list_at(&cont, at.line());
        });
    });
}

/// Continue (or collapse) the list item above `line`, the fresh line
/// Enter opened. Positions are re-derived from line numbers, never
/// carried across edits as iterators.
fn continue_list_at(buffer: &TextBuffer, line: i32) {
    if line <= 0 {
        return;
    }
    let Some(item_start) = buffer.iter_at_line(line - 1) else {
        return;
    };
    let mut item_end = item_start;
    item_end.forward_to_line_end();
    let text = buffer.text(&item_start, &item_end, true).to_string();
    let Some(fresh_start) = buffer.iter_at_line(line) else {
        return;
    };
    match enter_action(&text) {
        EnterAction::Nothing => {}
        EnterAction::Continue(prefix) => {
            buffer.begin_user_action();
            let mut at = fresh_start;
            buffer.insert(&mut at, &prefix);
            buffer.end_user_action();
        }
        EnterAction::RemoveItem => {
            // Delete the empty item's line plus the newline after it.
            buffer.begin_user_action();
            let mut from = item_start;
            let mut to = fresh_start;
            buffer.delete(&mut from, &mut to);
            buffer.end_user_action();
        }
    }
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
    use super::{EnterAction, find_checkbox, font_css_rule};

    /// The family must land on the `textview` widget node, where the
    /// text layout picks it up (GTK's own `.monospace` rule works the
    /// same way); a family declared only on the inner `text` node
    /// never reached the rendered text.
    #[test]
    fn font_rule_targets_textview_node() {
        assert_eq!(
            font_css_rule("pinlet-font-7", 1.0, "Caveat"),
            ".pinlet-font-7 textview.pinlet-body { font-family: 'Caveat', monospace; font-size: 14.0pt; }\n\
             .pinlet-font-7 textview.pinlet-body text { font-size: 14.0pt; }"
        );
    }

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

    #[test]
    fn prefix_toggle_adds_and_strips() {
        assert_eq!(super::toggle_prefix_all("a\nb", "## "), "## a\n## b");
        assert_eq!(super::toggle_prefix_all("## a\n## b", "## "), "a\nb");
        // Mixed: complete the missing lines, keep empty ones bare.
        assert_eq!(
            super::toggle_prefix_all("## a\nb\n\nc", "## "),
            "## a\n## b\n\n## c"
        );
        assert_eq!(super::toggle_prefix_all("", "## "), "");
    }

    #[test]
    fn task_toggle_converts_lines() {
        assert_eq!(super::toggle_task_line("- [ ] foo"), "- foo");
        assert_eq!(super::toggle_task_line("- [x] foo"), "- foo");
        assert_eq!(super::toggle_task_line("- foo"), "- [ ] foo");
        assert_eq!(super::toggle_task_line("plain"), "- [ ] plain");
        assert_eq!(super::toggle_task_line("  * [X] star"), "  * star");
        // All tasks: strip; otherwise complete.
        assert_eq!(super::toggle_task_all("- [ ] a\n- [x] b"), "- a\n- b");
        assert_eq!(super::toggle_task_all("- [ ] a\n- b"), "- [ ] a\n- [ ] b");
    }

    #[test]
    fn enter_continues_or_collapses_lists() {
        assert!(matches!(
            super::enter_action("- [ ] buy milk"),
            EnterAction::Continue(prefix) if prefix == "- [ ] "
        ));
        assert!(matches!(
            super::enter_action("  * [x] done"),
            EnterAction::Continue(prefix) if prefix == "  * [ ] "
        ));
        assert!(matches!(
            super::enter_action("- just a bullet"),
            EnterAction::Continue(prefix) if prefix == "- "
        ));
        assert!(matches!(
            super::enter_action("- [ ]"),
            EnterAction::RemoveItem
        ));
        assert!(matches!(super::enter_action("- "), EnterAction::RemoveItem));
        assert!(matches!(
            super::enter_action("plain text"),
            EnterAction::Nothing
        ));
        // Ordered items continue with the next number, keeping the
        // delimiter style; empty ones collapse like bullets.
        assert!(matches!(
            super::enter_action("1. first"),
            EnterAction::Continue(prefix) if prefix == "2. "
        ));
        assert!(matches!(
            super::enter_action("  12) second"),
            EnterAction::Continue(prefix) if prefix == "  13) "
        ));
        assert!(matches!(
            super::enter_action("2) [ ] task"),
            EnterAction::Continue(prefix) if prefix == "3) [ ] "
        ));
        assert!(matches!(
            super::enter_action("1. "),
            EnterAction::RemoveItem
        ));
        assert!(matches!(
            super::enter_action("1. [ ]"),
            EnterAction::RemoveItem
        ));
        // Not a list without a blank after the marker; unparseable
        // numbers stay untouched.
        assert!(matches!(super::enter_action("1.foo"), EnterAction::Nothing));
        assert!(matches!(
            super::enter_action("99999999999999999999999. big"),
            EnterAction::Nothing
        ));
    }

    #[test]
    fn counts_split_words_and_chars() {
        assert_eq!(super::count_words(""), (0, 0));
        assert_eq!(super::count_words("hello world"), (2, 11));
        assert_eq!(super::count_words("  a\nb  "), (2, 7));
    }

    #[test]
    fn font_scale_clamps_and_rejects_garbage() {
        assert_eq!(super::clamp_font_scale(1.2), 1.2);
        assert_eq!(super::clamp_font_scale(0.1), 0.5);
        assert_eq!(super::clamp_font_scale(9.0), 3.0);
        assert_eq!(super::clamp_font_scale(f32::NAN), 1.0);
        assert_eq!(super::clamp_font_scale(f32::INFINITY), 1.0);
    }
}
