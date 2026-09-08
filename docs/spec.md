# Product Requirement & Technical Specification: Linux Sticky Notes (Rust + GTK4)

Working name: **Pinlet**

## 1. Executive Summary & Vision

A native, high-performance, and beautifully designed sticky notes application built specifically for modern Linux desktop environments (Ubuntu / GNOME). Built with Rust, GTK4, and libadwaita, the application provides low resource footprint, Wayland compliance, rich reminder scheduling, and customizable desktop note management. Notes are stored as plain Markdown files inside a git repository, enabling user-owned, conflict-safe cross-machine syncing with no proprietary service.

## 2. Technology Stack & Architecture

### 2.1 Core Components

- **Language**: Rust (2024 Edition)
- **GUI Toolkit**: gtk4-rs + libadwaita (Native GNOME look and feel, dark mode support)
- **Storage**: Plain Markdown files with YAML frontmatter (serde + serde_yaml); notes live in a git repository
- **Git Integration**: git CLI for v1 auto-commits; gix (pure Rust git) for built-in pull/push sync in Phase 4
- **System Tray Protocol**: ksni (Pure Rust StatusNotifierItem DBus implementation)
- **Desktop Integration**: ashpd / notify-rust (Freedesktop portal integration & system notifications)
- **Desktop Layer Positioning**: gtk4-layer-shell (wallpaper-level note pinning) plus `_NET_WM_WINDOW_TYPE_DESKTOP` X11/XWayland windows
- **Time & Date Operations**: chrono
- **IPC & DBus**: zbus (single-instance handoff, GNOME Shell search provider)
- **Cryptography**: argon2 + chacha20poly1305 (encrypted locked notes)
- **CLI Parsing**: clap (quick-capture subcommand)
- **Unique IDs**: uuid (note file names and frontmatter IDs)

### 2.2 System Architecture Overview

```text
┌──────────────────────────────────────────────────────────────┐
│                        QUICK CAPTURE                         │
│           CLI subcommand & Global Shortcut portal            │
└──────────────────────────────────────────────────────────────┘
                                │
       DBus handoff to running instance (single-instance)
                                ▼
┌──────────────────────────────────────────────────────────────┐
│                      SYSTEM TRAY (ksni)                      │
└──────────────────────────────────────────────────────────────┘
                                │
                      DBus Action Signals
                                ▼
┌──────────────────────────────────────────────────────────────┐
│                     GTK4 / Libadwaita UI                     │
│   ┌───────────────┐  ┌───────────────┐  ┌───────────────┐    │
│   │ Note Window 1 │  │ Note Window 2 │  │    Settings   │    │
│   └───────────────┘  └───────────────┘  └───────────────┘    │
└──────────────────────────────────────────────────────────────┘
                                │
                    Event & Persistence Sync
                                ▼
┌──────────────────────────────────────────────────────────────┐
│                 APP CORE & SCHEDULER ENGINE                  │
│• Debounced Auto-save Listener                                │
│• Background Timer Service (GLib Timeout / Tokio)             │
│• Reminder Engine & Recurrence Scheduler                      │
│• In-Memory Note Index & Search                               │
└──────────────────────────────────────────────────────────────┘
                                │
 debounced writes · auto-commit          search queries
                ────────────────┬────────────────
                │                               │
┌──────────────────────────────┐  ┌───────────────────────────┐
│     GIT-BACKED NOTE REPO     │  │    GNOME SHELL SEARCH     │
│notes/*.md · YAML frontmatter │  │      PROVIDER (DBus)      │
│        settings.json         │  │                           │
└──────────────────────────────┘  └───────────────────────────┘
```

## 3. Feature Specifications

### 3.1 Local-First File Storage & Git Sync

- **Markdown Note Files**: Each note is a single Markdown file whose YAML frontmatter carries title, color, flags, and reminders; the body is the note content.
- **Git-Backed Repository**: The note directory is a git repository. The app auto-commits coalesced changes (2 minutes after the last edit, plus on lock and on quit), so every change is versioned and diffable.
- **User-Managed Sync (v1)**: Cross-machine syncing uses the user's own git workflow — push/pull via CLI or any git host, no account or server required.
- **Built-in Sync (Phase 4)**: Pull on launch and push after idle; merge conflicts surface a notification with keep-local / keep-remote resolution.
- **Local-Only State**: Machine-specific data (window geometry, per-machine settings) lives in a git-ignored `local-state.json` and never syncs.
- **Encrypted Notes Stay Local**: Locked, encrypted notes are excluded from the synced repository — ciphertext produces opaque diffs — and exist only on the machine where they were locked.

