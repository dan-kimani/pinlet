//! Core data model: notes, colors, and reminders.
//!
//! A note on disk is a Markdown file: YAML frontmatter carrying all
//! structured metadata, followed by the note body. [`render_file`]
//! and [`parse_note_file`] convert between the two representations.

use std::path::Path;

use chrono::{DateTime, Utc};
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
        match raw.parse() {
            Ok(color) => Ok(color),
            Err(never) => match never {},
        }
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
    /// How many times it has been snoozed.
    #[serde(default)]
    pub snooze_count: u32,
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
        match raw.as_str() {
            "none" => Ok(Self::None),
            "daily" => Ok(Self::Daily),
            "weekly" => Ok(Self::Weekly),
            "weekdays" => Ok(Self::Weekdays),
            other => other
                .strip_prefix("custom:")
                .and_then(|rest| rest.strip_suffix('d'))
                .and_then(|days| days.parse().ok())
                .map(Self::Custom)
                .ok_or_else(|| D::Error::custom(format!("invalid recurrence rule: {other}"))),
        }
    }
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
    /// Logical workspace grouping (unused for now).
    #[serde(default = "default_workspace")]
    pub workspace_id: i64,
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
}

impl Note {
    /// A freshly created note.
    pub fn new(id: Uuid, color: NoteColor) -> Self {
        let now = Utc::now();
        Self {
            id,
            title: String::new(),
            color,
            workspace_id: -1,
            is_pinned_to_desktop: false,
            is_always_on_top: false,
            is_locked: false,
            created_at: Some(now),
            updated_at: Some(now),
            reminders: Vec::new(),
        }
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

/// Serialize the default workspace sentinel (`-1`).
fn default_workspace() -> i64 {
    -1
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

/// Split `---\n{yaml}\n---\n{body}`; `None` without the opening
/// delimiter. The closing delimiter is the first line that is exactly
/// `---` after the opening one, so the body may contain `---` lines.
fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("---\n")?;
    let (yaml, body) = rest.split_once("\n---\n")?;
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
            snooze_count: 2,
        });
        let body = "first line\nsecond line\n\n---\nnot a delimiter\n";
        let rendered = render_file(&note, body).unwrap();
        assert!(rendered.starts_with("---\n"));

        let (yaml, parsed_body) = split_frontmatter(&rendered).unwrap();
        let parsed: Note = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(parsed.id, note.id);
        assert_eq!(parsed.color, NoteColor::Blue);
        assert_eq!(parsed.reminders[0].recurrence_rule, Recurrence::Custom(5));
        assert_eq!(parsed.reminders[0].snooze_count, 2);
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

        assert!(serde_yaml_ng::from_str::<Recurrence>("garbage").is_err());
    }

    #[test]
    fn color_wire_format() {
        let yaml = serde_yaml_ng::to_string(&NoteColor::Yellow).unwrap();
        assert_eq!(yaml.trim(), "Yellow");

        let custom: NoteColor = serde_yaml_ng::from_str("\"#ff00aa\"").unwrap();
        assert_eq!(custom, NoteColor::Custom("#ff00aa".to_owned()));

        let unknown: NoteColor = serde_yaml_ng::from_str("\"anything\"").unwrap();
        assert_eq!(unknown, NoteColor::Custom("anything".to_owned()));
    }

    #[test]
    fn derive_title_uses_first_non_empty_line() {
        assert_eq!(Note::derive_title("\n\n  Groceries  \nmore"), "Groceries");
        assert_eq!(Note::derive_title(""), "");
        assert_eq!(Note::derive_title("   \n  \n"), "");
    }
}
