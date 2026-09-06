//! Glib timer helpers.

use gtk4::glib::{self, SourceId};

/// Cancel a timer source if it is still pending.
///
/// `SourceId::remove` panics on sources that already fired
/// (`timeout_add_local_once` sources remove themselves after
/// firing), so stale ids must be checked before removal.
pub fn cancel_source(source: SourceId) {
    if glib::MainContext::default().find_source_by_id(&source).is_some() {
        source.remove();
    }
}