### 3.2 Sticky Note UI & Interaction

- **Frameless/Bordered Window Toggle**: Clean, minimalist window chrome styled to look like physical paper notes.
- **Inline Color Selector Toolbar**: Header palette allows toggling individual note background colors (Canary Yellow, Mint Green, Sky Blue, Soft Pink, Lavender, Charcoal/Dark).
- **Rich Text & Checklists**: Text buffers supporting word wrap, inline basic formatting, and interactive checkable boxes (`- [ ]`).
- **Markdown Toolbar**: Edit-mode buttons for bold, italic, strikethrough, link, heading, quote, code, bullet list, and checklist — operating on the selection (or cursor spot) in one undo step.
- **Editable Checklists**: Task markers toggle by clicking them in the editor as well as the preview; Enter on a task, bullet, or numbered item continues it (numbered items take the next number; following lines keep theirs), Enter on an empty item removes it.
- **Word/Character Footer**: A slim footer under the editor shows live word and character counts (hidden for locked notes).
- **Per-Note Text Size**: Toolbar A−/A+ (or Ctrl+plus/minus) set a per-note scale; the reset button (or Ctrl+0) returns to the global size. Scales clamp to 50–300%.
- **Tags**: Notes carry up to two free-form tags shown as `#`-prefixed pills in the footer, with a plus button revealing an inline entry to add more. Each pill carries a `×` button that removes it; pills are otherwise inert. Pill colors come from a deterministic per-tag palette. The add control hides at the cap. Tags are hidden for locked notes.
- **Header Actions**: Quick access buttons for adding a new note (+), color theme picker, setting reminders (⏰), locking (🔒), pinning (📌), and moving to trash (🗑️).

### 3.3 System Tray & Quick Actions

- **Status Bar Presence**: Native Ubuntu top bar icon with dropdown menu.
- **Quick Menu Actions**:
  - Spawn New Sticky Note
  - Toggle Show/Hide All Notes
  - Notes submenu: every note, five per level with nested More… pages, so a closed note can be reopened.
  - Trash submenu: trashed notes with Restore / Delete forever per entry, plus Empty trash. Trashed notes appear nowhere else (no windows, no reminders).
  - Dynamic Section: List of top 5 upcoming due reminders with time labels. Clicking any item opens and focuses the target note.
  - Preferences / Settings
  - Application Quit

### 3.4 Active Reminders & System Notifications

- **Scheduled Alarms**: Date and time picker dialog per note; a note may carry any number of independent reminders, each with its own schedule.
- **Desktop Notifications**: Fires native desktop notifications via DBus when a reminder triggers.
- **Actionable Notification Buttons**:
  - _Open Note_: Brings the corresponding sticky note to the foreground.
  - _Snooze 10m_: Reschedules the reminder.
  - _Mark done_: Removes the fired reminder so it never fires again (for a recurring reminder this ends the whole series).
- **Trash**: Deleting a note moves it to the trash instead — flagged in frontmatter, window closed, silent and hidden until restored or emptied. Permanent deletion happens per note or via Empty trash; git history retains the content either way.
- **Catch-up Mechanism**: Checks for expired reminders on boot or wake from system sleep.

### 3.5 Themes & Preferences

- **Global vs. Individual Note Styles**:
  - _Mode A (Flexible)_: Each note maintains its own user-chosen accent color.
  - _Mode B (Uniform)_: All notes inherit a single unified theme selected in Global Settings.
- **Dark Mode Synchronization**: Automatically adjusts contrast and text color depending on GNOME's system dark mode state.
- **Text Size**: A global text scale (80–150% presets) applies to every note; individual notes can override it and reset back to global.

### 3.6 Advanced Desktop Features

