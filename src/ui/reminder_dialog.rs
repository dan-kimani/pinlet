//! Date & time picker for adding a note reminder (spec §3.4).

use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Local, TimeZone, Timelike, Utc};
use gtk4::{Align, Box as GtkBox, Button, Calendar, DropDown, Label, Orientation, SpinButton};

use crate::storage::Recurrence;

/// Show the reminder picker: a calendar, hour/minute spinners, and a
/// repetition choice. `on_add` receives the chosen time in UTC plus
/// its recurrence rule.
pub fn present(window: &gtk4::Window, on_add: impl Fn(DateTime<Utc>, Recurrence) + 'static) {
    let calendar = Calendar::new();
    let hour = SpinButton::with_range(0.0, 23.0, 1.0);
    let minute = SpinButton::with_range(0.0, 59.0, 1.0);
    let now = Local::now();
    hour.set_value(f64::from(now.hour()));
    minute.set_value(f64::from(now.minute()));

    let time_row = GtkBox::new(Orientation::Horizontal, 8);
    time_row.append(&hour);
    time_row.append(&Label::new(Some(":")));
    time_row.append(&minute);
    time_row.set_halign(Align::Center);

    let repeats = DropDown::from_strings(&["Just once", "Daily", "Weekly", "Weekdays", "Custom…"]);
    let custom_days = SpinButton::with_range(1.0, 365.0, 1.0);
    custom_days.set_value(7.0);
    custom_days.set_sensitive(false);
    {
        let custom_days = custom_days.clone();
        repeats.connect_selected_notify(move |drop| {
            custom_days.set_sensitive(drop.selected() == 4);
        });
    }
    let repeats_row = GtkBox::new(Orientation::Horizontal, 8);
    repeats_row.append(&Label::new(Some("Repeats")));
    repeats_row.append(&repeats);
    repeats_row.set_halign(Align::Center);
    let custom_row = GtkBox::new(Orientation::Horizontal, 8);
    custom_row.append(&Label::new(Some("Every")));
    custom_row.append(&custom_days);
    custom_row.append(&Label::new(Some("days")));
    custom_row.set_halign(Align::Center);

    let add_btn = Button::builder()
        .label("Add")
        .css_classes(["suggested-action"])
        .build();
    let cancel_btn = Button::builder().label("Cancel").build();

    let buttons = GtkBox::new(Orientation::Horizontal, 8);
    buttons.set_halign(Align::End);
    buttons.append(&cancel_btn);
    buttons.append(&add_btn);

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.append(&calendar);
    content.append(&time_row);
    content.append(&repeats_row);
    content.append(&custom_row);
    content.append(&buttons);

    let dialog = adw::Dialog::builder()
        .title("Reminder")
        .child(&content)
        .content_width(360)
        .build();

    let on_add = Rc::new(on_add);
    {
        let dialog = dialog.clone();
        cancel_btn.connect_clicked(move |_| {
            dialog.close();
        });
    }
    {
        let dialog = dialog.clone();
        let on_add = on_add.clone();
        let calendar = calendar.clone();
        let hour = hour.clone();
        let minute = minute.clone();
        add_btn.connect_clicked(move |_| {
            // `picked_time` is `None` on nonexistent local times (a DST
            // gap): keep the dialog open so the failure is visible
            // instead of silently dropping the reminder.
            if let Some(due) = picked_time(&calendar, hour.value(), minute.value()) {
                (on_add)(due, picked_recurrence(&repeats, custom_days.value()));
                dialog.close();
            }
        });
    }

    dialog.present(Some(window));
}

/// Map the repetition widgets to a recurrence rule.
fn picked_recurrence(repeats: &DropDown, custom_days: f64) -> Recurrence {
    match repeats.selected() {
        1 => Recurrence::Daily,
        2 => Recurrence::Weekly,
        3 => Recurrence::Weekdays,
        4 => Recurrence::Custom(custom_days as u32),
        _ => Recurrence::None,
    }
}

/// Combine the calendar date and spinners into a UTC instant.
fn picked_time(calendar: &Calendar, hour: f64, minute: f64) -> Option<DateTime<Utc>> {
    let date = calendar.date();
    Local
        .with_ymd_and_hms(
            date.year(),
            date.month() as u32,
            date.day_of_month() as u32,
            hour as u32,
            minute as u32,
            0,
        )
        .single()
        .map(|local| local.with_timezone(&Utc))
}
