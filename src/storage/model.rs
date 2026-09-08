//! Core data model: notes, colors, and reminders.
//!
//! A note on disk is a Markdown file: YAML frontmatter carrying all
//! structured metadata, followed by the note body. [`render_file`]
//! and [`parse_note_file`] convert between the two representations.

use std::path::Path;

use chrono::{DateTime, Datelike, Duration, Utc, Weekday};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// Line that opens and closes the frontmatter block.
const FRONTMATTER_DELIM: &str = "---";

/// The note color themes, plus arbitrary custom hex colors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NoteColor {
    /// Canary Yellow.
    #[default]
    Yellow,
    /// Mint Green.
    Green,
    /// Sky Blue.
    Blue,
    /// Soft Pink.
    Pink,
    /// Lavender.
    Purple,
    /// Charcoal / Dark.
    Charcoal,
    /// Custom `#RRGGBB` hex value.
    Custom(String),
}

impl NoteColor {
    /// Parse a user-supplied color: a palette name or `#RRGGBB` hex.
    /// Anything else is rejected — unknown strings must not silently
    /// become custom colors that generate broken CSS.
    pub fn parse_validated(s: &str) -> Option<Self> {
        match s {
            "Yellow" => Some(Self::Yellow),
            "Green" => Some(Self::Green),
            "Blue" => Some(Self::Blue),
            "Pink" => Some(Self::Pink),
            "Purple" => Some(Self::Purple),
            "Charcoal" => Some(Self::Charcoal),
            hex if is_hex_color(hex) => Some(Self::Custom(hex.to_owned())),
            _ => None,
        }
    }

    /// The six named colors offered by the inline palette.
    pub const PALETTE: [Self; 6] = [
        Self::Yellow,
        Self::Green,
        Self::Blue,
        Self::Pink,
        Self::Purple,
        Self::Charcoal,
    ];

    /// Human-readable name, used in tooltips.
    pub fn name(&self) -> &str {
        match self {
            Self::Yellow => "Canary Yellow",
            Self::Green => "Mint Green",
            Self::Blue => "Sky Blue",
            Self::Pink => "Soft Pink",
            Self::Purple => "Lavender",
            Self::Charcoal => "Charcoal",
            Self::Custom(hex) => hex,
        }
    }

    /// Wire representation used in frontmatter.
    fn wire(&self) -> String {
        match self {
            Self::Yellow => "Yellow".to_owned(),
            Self::Green => "Green".to_owned(),
            Self::Blue => "Blue".to_owned(),
            Self::Pink => "Pink".to_owned(),
            Self::Purple => "Purple".to_owned(),
            Self::Charcoal => "Charcoal".to_owned(),
            Self::Custom(hex) => hex.clone(),
        }
    }
}

impl std::str::FromStr for NoteColor {
    type Err = std::convert::Infallible;

    /// Unknown strings become `Custom` colors.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Yellow" => Self::Yellow,
            "Green" => Self::Green,
            "Blue" => Self::Blue,
            "Pink" => Self::Pink,
            "Purple" => Self::Purple,
            "Charcoal" => Self::Charcoal,
            other => Self::Custom(other.to_owned()),
        })
    }
}

impl Serialize for NoteColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.wire())
    }
}

impl<'de> Deserialize<'de> for NoteColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        // Lenient like every other frontmatter field: an unknown
        // color falls back to the default instead of producing a
        // `Custom` that matches no stylesheet rule.
        Ok(Self::parse_validated(&raw).unwrap_or_default())
    }
}

/// One scheduled alarm attached to a note. A note may carry any
/// number of independent reminders (spec §3.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reminder {
    /// Due time, ISO 8601 UTC.
    pub due_at: DateTime<Utc>,
    /// Repetition rule.
    #[serde(default)]
    pub recurrence_rule: Recurrence,
    /// Last time this reminder fired, if ever.
    #[serde(default)]
    pub last_fired_at: Option<DateTime<Utc>>,
}

/// Repetition rule for a reminder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Recurrence {
    /// One-shot reminder.
    #[default]
    None,
    /// Every day at the same time.
    Daily,
    /// Every week at the same time.
    Weekly,
    /// Monday through Friday.
    Weekdays,
    /// Repeat every `n` days.
    Custom(u32),
}