- **Desktop Pinning**: Pins notes down behind other application windows directly to the desktop workspace. The rendering mechanism is chosen per session:
  - **X11 (Xorg) / XWayland**: a window carrying `_NET_WM_WINDOW_TYPE_DESKTOP` (above the wallpaper, below all windows, exempt from "show desktop").
  - **Wayland + wlr-layer-shell** (KDE Plasma 6, wlroots compositors): the layer-shell protocol.
  - The pin button disables with an explanation only when neither of these is available.
- **Privacy Locking**: Lock individual notes containing sensitive information; unlocking requires password or Linux PAM authentication.
- **Drag and Drop (Drop Zone)**: Drag text, links, or image files onto a note window or system tray icon to automatically append content.

### 3.7 Window State Persistence

- **Geometry Restore**: Each note remembers its position, size, and monitor across sessions and is restored to its previous location on launch.
- **Per-Note Topmost**: The always-on-top flag persists independently per note.
- Geometry is saved debounced on move/resize and flushed on close, into the local-only state file (§3.1).

### 3.8 Global Note Search

- **In-Memory Index**: All note titles and contents are indexed in memory at startup and updated incrementally on change — note-scale data needs no external search engine.
- **Quick-Find Popup**: `Ctrl+Shift+F` opens a search popup from any note window or the tray; results show title + content snippet.
- **Result Actions**: Selecting a result opens and focuses the target note. Search also serves as the backend for the GNOME Shell search provider (§3.11).

### 3.9 Quick Capture

- **CLI Capture**: `pinlet "buy milk"` (or `pinlet --new --color Yellow "..."`) creates a note in the running instance via DBus single-instance handoff; starts the app if it is not running.
- **Global Shortcut**: User-configurable hotkey creating a note from anywhere. Preferred mechanism: the XDG GlobalShortcuts portal (ashpd); on GNOME < 47 a custom keybinding in `org.gnome.settings-daemon.plugins.media-keys` is registered as a fallback. Both invoke the same capture path as the CLI.
- **Clipboard Capture**: Optional action to append the current clipboard contents to a note.

### 3.10 Encrypted Locked Notes

- Locked notes are encrypted at rest: title and content are encrypted with XChaCha20-Poly1305 using a key derived from the user password via argon2id.
- Unlock requires the password (or PAM authentication); decrypted content is held in memory only while unlocked.
- Encrypted notes are excluded from the synced repository (§3.1) and remain local-only.
- Unencrypted notes and app metadata remain unaffected.

### 3.11 GNOME Shell Search Provider

- Implements the `org.gnome.Shell.SearchProvider2` DBus interface so notes appear in the GNOME Activities overview search.
- Results show title and content snippet; activating a result opens and focuses the note.
- Registered via the application `.desktop` file.

### 3.12 Recurring Reminders

- Reminders can repeat: `none`, `daily`, `weekly`, `weekdays`, or `custom:<n>d` (every n days).
- After a reminder fires, the next occurrence is computed and scheduled automatically.
- Extended snooze options: 10 minutes (default), 1 hour, tomorrow morning.
- The catch-up mechanism applies to missed recurring occurrences.

## 4. Data Storage & Sync Model

### 4.1 Repository Layout

The note directory under the XDG data dir is a git repository the user fully owns:

```text
~/.local/share/pinlet/            ← git repository
├── notes/
│   └── 3f9a2c1d-….md             ← one file per note
├── settings.json                 ← global settings (synced)
├── local-state.json              ← machine-specific, git-ignored
└── .gitignore
```

### 4.2 Note File Format

Each note file carries its metadata in YAML frontmatter; the body is the note's Markdown content. A note may carry any number of independent reminders in its `reminders` list. Writes are atomic (temp file, then rename).

```yaml
---
id: 3f9a2c1d-4b2a-4f1e-9c3d-2a1b3c4d5e6f
title: Groceries
color: Yellow                  # Yellow | Green | Blue | Pink | Purple | Custom(Hex)
workspace_id: -1
is_pinned_to_desktop: false
is_always_on_top: false
is_locked: false
tags: [work, home]               # free-form labels, may be absent
is_trashed: false
trashed_at: null
font_scale: null                 # per-note text multiplier, null = global
created_at: 2026-09-05T10:00:00Z
updated_at: 2026-09-05T11:30:00Z
reminders:
  - due_at: 2026-09-06T09:00:00Z
    recurrence_rule: daily      # none | daily | weekly | weekdays | custom:<n>d
    last_fired_at: null
    snooze_count: 0
  - due_at: 2026-09-12T18:30:00Z
    recurrence_rule: none
    last_fired_at: null
    snooze_count: 0
---
- [ ] Buy milk
- [x] Bread
```

