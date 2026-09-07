//! Password prompt for unlocking notes (spec §3.10).

use std::rc::Rc;

use adw::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Orientation, PasswordEntry};

/// Present the unlock password dialog. `on_password` receives the
/// entered password.
pub fn present(window: &gtk4::Window, on_password: impl Fn(String) + 'static) {
    let first = PasswordEntry::builder()
        .show_peek_icon(true)
        .hexpand(true)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 8);
    content.append(&Label::builder().label("Password").xalign(0.0).build());
    content.append(&first);

    let ok_btn = Button::builder()
        .label("Unlock")
        .css_classes(["suggested-action"])
        .build();
    let cancel_btn = Button::builder().label("Cancel").build();
    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_halign(Align::End);
    buttons.append(&cancel_btn);
    buttons.append(&ok_btn);
    content.append(&buttons);

    let dialog = adw::Dialog::builder()
        .title("Unlock note")
        .child(&content)
        .content_width(320)
        .build();

    let on_password = Rc::new(on_password);
    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }
    // One submit path for the Unlock button and the Enter key
    // (without the latter the dialog can only be confirmed by click).
    let submit: Rc<dyn Fn()> = Rc::new({
        let dialog = dialog.clone();
        let on_password = on_password.clone();
        let first = first.clone();
        move || {
            let password = first.text().to_string();
            if password.is_empty() {
                first.add_css_class("error");
                first.grab_focus();
                return;
            }
            (on_password)(password);
            dialog.close();
        }
    });
    {
        let submit = submit.clone();
        ok_btn.connect_clicked(move |_| submit());
    }
    {
        let submit = submit.clone();
        first.connect_activate(move |_| submit());
    }

    dialog.present(Some(window));
}
