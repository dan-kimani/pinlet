//! Password prompts for locking and unlocking notes (spec §3.10).

use std::rc::Rc;

use adw::prelude::*;
use gtk4::{Align, Box as GtkBox, Button, Label, Orientation, PasswordEntry};

/// Present a password dialog. With `confirm`, the password must be
/// entered twice (used when creating a lock); `on_password` receives
/// the entered password.
pub fn present(
    window: &gtk4::Window,
    confirm: bool,
    on_password: impl Fn(String) + 'static,
) {
    let first = PasswordEntry::builder()
        .show_peek_icon(true)
        .hexpand(true)
        .build();
    let second = PasswordEntry::builder()
        .show_peek_icon(true)
        .hexpand(true)
        .build();

    let content = GtkBox::new(Orientation::Vertical, 8);
    content.append(&Label::builder().label("Password").xalign(0.0).build());
    content.append(&first);
    if confirm {
        content.append(&Label::builder().label("Repeat password").xalign(0.0).build());
        content.append(&second);
    }

    let ok_btn = Button::builder()
        .label(if confirm { "Lock" } else { "Unlock" })
        .css_classes(["suggested-action"])
        .build();
    let cancel_btn = Button::builder().label("Cancel").build();
    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_halign(Align::End);
    buttons.append(&cancel_btn);
    buttons.append(&ok_btn);
    content.append(&buttons);

    let dialog = adw::Dialog::builder()
        .title(if confirm { "Lock note" } else { "Unlock note" })
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
    {
        let dialog = dialog.clone();
        let on_password = on_password.clone();
        let first = first.clone();
        let second = second.clone();
        ok_btn.connect_clicked(move |_| {
            let password = first.text().to_string();
            if confirm && (password.is_empty() || password != second.text()) {
                second.add_css_class("error");
                second.grab_focus();
                return;
            }
            if password.is_empty() {
                first.add_css_class("error");
                first.grab_focus();
                return;
            }
            (on_password)(password);
            dialog.close();
        });
    }

    dialog.present(Some(window));
}
