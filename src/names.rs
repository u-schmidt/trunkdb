//! Field names stored once (SPEC §170): the file's dictionary of the names its
//! documents use, each a small number, so a stored document's keys are
//! numbers into it instead of the names themselves.
//!
//! One dictionary for the file, kept in the catalog, which writes each
//! batch's new names in cells of their own (`catalog.rs`). Only names
//! of at most `MAX_LEN` bytes join it, and only its first `MAX_NAMES`:
//! any other key is stored as it is, inline, so a document whose keys are
//! data, ids or dates, can't grow the dictionary without end.

use std::collections::HashMap;

/// Longest name the dictionary takes, in UTF-8 bytes.
pub(crate) const MAX_LEN: usize = 64;
/// Most names the dictionary holds.
pub(crate) const MAX_NAMES: usize = 4096;

/// The names, by number, and the numbers, by name.
#[derive(Clone, Debug, Default)]
pub(crate) struct FieldNames {
    names: Vec<Box<str>>,
    numbers: HashMap<Box<str>, u32>,
}

/// No names: what a document stored with every key inline is encoded
/// and decoded with.
#[cfg(test)]
pub(crate) static NONE: std::sync::LazyLock<std::sync::Arc<FieldNames>> =
    std::sync::LazyLock::new(Default::default);

impl FieldNames {
    /// The name numbered `number`.
    pub fn name(&self, number: u64) -> Option<&str> {
        self.names.get(usize::try_from(number).ok()?).map(|n| &**n)
    }

    /// The number of `name`, if the dictionary has it.
    pub fn number(&self, name: &str) -> Option<u32> {
        self.numbers.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether `name` can join: short enough, and room left.
    fn takes(&self, name: &str) -> bool {
        name.len() <= MAX_LEN && self.names.len() < MAX_NAMES
    }

    /// Adds `name` as the next number, if it can join and isn't there.
    /// Whether it was added.
    pub fn add(&mut self, name: &str) -> bool {
        if !self.takes(name) || self.numbers.contains_key(name) {
            return false;
        }
        self.numbers.insert(name.into(), self.names.len() as u32);
        self.names.push(name.into());
        true
    }

    /// The keys of `doc`, at every level, the dictionary doesn't have
    /// yet and could take, each once, in the order they come — as many
    /// as there's room for. Allocates nothing when there are none.
    pub fn unknown(&self, doc: &crate::Document) -> Vec<String> {
        let mut found = Vec::new();
        self.unknown_into(doc, &mut found);
        found
    }

    fn unknown_into(&self, doc: &crate::Document, found: &mut Vec<String>) {
        let room = MAX_NAMES.saturating_sub(self.names.len());
        match doc {
            crate::Document::Object(map) => {
                for (key, value) in map {
                    if found.len() < room
                        && key.len() <= MAX_LEN
                        && !self.numbers.contains_key(key.as_str())
                        && !found.contains(key)
                    {
                        found.push(key.clone());
                    }
                    self.unknown_into(value, found);
                }
            }
            crate::Document::Array(items) => {
                for item in items {
                    self.unknown_into(item, found);
                }
            }
            _ => {}
        }
    }

    /// Every name, in number order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(|n| &**n)
    }
}

/// A dictionary holding some of the names tests use, and not others:
/// so their documents are stored with keys of both kinds.
#[cfg(test)]
pub(crate) fn sample() -> FieldNames {
    let mut names = FieldNames::default();
    for name in ["a", "n", "x", "o", "q", "inner", "m", "i", "s", "f", "u"] {
        names.add(name);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    #[test]
    fn names_are_numbered_in_the_order_first_seen_at_every_level() {
        let mut names = FieldNames::default();
        let doc: Document = r#"{"b": 1, "a": {"c": [ {"d": 2} ], "b": 3}}"#.parse().unwrap();
        let unknown = names.unknown(&doc);
        assert_eq!(unknown, ["b", "a", "c", "d"]);
        for name in &unknown {
            assert!(names.add(name));
        }
        assert!(names.unknown(&doc).is_empty(), "known already");
        assert_eq!(names.number("a"), Some(1));
        assert_eq!(names.name(3), Some("d"));
        assert_eq!(names.name(4), None);
        assert_eq!(names.name(u64::MAX), None);
    }

    #[test]
    fn long_names_and_names_past_the_limit_stay_out() {
        let mut names = FieldNames::default();
        // `unknown` takes what `add` takes: a key of `MAX_LEN` bytes, not
        // one more.
        let edge = Document::Object(
            [MAX_LEN, MAX_LEN + 1]
                .map(|len| ("y".repeat(len), Document::Null))
                .into_iter()
                .collect(),
        );
        assert_eq!(names.unknown(&edge), ["y".repeat(MAX_LEN)]);
        assert!(!names.add(&"x".repeat(MAX_LEN + 1)));
        assert!(names.add(&"x".repeat(MAX_LEN)));
        for n in 1..MAX_NAMES {
            assert!(names.add(&n.to_string()));
        }
        assert!(!names.add("one more"));
        assert_eq!(names.len(), MAX_NAMES);
        let doc: Document = r#"{"one more": 1}"#.parse().unwrap();
        assert!(names.unknown(&doc).is_empty(), "no room");
    }
}