impl Serialize for Recurrence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Self::None => "none".to_owned(),
            Self::Daily => "daily".to_owned(),
            Self::Weekly => "weekly".to_owned(),
            Self::Weekdays => "weekdays".to_owned(),
            Self::Custom(n) => format!("custom:{n}d"),
        };
        serializer.serialize_str(&wire)
    }
}

impl<'de> Deserialize<'de> for Recurrence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let parsed = match raw.as_str() {
            "none" => Self::None,
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            "weekdays" => Self::Weekdays,
            other => other
                .strip_prefix("custom:")
                .and_then(|rest| rest.strip_suffix('d'))
                .and_then(|days| days.parse().ok())
                .map(Self::Custom)
                // Lenient: one hand-edited garbage rule must not fail
                // the whole note parse and drop the note from the
                // store. It degrades to a one-shot reminder and is
                // normalized on the next save.
                .unwrap_or(Self::None),
        };
        Ok(parsed)
    }
}

impl Recurrence {
    /// The next occurrence strictly after `due`.
    ///
    /// `Custom(0)` (reachable via hand-edited frontmatter) is clamped
    /// to daily so callers advancing in a loop always terminate.
    pub fn next_after(&self, due: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::None => due,
            Self::Daily => due + Duration::days(1),
            Self::Weekly => due + Duration::days(7),
            Self::Weekdays => next_weekday(due),
            Self::Custom(n) => due + Duration::days(i64::from((*n).max(1))),
        }
    }

    /// The first occurrence strictly after `now`, starting from `due`.
    /// Missed occurrences collapse into this single firing. Unlike an
    /// iteratively capped advance, this always lands in the future —
    /// a long-overdue reminder can never get stuck with a past due
    /// that neither fires nor appears upcoming.
    pub fn next_occurrence(&self, due: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
        match self {
            Self::None => due,
            Self::Daily => shift_past(due, now, 1),
            Self::Weekly => shift_past(due, now, 7),
            Self::Custom(n) => shift_past(due, now, i64::from((*n).max(1))),
            Self::Weekdays => {
                // Every step moves at least a day forward, so this
                // always terminates.
                let mut next = due;
                while next <= now {
                    next = next_weekday(next);
                }
                next
            }
        }
    }
}

/// `due` advanced by whole `step`-day blocks to land strictly past
/// `now`, preserving the time of day.
fn shift_past(due: DateTime<Utc>, now: DateTime<Utc>, step: i64) -> DateTime<Utc> {
    if due > now {
        return due;
    }
    // Whole step-blocks past the present. Both operands are >= 1
    // here (`due <= now`, `step >= 1` at every call site).
    let behind = (now - due).num_days() + 1;
    due + Duration::days((behind + step - 1) / step * step)
}

/// The next weekday (Mon–Fri) strictly after `due`.
fn next_weekday(due: DateTime<Utc>) -> DateTime<Utc> {
    let mut next = due + Duration::days(1);
    while matches!(next.weekday(), Weekday::Sat | Weekday::Sun) {
        next += Duration::days(1);
    }
    next
}

/// A sticky note: frontmatter metadata plus a Markdown body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// Stable identifier; also the file name (`{id}.md`).
    pub id: Uuid,
    /// Note title; derived from the body's first line on save.
    #[serde(default)]
    pub title: String,
    /// Background color.
    #[serde(default)]
    pub color: NoteColor,
    /// Pinned to the desktop layer (spec §3.6).
    #[serde(default)]
    pub is_pinned_to_desktop: bool,
    /// Always on top of other windows.
    #[serde(default)]
    pub is_always_on_top: bool,
    /// Locked (and encrypted) — spec §3.10.
    #[serde(default)]
    pub is_locked: bool,
    /// Creation time.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Last modification time.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Scheduled alarms for this note.
    #[serde(default)]
    pub reminders: Vec<Reminder>,
    /// User-assigned labels (frontmatter; empty means untagged).
    #[serde(default)]
    pub tags: Vec<String>,
    /// In the trash instead of permanently deleted: hidden
    /// everywhere until restored or emptied.
    #[serde(default)]
    pub is_trashed: bool,
    /// When the note entered the trash, if ever.
    #[serde(default)]
    pub trashed_at: Option<DateTime<Utc>>,
    /// Per-note text scale multiplier. `None` follows the global
    /// font scale in settings.
    #[serde(default)]
    pub font_scale: Option<f32>,
}

