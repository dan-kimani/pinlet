//! Global capture shortcut via the XDG GlobalShortcuts portal
//! (spec §3.8). The portal lets the user configure the actual key
//! combination in system settings; Pinlet only asks for a logical
//! "new note" shortcut.
//!
//! Runs on its own async-io thread; activations funnel through the
//! same message channel as the tray.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender as MsgSender;

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_lite::StreamExt;

use crate::messages::Msg;

/// Bind the "new-note" shortcut and forward activations forever.
/// Runs inside [`async_io::block_on`] on a dedicated thread.
/// Activations only fire while `gate` is set — the thread itself is
/// spawned at most once, and the Preferences switch flips the gate.
async fn run(tx: MsgSender<Msg>, gate: Arc<AtomicBool>) {
    let Ok(proxy) = GlobalShortcuts::new().await else {
        eprintln!("GlobalShortcuts portal unavailable");
        return;
    };
    let Ok(session) = proxy.create_session().await else {
        eprintln!("failed to create shortcut session");
        return;
    };
    let shortcuts = vec![NewShortcut::new("new-note", "New note")];
    if let Ok(request) = proxy.bind_shortcuts(&session, &shortcuts, None).await
        && let Err(err) = request.response()
    {
        eprintln!("failed to bind shortcut: {err}");
        return;
    }
    let Ok(mut stream) = proxy.receive_activated().await else {
        eprintln!("failed to listen for shortcut activations");
        return;
    };
    while let Some(activated) = stream.next().await {
        if activated.shortcut_id() == "new-note" && gate.load(Ordering::SeqCst) {
            let _ = tx.send(Msg::NewNote);
        }
    }
}

/// Spawn the portal listener thread. Best effort: without a portal
/// the thread logs and exits, and the app keeps running.
pub fn spawn(tx: MsgSender<Msg>, gate: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("pinlet-shortcuts".to_owned())
        .spawn(move || async_io::block_on(run(tx, gate)))
        .expect("failed to spawn shortcut thread");
}

/// Whether this desktop session provides the GlobalShortcuts portal
/// (needs a desktop backend — e.g. GNOME 47+). Probed once at
/// startup so the preferences UI can reflect it.
pub fn is_supported() -> bool {
    async_io::block_on(async { GlobalShortcuts::new().await.is_ok() })
}
