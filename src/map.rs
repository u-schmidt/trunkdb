//! `Map`, the fields of an object document (SPEC §116): names to values,
//! in the order they were inserted. A type of trunkdb's own, so the map
//! inside it — an `IndexMap` now — isn't part of trunkdb's API, and can
//! change without breaking a program that uses it.

use crate::document::Document;
use indexmap::IndexMap;
use std::fmt;
use std::iter::FusedIterator;

/// The fields of an object (`Document::Object`, SPEC §116): names to
/// values, kept in the order they were first inserted, as JSON text has
/// them and a document comes back from the database.
///
/// ```
/// use trunkdb::{Document, Map};
///
/// let mut task = Map::new();
/// task.insert("title".into(), "write the spec".into());
/// task.insert("hours".into(), 3.into());
/// task.insert("hours".into(), 4.into()); // keeps its place
/// assert_eq!(task.get("hours"), Some(&Document::Int(4)));
/// assert_eq!(task.keys().collect::<Vec<_>>(), ["title", "hours"]);
///
/// let same = Map::from([("hours", 4.into()), ("title", "write the spec".into())]);
/// assert_eq!(task, same); // the same fields, in another order
/// let doc = Document::Object(task);
/// # let _ = doc;
/// ```
///
/// Two maps are equal when they hold the same names with equal values,
/// in any order.
#[derive(Clone, Default, PartialEq)]
pub struct Map(pub(crate) IndexMap<String, Document>);

impl Map {
    /// An empty map.
    pub fn new() -> Self {
        Map(IndexMap::new())
    }

    /// An empty map with room for `capacity` fields.
    pub fn with_capacity(capacity: usize) -> Self {
        Map(IndexMap::with_capacity(capacity))
    }

    /// How many fields it has.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it has no fields.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value of field `name`, if it has one.
    pub fn get(&self, name: &str) -> Option<&Document> {
        self.0.get(name)
    }

    /// The value of field `name`, to change in place.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Document> {
        self.0.get_mut(name)
    }

    /// Whether it has a field `name`.
    pub fn contains_key(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Sets the field `name` to `value`, and gives back the value it had.
    /// A new field goes last; one already there keeps its place.
    pub fn insert(&mut self, name: String, value: Document) -> Option<Document> {
        self.0.insert(name, value)
    }

    /// Takes the field `name` out, and gives back its value. The fields
    /// after it move up, so the others keep their order.
    pub fn remove(&mut self, name: &str) -> Option<Document> {
        self.0.shift_remove(name)
    }

    /// Keeps only the fields `keep` says yes to, in their order.
    pub fn retain(&mut self, mut keep: impl FnMut(&String, &mut Document) -> bool) {
        self.0.retain(|name, value| keep(name, value));
    }

    /// Removes every field.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The fields, in order.
    pub fn iter(&self) -> Iter<'_> {
        Iter(self.0.iter())
    }

    /// The fields, in order, their values to change.
    pub fn iter_mut(&mut self) -> IterMut<'_> {
        IterMut(self.0.iter_mut())
    }

    /// The names, in order.
    pub fn keys(&self) -> Keys<'_> {
        Keys(self.0.keys())
    }

    /// The values, in order.
    pub fn values(&self) -> Values<'_> {
        Values(self.0.values())
    }

    /// The values, in order, to change.
    pub fn values_mut(&mut self) -> ValuesMut<'_> {
        ValuesMut(self.0.values_mut())
    }
}

/// As the map inside prints it, `{"name": value, …}`, so a `Document`'s
/// `Debug` reads `Object({…})`, as it did before `Map`.
impl fmt::Debug for Map {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The field `name`'s value. Panics if there is none, as `HashMap`'s
/// does; `get` doesn't.
impl std::ops::Index<&str> for Map {
    type Output = Document;

