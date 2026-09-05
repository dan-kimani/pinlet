//! In-memory note index backing quick search (spec §3.8) and, later,
//! the GNOME Shell search provider.
//!
//! Notes are few and small, so the index is a plain map plus a
//! ranked scan per query — no external search engine needed at this
//! scale.

use std::collections::HashMap;

use uuid::Uuid;

/// A ranked search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// Note id.
    pub id: Uuid,
    /// Note title.
    pub title: String,
    /// Short context snippet containing the first match.
    pub snippet: String,
}

/// Index of all live notes, updated by the app core on every save
/// and delete.
#[derive(Debug, Clone, Default)]
pub struct SearchIndex {
    entries: HashMap<Uuid, Entry>,
}

#[derive(Debug, Clone)]
struct Entry {
    title: String,
    body: String,
}

impl SearchIndex {
    /// Insert or replace the index entry for a note.
    pub fn update(&mut self, id: Uuid, title: &str, body: &str) {
        self.entries.insert(
            id,
            Entry {
                title: title.to_owned(),
                body: body.to_owned(),
            },
        );
    }

    /// Drop a note from the index.
    pub fn remove(&mut self, id: Uuid) {
        self.entries.remove(&id);
    }

    /// Ranked, case-insensitive search; title matches rank above
    /// body matches, then alphabetical by title.
    pub fn query(&self, needle: &str, limit: usize) -> Vec<SearchHit> {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }

        let mut hits: Vec<(u8, SearchHit)> = self
            .entries
            .iter()
            .filter_map(|(id, entry)| {
                let title = entry.title.to_lowercase();
                let body = entry.body.to_lowercase();
                let rank = if title.contains(&needle) {
                    0
                } else if body.contains(&needle) {
                    1
                } else {
                    return None;
                };
                Some((
                    rank,
                    SearchHit {
                        id: *id,
                        title: entry.title.clone(),
                        snippet: snippet_for(&title, &body, &needle),
                    },
                ))
            })
            .collect();

        hits.sort_by_key(|(rank, hit)| (*rank, hit.title.to_lowercase()));
        hits.truncate(limit);
        hits.into_iter().map(|(_, hit)| hit).collect()
    }
}

/// The first body line containing the needle, truncated to 72 chars.
/// Falls back to the title for title-only matches.
fn snippet_for(title: &str, body: &str, needle: &str) -> String {
    for line in body.lines() {
        if line.to_lowercase().contains(needle) {
            let trimmed: String = line.trim().chars().take(72).collect();
            if line.trim().chars().count() > 72 {
                return format!("{trimmed}…");
            }
            return trimmed;
        }
    }
    if title.contains(needle) {
        title.trim().to_owned()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    #[test]
    fn ranks_title_matches_above_body_matches() {
        let mut index = SearchIndex::default();
        index.update(note_id(1), "Groceries", "milk, eggs, bread");
        index.update(note_id(2), "Work log", "discuss groceries budget");

        let hits = index.query("groceries", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, note_id(1)); // title match first
        assert_eq!(hits[1].id, note_id(2)); // body match second
    }

    #[test]
    fn is_case_insensitive_and_limited() {
        let mut index = SearchIndex::default();
        index.update(note_id(1), "Alpha", "one");
        index.update(note_id(2), "Beta", "two");
        index.update(note_id(3), "Gamma", "three");

        // Uppercase query matches all three; the limit trims to two.
        let hits = index.query("A", 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn remove_drops_entries() {
        let mut index = SearchIndex::default();
        index.update(note_id(1), "Keep me", "content");
        index.update(note_id(2), "Drop me", "content");
        index.remove(note_id(2));

        let hits = index.query("content", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, note_id(1));
    }

    #[test]
    fn snippet_comes_from_matching_line() {
        let mut index = SearchIndex::default();
        index.update(note_id(1), "Title", "first line\nsecond line has the needle here\nthird");
        let hits = index.query("needle", 10);
        assert_eq!(hits[0].snippet, "second line has the needle here");
    }
}