impl Note {
    /// A freshly created note.
    pub fn new(id: Uuid, color: NoteColor) -> Self {
        let now = Utc::now();
        Self {
            id,
            title: String::new(),
            color,
            is_pinned_to_desktop: false,
            is_always_on_top: false,
            is_locked: false,
            created_at: Some(now),
            updated_at: Some(now),
            reminders: Vec::new(),
            tags: Vec::new(),
            is_trashed: false,
            trashed_at: None,
            font_scale: None,
        }
    }

    /// Longest tag the UI accepts; longer input is rejected rather
    /// than silently truncated into a different tag.
    pub const MAX_TAG_LEN: usize = 32;

    /// Most tags a note carries. The UI hides the add control at the
    /// cap; cleaning truncates defensively past it.
    pub const MAX_TAGS: usize = 2;

    /// Clean user-supplied tags for storage: normalized, deduplicated
    /// (first occurrence wins), capped at [`Self::MAX_TAGS`].
    pub fn clean_tags(raw: Vec<String>) -> Vec<String> {
        let mut clean = Vec::new();
        for item in raw {
            if let Some(tag) = Self::normalize_tag(&item) {
                if !clean.contains(&tag) {
                    clean.push(tag);
                }
            }
        }
        clean.truncate(Self::MAX_TAGS);
        clean
    }

    /// Normalize a user-typed tag: trimmed, non-empty, bounded.
    /// `None` means the input is not a usable tag.
    pub fn normalize_tag(raw: &str) -> Option<String> {
        let tag = raw.trim();
        if tag.is_empty() || tag.chars().count() > Self::MAX_TAG_LEN {
            return None;
        }
        Some(tag.to_owned())
    }

    /// Derive the note title from its body: the first non-empty line.
    pub fn derive_title(body: &str) -> String {
        body.lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_owned()
    }
}

/// Render a note to its on-disk Markdown representation: YAML
/// frontmatter, then the body.
pub fn render_file(note: &Note, body: &str) -> AppResult<String> {
    let yaml = serde_yaml_ng::to_string(note)?;
    let mut out = String::with_capacity(yaml.len() + body.len() + 16);
    out.push_str(FRONTMATTER_DELIM);
    out.push('\n');
    out.push_str(&yaml);
    out.push_str(FRONTMATTER_DELIM);
    out.push('\n');
    out.push_str(body);
    Ok(out)
}