### 4.3 Local-Only State (`local-state.json`)

Machine-specific data that must never sync (illustrative):

```json
{
  "window_geometry": {
    "3f9a2c1d-…": { "x": 120, "y": 80, "width": 320, "height": 280, "monitor_id": 0 }
  },
  "git": { "auto_commit": true, "commit_after_idle_ms": 120000 }
}
```

### 4.4 Global Settings (`settings.json`)

```json
{
  "force_global_color": false,
  "default_color": "Yellow",
  "sync_dark_mode": true,
  "auto_save_debounce_ms": 500,
  "enable_global_shortcut": false,
  "autostart": false,
  "font_scale": 1.0,
  "tag_colors": { "work": "blue" },
  "archive_retention_days": 30
}
```

### 4.5 Git Sync Behavior

- **Auto-commit**: Coalesced commits — 2 minutes after the last edit, plus on lock and on quit — with generated messages (`Update 'Groceries'`, `Delete note`, `Settings changed`).
- **v1 (user-managed)**: The app never force-pushes or rebases; users push/pull with their own git tooling or host.
- **Phase 4 (built-in)**: Pull on launch, push after idle; conflicts raise a notification with keep-local / keep-remote resolution.
- **Exclusions**: `local-state.json` and encrypted (locked) notes never enter the repository.

## 5. Implementation Milestones & Roadmap

### Phase 1: MVP Core (Weeks 1–2)

- [ ] GTK4 + libadwaita window creation and CSS color styling.
- [ ] Markdown file persistence (serde_yaml frontmatter) with debounced auto-save and atomic writes. _(§3.1)_
- [ ] Git auto-commit on idle / lock / quit. _(§3.1)_
- [ ] Multi-window management (creating, updating, and deleting note windows).

### Phase 2: Tray & System Integration (Weeks 3–4)

- [ ] Implement ksni system tray service with dynamic menu items.
- [ ] Wire thread-safe DBus signals between tray clicks and GTK UI window focus.
- [ ] Implement notify-rust reminder engine and background GLib timeout loop.
- [ ] Window state persistence: position, size, and monitor restore. _(§3.7)_
- [ ] Global note search: in-memory index + quick-find popup. _(§3.8)_

### Phase 3: Advanced Features & Preferences (Weeks 5–6)

- [ ] Build Settings preferences modal (AdwPreferencesWindow).
- [ ] Implement desktop layer pinning via gtk4-layer-shell.
- [ ] Support interactive checklists and drag-and-drop text attachments.
- [ ] Quick capture: CLI subcommand + global shortcut via GlobalShortcuts portal. _(§3.9)_
- [ ] Encrypted locked notes (argon2id + XChaCha20-Poly1305, local-only). _(§3.10)_
- [ ] Package app as Debian .deb package for easy installation on Ubuntu.

### Phase 4: Post-v1 Enhancements (Weeks 7–8)

- [ ] GNOME Shell search provider (SearchProvider2 DBus interface). _(§3.11)_
- [ ] Recurring reminders (daily / weekly / weekdays / custom). _(§3.12)_
- [ ] Extended snooze options (1 hour, tomorrow morning).
- [ ] Built-in git sync: pull on launch, push after idle, conflict resolution UI. _(§3.1)_

## 6. Future Considerations (Post-v1)

Candidate features recorded during design review, not yet scheduled:

- Checklist progress indicators
- Embedded images (assets directory inside the note repo); note templates; note-to-note links
- Natural-language due dates; GNOME Calendar integration
- GNOME 47+ accent color synchronization; screenshot-to-note via portal
- Importers for Xpad / KNotes / GNOME Sticky Notes

Explicitly out of scope: proprietary cloud sync, collaboration, and full wiki-style features — syncing is git-based, and the app stays local-first and lightweight. Export and backup are intentionally absent from the roadmap: notes are already portable Markdown, and git history covers backups.
