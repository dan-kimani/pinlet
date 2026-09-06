//! Date & time picker for adding a note reminder (spec §3.4).

use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Local, TimeZone, Timelike, Utc};
use gtk4::{Align, Box as GtkBox, Button, Calendar, Label, Orientation, SpinButton};

/// Show the reminder picker: a calendar plus hour/minute spinners.
/// `on_add` receives the chosen time in UTC.
pub fn present(window: &gtk4::Window, on_add: impl Fn(DateTime<Utc>) + 'static) {
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
            if let Some(due) = picked_time(&calendar, hour.value(), minute.value()) {
                (on_add)(due);
            }
            dialog.close();
        });
    }

    dialog.present(Some(window));
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