/// Parse a note file into its frontmatter model and body text.
pub fn parse_note_file(path: &Path) -> AppResult<(Note, String)> {
    let raw = std::fs::read_to_string(path)?;
    let (yaml, body) = split_frontmatter(&raw).ok_or_else(|| AppError::InvalidNoteFile {
        path: path.to_path_buf(),
        reason: "missing YAML frontmatter".to_owned(),
    })?;
    let note: Note = serde_yaml_ng::from_str(yaml).map_err(|err| AppError::InvalidNoteFile {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;
    Ok((note, body.to_owned()))
}

/// Check for `#RRGGBB` hex (frontmatter and CLI input).
fn is_hex_color(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(|b| b.is_ascii_hexdigit())
}

/// Split `---\n{yaml}\n---\n{body}`; `None` without the opening
/// delimiter. The closing delimiter is the first line that is exactly
/// `---` after the opening one, so the body may contain `---` lines.
/// CRLF line endings (hand-edited on Windows) are accepted too.
fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let (yaml, body) = rest
        .split_once("\n---\n")
        .or_else(|| rest.split_once("\r\n---\r\n"))?;
    Some((yaml, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_then_split_round_trips() {
        let mut note = Note::new(Uuid::new_v4(), NoteColor::Blue);
        note.reminders.push(Reminder {
            due_at: Utc::now(),
            recurrence_rule: Recurrence::Custom(5),
            last_fired_at: None,
        });
        let body = "first line\nsecond line\n\n---\nnot a delimiter\n";
        let rendered = render_file(&note, body).unwrap();
        assert!(rendered.starts_with("---\n"));

        let (yaml, parsed_body) = split_frontmatter(&rendered).unwrap();
        let parsed: Note = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(parsed.id, note.id);
        assert_eq!(parsed.color, NoteColor::Blue);
        assert_eq!(parsed.reminders[0].recurrence_rule, Recurrence::Custom(5));
        assert_eq!(parsed_body, body);
    }

    #[test]
    fn recurrence_wire_format() {
        let yaml = serde_yaml_ng::to_string(&Recurrence::Custom(7)).unwrap();
        assert_eq!(yaml.trim(), "custom:7d");

        let weekly: Recurrence = serde_yaml_ng::from_str("weekly").unwrap();
        assert_eq!(weekly, Recurrence::Weekly);

        let none: Recurrence = serde_yaml_ng::from_str("none").unwrap();
        assert_eq!(none, Recurrence::None);

        // Lenient: garbage degrades to a one-shot rule instead of
        // failing the note parse.
        let garbage: Recurrence = serde_yaml_ng::from_str("garbage").unwrap();
        assert_eq!(garbage, Recurrence::None);
    }

    #[test]
    fn recurrence_advances_to_next_occurrence() {
        // 2024-01-05 was a Friday.
        let fri = "2024-01-05T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            Recurrence::Daily.next_after(fri),
            at("2024-01-06T10:00:00Z")
        );
        assert_eq!(
            Recurrence::Weekly.next_after(fri),
            at("2024-01-12T10:00:00Z")
        );
        // Friday rolls to Monday; weekend starts jump there too.
        assert_eq!(
            Recurrence::Weekdays.next_after(fri),
            at("2024-01-08T10:00:00Z")
        );
        assert_eq!(
            Recurrence::Weekdays.next_after(at("2024-01-06T10:00:00Z")),
            at("2024-01-08T10:00:00Z")
        );
        assert_eq!(
            Recurrence::Weekdays.next_after(at("2024-01-03T10:00:00Z")),
            at("2024-01-04T10:00:00Z")
        );
        assert_eq!(
            Recurrence::Custom(3).next_after(fri),
            at("2024-01-08T10:00:00Z")
        );
        // Custom(0) would never advance — clamped to daily so the
        // catch-up loop in the reminder engine always terminates.
        assert_eq!(
            Recurrence::Custom(0).next_after(fri),
            at("2024-01-06T10:00:00Z")
        );
    }

    #[test]
    fn next_occurrence_always_lands_future() {
        let at = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let fri = at("2024-01-05T10:00:00Z");
        // Already future: untouched.
        assert_eq!(
            Recurrence::Daily.next_occurrence(at("2024-01-06T10:00:00Z"), fri),
            at("2024-01-06T10:00:00Z")
        );
        // Just overdue: the next slot, time of day kept.
        assert_eq!(
            Recurrence::Daily.next_occurrence(fri, at("2024-01-05T10:00:01Z")),
            at("2024-01-06T10:00:00Z")
        );
        // Years overdue: still lands future instead of sticking past.
        let long_overdue = Recurrence::Daily.next_occurrence(fri, at("2027-06-01T09:00:00Z"));
        assert!(long_overdue > at("2027-06-01T09:00:00Z"));
        assert_eq!(long_overdue.time(), fri.time(), "time of day is preserved");
        // Weekly and custom advance by whole blocks.
        assert_eq!(
            Recurrence::Weekly.next_occurrence(fri, at("2024-01-10T10:00:00Z")),
            at("2024-01-12T10:00:00Z")
        );
        assert_eq!(
            Recurrence::Custom(3).next_occurrence(fri, at("2024-01-10T10:00:00Z")),
            at("2024-01-11T10:00:00Z")
        );
        // Weekdays skip the weekend even from far past.
        let next = Recurrence::Weekdays.next_occurrence(fri, at("2024-01-20T10:00:00Z"));
        assert!(next > at("2024-01-20T10:00:00Z"));
        assert!(!matches!(next.weekday(), Weekday::Sat | Weekday::Sun));
        // One-shots never move.
        assert_eq!(
            Recurrence::None.next_occurrence(fri, at("2025-01-01T00:00:00Z")),
            fri
        );
    }

    #[test]
    fn color_wire_format() {
        let yaml = serde_yaml_ng::to_string(&NoteColor::Yellow).unwrap();
        assert_eq!(yaml.trim(), "Yellow");

        let custom: NoteColor = serde_yaml_ng::from_str("\"#ff00aa\"").unwrap();
        assert_eq!(custom, NoteColor::Custom("#ff00aa".to_owned()));

        // Unknown strings fall back to the default instead of a
        // `Custom` that matches no stylesheet rule.
        let unknown: NoteColor = serde_yaml_ng::from_str("\"anything\"").unwrap();
        assert_eq!(unknown, NoteColor::default());
    }

    #[test]
    fn validated_colors_accept_palette_and_hex_only() {
        assert_eq!(NoteColor::parse_validated("Blue"), Some(NoteColor::Blue));
        assert_eq!(
            NoteColor::parse_validated("#ff00aa"),
            Some(NoteColor::Custom("#ff00aa".to_owned()))
        );
        assert_eq!(NoteColor::parse_validated("chartreuse"), None);
        assert_eq!(NoteColor::parse_validated("#xyz"), None);
        assert_eq!(NoteColor::parse_validated(""), None);
    }

    #[test]
    fn split_frontmatter_accepts_crlf() {
        let raw = "---\r\nid: 1\r\n---\r\nbody\r\n";
        let (yaml, body) = split_frontmatter(raw).unwrap();
        assert_eq!(yaml, "id: 1");
        assert_eq!(body, "body\r\n");
        assert!(split_frontmatter("no frontmatter").is_none());
    }

    #[test]
    fn derive_title_uses_first_non_empty_line() {
        assert_eq!(Note::derive_title("\n\n  Groceries  \nmore"), "Groceries");
        assert_eq!(Note::derive_title(""), "");
        assert_eq!(Note::derive_title("   \n  \n"), "");
    }

    #[test]
    fn tag_cleaning_dedupes_and_caps() {
        let cleaned = Note::clean_tags(vec![
            "  work ".to_owned(),
            "work".to_owned(),
            "".to_owned(),
            "home".to_owned(),
            "extra".to_owned(),
        ]);
        assert_eq!(cleaned, vec!["work".to_owned(), "home".to_owned()]);
    }

    #[test]
    fn tag_input_trims_and_rejects_garbage() {
        assert_eq!(Note::normalize_tag("  work  "), Some("work".to_owned()));
        assert_eq!(Note::normalize_tag(""), None);
        assert_eq!(Note::normalize_tag("   "), None);
        assert_eq!(Note::normalize_tag(&"x".repeat(33)), None);
        assert_eq!(Note::normalize_tag(&"x".repeat(32)), Some("x".repeat(32)));
    }

    #[test]
    fn new_fields_round_trip_and_default() {
        let mut note = Note::new(Uuid::new_v4(), NoteColor::Yellow);
        note.tags = vec!["work".to_owned(), "home".to_owned()];
        note.is_trashed = true;
        note.trashed_at = Some(Utc::now());
        note.font_scale = Some(1.2);
        let rendered = render_file(&note, "body").unwrap();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pinlet-test-{}.md", Uuid::new_v4()));
        std::fs::write(&path, &rendered).unwrap();
        let (parsed, _) = parse_note_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(parsed.tags, note.tags);
        assert!(parsed.is_trashed);
        assert!(parsed.trashed_at.is_some());
        assert_eq!(parsed.font_scale, Some(1.2));

        // Old files without the keys still load.
        let legacy: Note = serde_yaml_ng::from_str(&format!("id: {}", note.id)).unwrap();
        assert!(legacy.tags.is_empty());
        assert!(!legacy.is_trashed);
        assert!(legacy.trashed_at.is_none());
        assert!(legacy.font_scale.is_none());
    }
}