    fn index(&self, name: &str) -> &Document {
        match self.0.get(name) {
            Some(value) => value,
            None => panic!("no field {name:?} in the map"),
        }
    }
}

/// The field `name`'s value, to change: `map["hours"] = 4.into()`.
/// Panics if there is none; `get_mut` doesn't. A new field is
/// `insert`ed.
impl std::ops::IndexMut<&str> for Map {
    fn index_mut(&mut self, name: &str) -> &mut Document {
        match self.0.get_mut(name) {
            Some(value) => value,
            None => panic!("no field {name:?} in the map"),
        }
    }
}

// `String` and `Document`, not anything that converts into them, as
// `insert` takes them: an `.into()` in the pair then knows its type.
impl FromIterator<(String, Document)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, Document)>>(fields: I) -> Self {
        Map(fields.into_iter().collect())
    }
}

impl Extend<(String, Document)> for Map {
    fn extend<I: IntoIterator<Item = (String, Document)>>(&mut self, fields: I) {
        self.0.extend(fields);
    }
}

/// A map written out: `Map::from([("title", "a".into()), ("hours", 3.into())])`.
impl<K: Into<String>, const N: usize> From<[(K, Document); N]> for Map {
    fn from(fields: [(K, Document); N]) -> Self {
        Map(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

impl From<Map> for Document {
    fn from(map: Map) -> Self {
        Document::Object(map)
    }
}

impl IntoIterator for Map {
    type Item = (String, Document);
    type IntoIter = IntoIter;

    fn into_iter(self) -> IntoIter {
        IntoIter(self.0.into_iter())
    }
}

impl<'a> IntoIterator for &'a Map {
    type Item = (&'a String, &'a Document);
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut Map {
    type Item = (&'a String, &'a mut Document);
    type IntoIter = IterMut<'a>;

    fn into_iter(self) -> IterMut<'a> {
        self.iter_mut()
    }
}

/// An iterator of the map's own, around the inner map's, so that one
/// isn't named in trunkdb's API either.
macro_rules! iterator {
    ($(#[$doc:meta])* $name:ident<$($a:lifetime)?>, $inner:ty, $item:ty $(, $derive:ident)*) => {
        $(#[$doc])*
        #[derive(Debug $(, $derive)*)]
        pub struct $name<$($a)?>($inner);

        impl<$($a)?> Iterator for $name<$($a)?> {
            type Item = $item;

            fn next(&mut self) -> Option<$item> {
                self.0.next()
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                self.0.size_hint()
            }
        }

        impl<$($a)?> DoubleEndedIterator for $name<$($a)?> {
            fn next_back(&mut self) -> Option<$item> {
                self.0.next_back()
            }
        }

        impl<$($a)?> ExactSizeIterator for $name<$($a)?> {}

        impl<$($a)?> FusedIterator for $name<$($a)?> {}
    };
}

iterator!(
    /// The fields of a `Map`, in order: `map.iter()`.
    Iter<'a>, indexmap::map::Iter<'a, String, Document>, (&'a String, &'a Document), Clone
);
iterator!(
    /// The fields of a `Map`, in order, their values to change.
    IterMut<'a>, indexmap::map::IterMut<'a, String, Document>, (&'a String, &'a mut Document)
);
iterator!(
    /// The fields of a `Map`, taken out of it, in order.
    IntoIter<>, indexmap::map::IntoIter<String, Document>, (String, Document), Clone
);
iterator!(
    /// The names of a `Map`, in order.
    Keys<'a>, indexmap::map::Keys<'a, String, Document>, &'a String, Clone
);
iterator!(
    /// The values of a `Map`, in order.
    Values<'a>, indexmap::map::Values<'a, String, Document>, &'a Document, Clone
);
iterator!(
    /// The values of a `Map`, in order, to change.
    ValuesMut<'a>, indexmap::map::ValuesMut<'a, String, Document>, &'a mut Document
);

#[cfg(test)]
mod tests {
    use super::*;

    fn names(map: &Map) -> Vec<&str> {
        map.keys().map(String::as_str).collect()
    }

    #[test]
    fn fields_keep_the_order_they_came_in() {
        let mut map = Map::new();
        for (name, n) in [("c", 1), ("a", 2), ("b", 3)] {
            assert_eq!(map.insert(name.into(), n.into()), None);
        }
        assert_eq!(names(&map), ["c", "a", "b"]);
        // Set again: a new value, the old place.
        assert_eq!(map.insert("a".into(), 9.into()), Some(Document::Int(2)));
        assert_eq!(names(&map), ["c", "a", "b"]);
        // Taken out: the others keep their order.
        assert_eq!(map.remove("c"), Some(Document::Int(1)));
        assert_eq!(map.remove("c"), None);
        assert_eq!(names(&map), ["a", "b"]);
        map.insert("d".into(), Document::Null);
        map.retain(|name, _| name != "a");
        assert_eq!(names(&map), ["b", "d"]);
        let values: Vec<_> = map.values().collect();
        assert_eq!(values, [&Document::Int(3), &Document::Null]);
        let pairs: Vec<_> = map.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        assert_eq!(pairs, [("b", Document::Int(3)), ("d", Document::Null)]);
        assert_eq!((map.len(), map.is_empty()), (2, false));
        map.clear();
        assert!(map.is_empty());
    }

    #[test]
    fn values_change_in_place() {
        let mut map = Map::from([("a", Document::Int(1)), ("b", Document::Int(2))]);
        *map.get_mut("a").unwrap() = Document::Int(10);
        map["b"] = Document::Int(20);
        for (_, value) in map.iter_mut() {
            if let Document::Int(n) = value {
                *n += 1;
            }
        }
        for value in map.values_mut() {
            if let Document::Int(n) = value {
                *n *= 2;
            }
        }
        for (_, value) in &mut map {
            if let Document::Int(n) = value {
                *n += 100;
            }
        }
        assert_eq!(
            (map["a"].clone(), map["b"].clone()),
            (Document::Int(122), Document::Int(142))
        );
        assert!(map.contains_key("a") && !map.contains_key("z"));
        assert_eq!(map.get("z"), None);
    }

    #[test]
    fn a_map_is_built_and_taken_apart_in_order() {
        let built: Map = [
            ("x".to_string(), Document::Int(1)),
            ("y".to_string(), Document::Int(2)),
        ]
        .into_iter()
        .collect();
        let mut extended = Map::from([("x", Document::Int(1))]);
        extended.extend([("y".to_string(), Document::Int(2))]);
        assert_eq!(built, extended);
        let owned: Vec<(String, Document)> = built.clone().into_iter().collect();
        assert_eq!(
            owned,
            [
                ("x".into(), Document::Int(1)),
                ("y".into(), Document::Int(2))
            ]
        );
        let borrowed: Vec<&String> = (&built).into_iter().map(|(k, _)| k).collect();
        assert_eq!(borrowed, ["x", "y"]);
        // Both ends, and the count of what's left.
        let mut iter = built.iter();
        assert_eq!(iter.len(), 2);
        assert_eq!(iter.next_back().map(|(k, _)| k.as_str()), Some("y"));
        assert_eq!(iter.len(), 1);
        assert_eq!(built.keys().rev().collect::<Vec<_>>(), ["y", "x"]);
        assert_eq!(Document::from(built.clone()), Document::Object(built));
    }

    #[test]
    fn maps_are_equal_in_any_order_and_print_as_before() {
        let one = Map::from([("a", Document::Int(1)), ("b", Document::Null)]);
        let other = Map::from([("b", Document::Null), ("a", Document::Int(1))]);
        assert_eq!(one, other);
        assert_ne!(one, Map::from([("a", Document::Int(1))]));
        assert_ne!(
            one,
            Map::from([("a", Document::Int(2)), ("b", Document::Null)])
        );
        assert_eq!(format!("{one:?}"), r#"{"a": Int(1), "b": Null}"#);
        assert_eq!(
            format!("{:?}", Document::Object(one)),
            r#"Object({"a": Int(1), "b": Null})"#
        );
        assert_eq!(Map::default(), Map::with_capacity(8));
    }

    #[test]
    #[should_panic(expected = "no field \"z\" in the map")]
    fn a_missing_field_by_index_panics() {
        let _ = &Map::from([("a", Document::Int(1))])["z"];
    }

    #[test]
    #[should_panic(expected = "no field \"z\" in the map")]
    fn a_missing_field_by_index_to_change_panics() {
        Map::new()["z"] = Document::Null;
    }
}
