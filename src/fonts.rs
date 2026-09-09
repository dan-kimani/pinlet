//! Note typeface helpers: the installed-font list behind the
//! Preferences font picker, the system monospace default, and the
//! handwriting-face heuristic.
//!
//! Families come from Pango (fontconfig under the hood), so the list
//! always reflects the fonts actually installed on the system. The
//! picker offers monospace families plus handwriting-style faces.

use gtk4::gio::prelude::SettingsExt;
use pango::prelude::*;

/// GSettings schema holding GNOME's monospace font choice.
const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
/// Key inside [`INTERFACE_SCHEMA`] naming the default monospace font.
const MONOSPACE_KEY: &str = "monospace-font-name";

/// Generic Pango fallback when the desktop names no monospace font.
const FALLBACK_MONO: &str = "Monospace";

/// Single-word tokens (lowercase, split on non-alphanumerics) that
/// mark a family as handwriting-style, e.g. "Gochi Hand" or
/// "Permanent Marker".
const HANDWRITING_TOKENS: &[&str] = &[
    "hand",
    "handwriting",
    "handwritten",
    "script",
    "cursive",
    "marker",
    "chalk",
];

/// Substring fragments (lowercase) catching handwriting families
/// whose name is a single glued word, e.g. "Caveat" or "Pacifico".
const HANDWRITING_NAMES: &[&str] = &[
    "caveat",
    "pacifico",
    "dancing",
    "comic",
    "bradley",
    "chalkboard",
    "chalkduster",
    "kalam",
    "indie flower",
    "shadows into light",
    "lobster",
    "satisfy",
    "amatic",
    "architects daughter",
    "covered by your grace",
    "gochi",
    "handlee",
    "homemade apple",
    "kaushan",
    "marck",
    "nothing you could do",
    "permanent marker",
    "reenie",
    "rock salt",
    "sacramento",
    "allura",
    "great vibes",
    "parisienne",
    "tangerine",
    "yellowtail",
    "rochester",
    "nanum brush",
    "nanum pen",
    "gaegu",
    "gamja flower",
    "poor story",
    "single day",
    "shantell",
    "zeyada",
    "delius",
    "neucha",
    "la belle aurore",
    "loved by the king",
    "henny penny",
];

/// Whether `name` looks like a handwriting-style face. Heuristic
/// over the family name only — fontconfig exposes no style tag for
/// this, so well-known handwriting families match by name.
pub fn is_handwritten_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    if HANDWRITING_NAMES.iter().any(|frag| lower.contains(frag)) {
        return true;
    }
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|token| HANDWRITING_TOKENS.contains(&token))
}

/// Every installed family name, sorted and deduplicated.
/// `context` must come from a widget (a bare `Context::new()` has
/// no font map attached and lists nothing).
pub fn installed_families(context: &pango::Context) -> Vec<String> {
    let mut names: Vec<String> = context
        .list_families()
        .iter()
        .map(|family| family.name().to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Installed monospace families, sorted and deduplicated (see
/// [`installed_families`] for the `context` requirement).
pub fn monospace_families(context: &pango::Context) -> Vec<String> {
    let mut names: Vec<String> = context
        .list_families()
        .iter()
        .filter(|family| family.is_monospace())
        .map(|family| family.name().to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Installed handwriting-style families (see [`is_handwritten_name`]).
pub fn handwritten_families(context: &pango::Context) -> Vec<String> {
    installed_families(context)
        .into_iter()
        .filter(|name| is_handwritten_name(name))
        .collect()
}

/// The families offered by the Preferences picker: monospace plus
/// handwriting faces, sorted and deduplicated. The system monospace
/// default is always present even if Pango flags it oddly.
pub fn note_font_choices(context: &pango::Context) -> Vec<String> {
    let mut choices = monospace_families(context);
    choices.extend(handwritten_families(context));
    let system = system_mono_family();
    if !choices.contains(&system) {
        choices.push(system);
    }
    choices.sort();
    choices.dedup();
    choices
}

/// The desktop's default monospace family: GNOME's
/// `monospace-font-name` with the size stripped ("Fira Mono 11" →
/// "Fira Mono"), else the generic Pango "Monospace". The schema
/// lookup is guarded so non-GNOME desktops fall back cleanly
/// instead of panicking on a missing schema.
pub fn system_mono_family() -> String {
    let family = gio_schema_string(INTERFACE_SCHEMA, MONOSPACE_KEY)
        .map(|value| pango::FontDescription::from_string(&value))
        .and_then(|desc| desc.family().map(|family| family.to_string()))
        .filter(|family| !family.is_empty());
    family.unwrap_or_else(|| FALLBACK_MONO.to_owned())
}

/// Resolve a stored `font_family` setting: empty (never chosen)
/// follows the current system monospace font.
pub fn resolve_font_family(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        system_mono_family()
    } else {
        trimmed.to_owned()
    }
}

/// Escape a family name for a single-quoted CSS string.
pub fn css_escape_family(family: &str) -> String {
    family.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Read one string key, returning `None` when the schema is absent
/// (non-GNOME desktop) or the key is unset.
fn gio_schema_string(schema_id: &str, key: &str) -> Option<String> {
    let source = gtk4::gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(schema_id, true)?;
    let settings =
        gtk4::gio::Settings::new_full(&schema, None::<&gtk4::gio::SettingsBackend>, None);
    let value = settings.string(key).to_string();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handwritten_heuristic_matches_known_faces() {
        for name in [
            "Caveat",
            "Pacifico",
            "Dancing Script",
            "Comic Sans MS",
            "Gochi Hand",
            "Permanent Marker",
            "Shantell Sans",
        ] {
            assert!(is_handwritten_name(name), "{name} should match");
        }
    }

    #[test]
    fn handwritten_heuristic_rejects_plain_faces() {
        for name in ["Fira Mono", "DejaVu Sans", "Cantarell", "Chandas"] {
            assert!(!is_handwritten_name(name), "{name} should not match");
        }
    }

    #[test]
    fn resolve_empty_follows_system() {
        assert!(!resolve_font_family("").is_empty());
        assert!(!resolve_font_family("   ").is_empty());
        assert_eq!(resolve_font_family("Caveat"), "Caveat");
    }

    #[test]
    fn css_escape_keeps_rule_intact() {
        assert_eq!(css_escape_family("Fira Mono"), "Fira Mono");
        assert_eq!(css_escape_family("O'Brien"), "O\\'Brien");
    }

    /// Full-listing coverage (a widget-backed context) lives in
    /// `app::tests::gtk_regressions`. This pins the invariants that
    /// hold for any context, including a bare one with no font map:
    /// sorted, unique, and always containing the system default.
    #[test]
    fn choices_are_sorted_unique_and_include_system_default() {
        let choices = note_font_choices(&pango::Context::new());
        assert!(!choices.is_empty());
        let mut sorted = choices.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(choices, sorted);
        assert!(choices.contains(&system_mono_family()));
    }
}
