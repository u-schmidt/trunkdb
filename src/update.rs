use crate::document::{Document, ID_FIELD};
use indexmap::IndexMap;
use std::cmp::Ordering;

/// Changes to make to every document a filter finds, named by field
/// instead of written as a closure (SPEC §68, §73): MongoDB's `$set`,
/// `$unset`, `$inc`, `$min`, `$max`, `$rename`, and `$push`, `$addToSet`
/// and `$pull` on arrays, on dotted paths (§31). For
/// `Collection::update_fields`.
///
/// ```
/// use trunkdb::query::Update;
///
/// let start = Update::new().set("status", "Running").inc("tries", 1).unset("error");
/// let tag = Update::new().add_to_set("tags", "urgent").pull("tags", "later");
/// # let _ = (start, tag);
/// ```
///
/// The changes apply in the order they were added, each to what the one
/// before left. Values convert as in `Filter`'s builder.
#[derive(Debug, Clone, Default)]
pub struct Update {
    changes: Vec<Change>,
}

#[derive(Debug, Clone)]
enum Change {
    Set(String, Document),
    Unset(String),
    Inc(String, Document),
    Min(String, Document),
    Max(String, Document),
    Rename(String, String),
    Push(String, Document),
    AddToSet(String, Document),
    Pull(String, Document),
}

impl Change {
    /// The paths the change names: one, or a rename's two.
    fn paths(&self) -> Vec<&str> {
        match self {
            Change::Set(path, _)
            | Change::Unset(path)
            | Change::Inc(path, _)
            | Change::Min(path, _)
            | Change::Max(path, _)
            | Change::Push(path, _)
            | Change::AddToSet(path, _)
            | Change::Pull(path, _) => vec![path],
            Change::Rename(from, to) => vec![from, to],
        }
    }
}

impl Update {
    /// Changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// `path` becomes `value`. Objects missing on the way are created;
    /// anything else on the way (a number, a string, null, an array)
    /// fails the update.
    pub fn set(mut self, path: impl Into<String>, value: impl Into<Document>) -> Self {
        self.changes.push(Change::Set(path.into(), value.into()));
        self
    }

    /// The field at `path` is removed. Nothing there, or no object on the
    /// way to it, is no change.
    pub fn unset(mut self, path: impl Into<String>) -> Self {
        self.changes.push(Change::Unset(path.into()));
        self
    }

    /// The number at `path` grows by `by`, an integer or a float: two
    /// integers stay an integer (and overflowing `i64` fails the update),
    /// anything with a float is a float. A missing field becomes `by`,
    /// as `set` would make it. Anything but a number there fails the
    /// update, null included.
    pub fn inc(mut self, path: impl Into<String>, by: impl Into<Document>) -> Self {
        self.changes.push(Change::Inc(path.into(), by.into()));
        self
    }

    /// `path` becomes `value` if `value` is less than what's there, or
    /// nothing or null is there (SPEC §73.2). Values compare as filters
    /// compare them: numbers by exact value, `Int` against `Float` too,
    /// strings, bools, ids and date-times each among their own kind.
    /// Something there that doesn't compare with `value`, a string
    /// against a number say, fails the update. `value` must be one of
    /// those kinds, and not NaN.
    pub fn min(mut self, path: impl Into<String>, value: impl Into<Document>) -> Self {
        self.changes.push(Change::Min(path.into(), value.into()));
        self
    }

    /// `min`, the other way: `path` becomes `value` if `value` is
    /// greater than what's there, or nothing or null is there. "The
    /// latest": `max("last_seen", DateTime::now())`.
    pub fn max(mut self, path: impl Into<String>, value: impl Into<Document>) -> Self {
        self.changes.push(Change::Max(path.into(), value.into()));
        self
    }

    /// The field at `from` moves to `to`, as `unset(from)` then `set(to,
    /// <its value>)`: it's added at the end of its new object, replacing
    /// whatever `to` held, and objects missing on the way are created.
    /// Nothing at `from` is no change, and leaves `to` alone. `from` and
    /// `to` must differ, and neither may lie inside the other.
    pub fn rename(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.changes.push(Change::Rename(from.into(), to.into()));
        self
    }

    /// `value` is added to the end of the array at `path`. A missing
    /// field becomes `[value]`; anything but an array there fails the
    /// update, null included.
    pub fn push(mut self, path: impl Into<String>, value: impl Into<Document>) -> Self {
        self.changes.push(Change::Push(path.into(), value.into()));
        self
    }

    /// `push`, unless the array already holds a value equal to `value`
    /// (SPEC §73.3): a set of tags, say. Equal as `pull` sees it.
    pub fn add_to_set(mut self, path: impl Into<String>, value: impl Into<Document>) -> Self {
        self.changes
            .push(Change::AddToSet(path.into(), value.into()));
        self
    }

    /// Every element equal to `value` is removed from the array at
    /// `path`, the others keeping their order (SPEC §73.3). Equal as a
    /// filter's `eq` sees it (`1` and `1.0` are equal), and arrays,
    /// objects and binary by their contents; NaN equals nothing. Nothing
    /// at `path` is no change; anything but an array there fails the
    /// update, null included.
    pub fn pull(mut self, path: impl Into<String>, value: impl Into<Document>) -> Self {
        self.changes.push(Change::Pull(path.into(), value.into()));
        self
    }

    /// What's wrong with the update as written, before any document is
    /// read: a path that's empty, has an empty step, uses brackets (no
    /// `[*]`: an update names one field), or reaches into `_id`, which no
    /// update changes; an `inc` by something that isn't a number; a `min`
    /// or `max` by something nothing compares with; a `rename` to the
    /// same field, or into or out of itself.
    pub(crate) fn check(&self) -> Result<(), String> {
        for change in &self.changes {
            for path in change.paths() {
                check_path(path)?;
            }
            match change {
                Change::Inc(path, by) if !matches!(by, Document::Int(_) | Document::Float(_)) => {
                    return Err(format!("{path:?}: `inc` needs a number, not {by:?}"));
                }
                Change::Min(path, value) | Change::Max(path, value) if !orders(value) => {
                    return Err(format!(
                        "{path:?}: `min` and `max` need a number, string, bool, id or date-time, not {value:?}"
                    ));
                }
                Change::Rename(from, to) if from == to => {
                    return Err(format!("{from:?}: a field can't be renamed to itself"));
                }
                Change::Rename(from, to)
                    if to.starts_with(&format!("{from}."))
                        || from.starts_with(&format!("{to}.")) =>
                {
                    return Err(format!(
                        "{from:?} to {to:?}: a field can't be renamed into or out of itself"
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Makes the changes to `doc`, in order. On an error, `doc` may be
    /// half changed; the caller drops it with the whole batch.
    pub(crate) fn apply(&self, doc: &mut Document) -> Result<(), String> {
        for change in &self.changes {
            match change {
                Change::Set(path, value) => *slot(doc, path)?.0 = value.clone(),
                Change::Unset(path) => {
                    take(doc, path);
                }
                Change::Inc(path, by) => match slot(doc, path)? {
                    (at, true) => *at = by.clone(),
                    (at, false) => *at = add(at, by).map_err(|e| format!("{path:?}: {e}"))?,
                },
                Change::Min(path, value) => bound(doc, path, value, Ordering::Greater, "min")?,
                Change::Max(path, value) => bound(doc, path, value, Ordering::Less, "max")?,
                Change::Rename(from, to) => {
                    if let Some(value) = take(doc, from) {
                        *slot(doc, to)?.0 = value;
                    }
                }
                Change::Push(path, value) => {
                    array_at(doc, path, "push")?.push(value.clone());
                }
                Change::AddToSet(path, value) => {
                    let items = array_at(doc, path, "add_to_set")?;
                    if !items.iter().any(|item| same(item, value)) {
                        items.push(value.clone());
                    }
                }
                Change::Pull(path, value) => match get_mut(doc, path) {
                    None => {}
                    Some(Document::Array(items)) => items.retain(|item| !same(item, value)),
                    Some(other) => {
                        return Err(format!(
                            "{path:?}: `pull` needs an array there, not {}",
                            kind(other)
                        ));
                    }
                },
            }
        }
        Ok(())
    }
}

/// A path an update may name: one field, not `_id` or inside it.
fn check_path(path: &str) -> Result<(), String> {
    if path.split('.').any(str::is_empty) {
        return Err(format!(
            "{path:?}: an update path needs a name between every two dots"
        ));
    }
    if path.contains(['[', ']']) {
        return Err(format!(
            "{path:?}: an update names one field; `[*]` and other brackets aren't paths here"
        ));
    }
    if path.split('.').next() == Some(ID_FIELD) {
        return Err(format!("{path:?}: a document's `_id` can't be updated"));
    }
    Ok(())
}

/// Whether `min` and `max` can compare `value` with anything: the kinds
/// filters order (SPEC §34.1), NaN aside.
fn orders(value: &Document) -> bool {
    match value {
        Document::Float(f) => !f.is_nan(),
        Document::Int(_)
        | Document::String(_)
        | Document::Bool(_)
        | Document::Id(_)
        | Document::DateTime(_) => true,
        _ => false,
    }
}

/// `min` (`replace_if` `Greater`) and `max` (`Less`): `value` replaces
/// what's at `path` if that compares `replace_if` to it, or is missing
/// or null.
fn bound(
    doc: &mut Document,
    path: &str,
    value: &Document,
    replace_if: Ordering,
    op: &str,
) -> Result<(), String> {
    let (at, missing) = slot(doc, path)?;
    if missing || *at == Document::Null {
        *at = value.clone();
        return Ok(());
    }
    match crate::query::compare(at, value) {
        Some(ordering) if ordering == replace_if => *at = value.clone(),
        Some(_) => {}
        None => {
            return Err(format!(
                "{path:?}: `{op}` can't compare {} there with {}",
                kind(at),
                kind(value)
            ));
        }
    }
    Ok(())
}

/// The array at `path`, for `push` and `add_to_set`: made empty if the
/// field is missing; anything else there is an error.
fn array_at<'a>(
    doc: &'a mut Document,
    path: &str,
    op: &str,
) -> Result<&'a mut Vec<Document>, String> {
    let (at, missing) = slot(doc, path)?;
    if missing {
        *at = Document::Array(Vec::new());
    }
    match at {
        Document::Array(items) => Ok(items),
        other => Err(format!(
            "{path:?}: `{op}` needs an array there, not {}",
            kind(other)
        )),
    }
}

/// Equal, for `add_to_set` and `pull`: as a filter's `eq` sees scalars
/// (`crate::query::equal`), and arrays, objects and binary by their
/// contents, an object's fields in any order, as `Document`'s `==`
/// sees them.
fn same(a: &Document, b: &Document) -> bool {
    match (a, b) {
        (Document::Array(x), Document::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Document::Object(x), Document::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, value)| y.get(key).is_some_and(|other| same(value, other)))
        }
        (Document::Binary(x), Document::Binary(y)) => x == y,
        _ => crate::query::equal(a, b),
    }
}

/// The value at `path`, to change, and whether it was missing: then it's
/// created as `Null`, with every object missing on the way. A step that
/// isn't an object is an error: an update doesn't turn a value into an
/// object.
fn slot<'a>(doc: &'a mut Document, path: &str) -> Result<(&'a mut Document, bool), String> {
    let (parents, name) = match path.rsplit_once('.') {
        Some((parents, name)) => (Some(parents), name),
        None => (None, path),
    };
    let mut object = object_at(doc, "", "the document")?;
    for step in parents.into_iter().flat_map(|p| p.split('.')) {
        let child = object
            .entry(step.to_string())
            .or_insert_with(|| Document::Object(IndexMap::new()));
        object = object_at(child, path, step)?;
    }
    match object.entry(name.to_string()) {
        indexmap::map::Entry::Occupied(entry) => Ok((entry.into_mut(), false)),
        indexmap::map::Entry::Vacant(entry) => Ok((entry.insert(Document::Null), true)),
    }
}

fn object_at<'a>(
    value: &'a mut Document,
    path: &str,
    what: &str,
) -> Result<&'a mut IndexMap<String, Document>, String> {
    match value {
        Document::Object(map) => Ok(map),
        other => {
            let at = if path.is_empty() {
                String::new()
            } else {
                format!("{path:?}: ")
            };
            Err(format!("{at}{what} is {}, not an object", kind(other)))
        }
    }
}

/// The value at `path`, if there's one, through objects only.
fn get_mut<'a>(doc: &'a mut Document, path: &str) -> Option<&'a mut Document> {
    let mut value = doc;
    for step in path.split('.') {
        match value {
            Document::Object(map) => value = map.get_mut(step)?,
            _ => return None,
        }
    }
    Some(value)
}

/// Removes the field at `path` and returns it, keeping the others in
/// their order. Nothing there, or no object on the way: `None`.
fn take(doc: &mut Document, path: &str) -> Option<Document> {
    let (parent, name) = match path.rsplit_once('.') {
        Some((parents, name)) => (get_mut(doc, parents)?, name),
        None => (doc, path),
    };
    match parent {
        Document::Object(map) => map.shift_remove(name),
        _ => None,
    }
}

/// `at + by` for `inc`, `by` a number (`Update::check`).
fn add(at: &Document, by: &Document) -> Result<Document, String> {
    Ok(match (at, by) {
        (Document::Int(a), Document::Int(b)) => Document::Int(
            a.checked_add(*b)
                .ok_or_else(|| format!("{a} + {b} overflows a 64-bit integer"))?,
        ),
        (Document::Int(a), Document::Float(b)) => Document::Float(*a as f64 + b),
        (Document::Float(a), Document::Int(b)) => Document::Float(a + *b as f64),
        (Document::Float(a), Document::Float(b)) => Document::Float(a + b),
        (other, _) => return Err(format!("`inc` needs a number there, not {}", kind(other))),
    })
}

fn kind(value: &Document) -> &'static str {
    match value {
        Document::Null => "null",
        Document::Bool(_) => "a bool",
        Document::Int(_) => "an integer",
        Document::Float(_) => "a float",
        Document::String(_) => "a string",
        Document::Binary(_) => "binary",
        Document::Array(_) => "an array",
        Document::Object(_) => "an object",
        Document::Id(_) => "an id",
        Document::DateTime(_) => "a date-time",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocId;

    fn doc(json: &str) -> Document {
        crate::json::from_json(serde_json::from_str(json).unwrap()).unwrap()
    }

    fn applied(update: Update, to: &str) -> Result<Document, String> {
        update.check()?;
        let mut d = doc(to);
        update.apply(&mut d).map(|()| d)
    }

    #[test]
    fn set_replaces_adds_and_makes_the_objects_on_the_way() {
        let set = |path: &str, to: &str| applied(Update::new().set(path, 7), to).unwrap();
        assert_eq!(set("a", r#"{"a": 1, "b": 2}"#), doc(r#"{"a": 7, "b": 2}"#));
        assert_eq!(set("c", r#"{"a": 1}"#), doc(r#"{"a": 1, "c": 7}"#));
        assert_eq!(
            set("x.y.z", r#"{"a": 1}"#),
            doc(r#"{"a": 1, "x": {"y": {"z": 7}}}"#)
        );
        assert_eq!(
            set("x.y", r#"{"x": {"w": 0}}"#),
            doc(r#"{"x": {"w": 0, "y": 7}}"#)
        );
        assert_eq!(set("a", r#"{"a": {"deep": [1]}}"#), doc(r#"{"a": 7}"#));
    }

    #[test]
    fn set_through_anything_but_an_object_fails() {
        for to in [
            r#"{"a": 3}"#,
            r#"{"a": null}"#,
            r#"{"a": [1]}"#,
            r#"{"a": "s"}"#,
        ] {
            let error = applied(Update::new().set("a.b", 1), to).unwrap_err();
            assert!(error.contains("not an object"), "{to}: {error}");
        }
        let error = applied(Update::new().set("a", 1), "[1, 2]").unwrap_err();
        assert!(error.contains("the document is an array"), "{error}");
    }

    #[test]
    fn unset_removes_keeps_the_order_and_ignores_what_isnt_there() {
        let unset = |path: &str, to: &str| applied(Update::new().unset(path), to).unwrap();
        // `==` on objects ignores the order, so the keys are compared.
        let keys = |d: Document| match d {
            Document::Object(map) => map.into_keys().collect::<Vec<_>>(),
            _ => unreachable!(),
        };
        let after = unset("b", r#"{"a": 1, "b": 2, "c": 3, "d": 4}"#);
        assert_eq!(after, doc(r#"{"a": 1, "c": 3, "d": 4}"#));
        assert_eq!(keys(after), ["a", "c", "d"]);
        assert_eq!(
            unset("x.y", r#"{"x": {"y": 1, "z": 2}}"#),
            doc(r#"{"x": {"z": 2}}"#)
        );
        for same in [r#"{"a": 1}"#, r#"{"x": 5}"#, r#"{"x": [{"y": 1}]}"#, "3"] {
            assert_eq!(unset("x.y", same), doc(same), "{same}");
        }
    }

    #[test]
    fn inc_adds_integers_and_floats_and_starts_a_missing_field() {
        let inc = |by: Document, to: &str| applied(Update::new().inc("n", by), to);
        assert_eq!(inc(2.into(), r#"{"n": 40}"#).unwrap(), doc(r#"{"n": 42}"#));
        assert_eq!(
            inc((-1).into(), r#"{"n": 0}"#).unwrap(),
            doc(r#"{"n": -1}"#)
        );
        assert_eq!(
            inc(0.5.into(), r#"{"n": 1}"#).unwrap(),
            doc(r#"{"n": 1.5}"#)
        );
        assert_eq!(
            inc(1.into(), r#"{"n": 1.5}"#).unwrap(),
            doc(r#"{"n": 2.5}"#)
        );
        assert_eq!(
            inc(0.25.into(), r#"{"n": 0.5}"#).unwrap(),
            doc(r#"{"n": 0.75}"#)
        );
        assert_eq!(
            inc(3.into(), r#"{"m": 1}"#).unwrap(),
            doc(r#"{"m": 1, "n": 3}"#)
        );
        let nested = applied(Update::new().inc("a.n", 1), "{}").unwrap();
        assert_eq!(nested, doc(r#"{"a": {"n": 1}}"#));

        let overflow = inc(1.into(), &format!(r#"{{"n": {}}}"#, i64::MAX)).unwrap_err();
        assert!(overflow.contains("overflows"), "{overflow}");
        for not_a_number in [
            r#"{"n": null}"#,
            r#"{"n": "1"}"#,
            r#"{"n": true}"#,
            r#"{"n": [1]}"#,
        ] {
            let error = inc(1.into(), not_a_number).unwrap_err();
            assert!(
                error.contains("needs a number there"),
                "{not_a_number}: {error}"
            );
        }
    }

    /// SPEC §73.2: `min` and `max` replace a greater or lesser value,
    /// or a missing or null one; numbers across `Int` and `Float`.
    #[test]
    fn min_and_max_keep_the_lesser_and_the_greater() {
        let min = |value: Document, to: &str| applied(Update::new().min("n", value), to);
        let max = |value: Document, to: &str| applied(Update::new().max("n", value), to);
        assert_eq!(min(3.into(), r#"{"n": 5}"#).unwrap(), doc(r#"{"n": 3}"#));
        assert_eq!(min(7.into(), r#"{"n": 5}"#).unwrap(), doc(r#"{"n": 5}"#));
        assert_eq!(max(7.into(), r#"{"n": 5}"#).unwrap(), doc(r#"{"n": 7}"#));
        assert_eq!(max(3.into(), r#"{"n": 5}"#).unwrap(), doc(r#"{"n": 5}"#));
        // Equal: no change, so an `Int` isn't swapped for an equal `Float`.
        assert_eq!(max(5.0.into(), r#"{"n": 5}"#).unwrap(), doc(r#"{"n": 5}"#));
        assert!(matches!(
            max(5.0.into(), r#"{"n": 5}"#).unwrap(),
            Document::Object(m) if m["n"] == Document::Int(5) && matches!(m["n"], Document::Int(_))
        ));
        assert_eq!(
            min(2.5.into(), r#"{"n": 3}"#).unwrap(),
            doc(r#"{"n": 2.5}"#)
        );
        assert_eq!(max(4.into(), r#"{"n": 3.5}"#).unwrap(), doc(r#"{"n": 4}"#));
        assert_eq!(
            min(1.into(), r#"{"m": 0}"#).unwrap(),
            doc(r#"{"m": 0, "n": 1}"#)
        );
        assert_eq!(max(1.into(), r#"{"n": null}"#).unwrap(), doc(r#"{"n": 1}"#));
        assert_eq!(min(1.into(), r#"{"n": null}"#).unwrap(), doc(r#"{"n": 1}"#));
        assert_eq!(
            max("b".into(), r#"{"n": "a"}"#).unwrap(),
            doc(r#"{"n": "b"}"#)
        );
        let early: crate::DateTime = "2026-01-01T00:00:00Z".parse().unwrap();
        let late: crate::DateTime = "2026-09-27T00:00:00Z".parse().unwrap();
        let seen = |at: crate::DateTime| {
            let mut map = IndexMap::new();
            map.insert("n".to_string(), Document::DateTime(at));
            Document::Object(map)
        };
        let mut d = seen(early);
        Update::new()
            .max("n", Document::DateTime(late))
            .apply(&mut d)
            .unwrap();
        assert_eq!(d, seen(late));
        Update::new()
            .min("n", Document::DateTime(late))
            .apply(&mut d)
            .unwrap();
        assert_eq!(d, seen(late));

        for (value, to) in [
            (Document::from(1), r#"{"n": "1"}"#),
            (Document::from("1"), r#"{"n": 1}"#),
            (Document::from(true), r#"{"n": 1}"#),
            (Document::from(1), r#"{"n": [1]}"#),
            (Document::from(1), r#"{"n": {"$float": "NaN"}}"#),
        ] {
            let error = max(value, to).unwrap_err();
            assert!(error.contains("`max` can't compare"), "{to}: {error}");
        }
        let error = min(1.into(), r#"{"n": "x"}"#).unwrap_err();
        assert!(
            error.contains("`min` can't compare a string there with an integer"),
            "{error}"
        );
    }

    /// SPEC §73.1: `rename` moves a field, to the end of its new object,
    /// over whatever was there; a missing field is no change.
    #[test]
    fn rename_moves_a_field() {
        let rename =
            |from: &str, to: &str, doc_: &str| applied(Update::new().rename(from, to), doc_);
        let keys = |d: &Document| match d {
            Document::Object(map) => map.keys().cloned().collect::<Vec<_>>(),
            _ => unreachable!(),
        };
        let moved = rename("a", "z", r#"{"a": 1, "b": 2}"#).unwrap();
        assert_eq!(moved, doc(r#"{"b": 2, "z": 1}"#));
        assert_eq!(keys(&moved), ["b", "z"]);
        assert_eq!(
            rename("a", "b", r#"{"a": 1, "b": 2}"#).unwrap(),
            doc(r#"{"b": 1}"#)
        );
        assert_eq!(
            rename("x.y", "w.v", r#"{"x": {"y": [1], "k": 0}}"#).unwrap(),
            doc(r#"{"x": {"k": 0}, "w": {"v": [1]}}"#)
        );
        assert_eq!(
            rename("x", "o.x", r#"{"x": 1, "o": {}}"#).unwrap(),
            doc(r#"{"o": {"x": 1}}"#)
        );
        for same in [r#"{"b": 2}"#, r#"{"a": null}"#, "[1]"] {
            let expected = if same.contains("null") {
                r#"{"b": null}"#
            } else {
                same
            };
            assert_eq!(rename("a", "b", same).unwrap(), doc(expected), "{same}");
        }
        let error = rename("a", "b.c", r#"{"a": 1, "b": 2}"#).unwrap_err();
        assert!(error.contains("not an object"), "{error}");
    }

    /// SPEC §73.3: `push` appends, `add_to_set` appends what isn't there
    /// yet, `pull` removes every equal element; a missing field starts
    /// an array or is left alone; anything but an array fails.
    #[test]
    fn push_add_to_set_and_pull_change_arrays() {
        let with = |update: Update, to: &str| applied(update, to);
        assert_eq!(
            with(Update::new().push("t", "c"), r#"{"t": ["a", "b"]}"#).unwrap(),
            doc(r#"{"t": ["a", "b", "c"]}"#)
        );
        assert_eq!(
            with(
                Update::new().push("t", "a").push("t", "a"),
                r#"{"t": ["a"]}"#
            )
            .unwrap(),
            doc(r#"{"t": ["a", "a", "a"]}"#)
        );
        assert_eq!(
            with(Update::new().push("o.t", 1), "{}").unwrap(),
            doc(r#"{"o": {"t": [1]}}"#)
        );
        assert_eq!(
            with(Update::new().add_to_set("t", "b"), r#"{"t": ["a", "b"]}"#).unwrap(),
            doc(r#"{"t": ["a", "b"]}"#)
        );
        assert_eq!(
            with(Update::new().add_to_set("t", "c"), r#"{"t": ["a", "b"]}"#).unwrap(),
            doc(r#"{"t": ["a", "b", "c"]}"#)
        );
        assert_eq!(
            with(Update::new().add_to_set("t", 1.0), r#"{"t": [1]}"#).unwrap(),
            doc(r#"{"t": [1]}"#)
        );
        assert_eq!(
            with(Update::new().add_to_set("t", 1), "{}").unwrap(),
            doc(r#"{"t": [1]}"#)
        );
        let keys =
            r#"{"t": [{"a": 1, "b": 2}, {"a": 1}, [1, 2], {"$binary": "AAE="}, 1, 1.0, "1"]}"#;
        let pulled = |value: Document| with(Update::new().pull("t", value), keys).unwrap();
        assert_eq!(
            pulled(doc(r#"{"b": 2, "a": 1}"#)),
            doc(r#"{"t": [{"a": 1}, [1, 2], {"$binary": "AAE="}, 1, 1.0, "1"]}"#)
        );
        assert_eq!(
            pulled(doc("[1, 2]")),
            doc(r#"{"t": [{"a": 1, "b": 2}, {"a": 1}, {"$binary": "AAE="}, 1, 1.0, "1"]}"#)
        );
        assert_eq!(
            pulled(Document::Binary(vec![0, 1])),
            doc(r#"{"t": [{"a": 1, "b": 2}, {"a": 1}, [1, 2], 1, 1.0, "1"]}"#)
        );
        assert_eq!(
            pulled(1.into()),
            doc(r#"{"t": [{"a": 1, "b": 2}, {"a": 1}, [1, 2], {"$binary": "AAE="}, "1"]}"#)
        );
        assert_eq!(pulled(doc("[2, 1]")), doc(keys), "arrays in order");
        assert_eq!(pulled(doc("[1]")), doc(keys));
        assert_eq!(
            with(
                Update::new().pull("t", Document::Float(f64::NAN)),
                r#"{"t": [{"$float": "NaN"}]}"#
            )
            .map(|d| format!("{d:?}")),
            Ok(format!("{:?}", doc(r#"{"t": [{"$float": "NaN"}]}"#)))
        );
        for same in [r#"{"a": 1}"#, r#"{"x": 1}"#, r#"{"t": []}"#] {
            assert_eq!(
                with(Update::new().pull("t.u", 1), same).unwrap(),
                doc(same),
                "{same}"
            );
        }

        for (update, op) in [
            (Update::new().push("t", 1), "`push`"),
            (Update::new().add_to_set("t", 1), "`add_to_set`"),
            (Update::new().pull("t", 1), "`pull`"),
        ] {
            for to in [r#"{"t": null}"#, r#"{"t": "a"}"#, r#"{"t": {"a": 1}}"#] {
                let error = with(update.clone(), to).unwrap_err();
                assert!(
                    error.contains(&format!("{op} needs an array there")),
                    "{to}: {error}"
                );
            }
        }
    }

    /// Each change sees what the ones before it left.
    #[test]
    fn changes_apply_in_order() {
        let update = Update::new()
            .set("n", 1)
            .inc("n", 1)
            .set("a", Document::Object(IndexMap::new()))
            .set("a.b", "x")
            .unset("gone")
            .inc("n", 10);
        let after = applied(update, r#"{"gone": 1, "n": 99}"#).unwrap();
        assert_eq!(after, doc(r#"{"n": 12, "a": {"b": "x"}}"#));
        assert_eq!(
            applied(Update::new(), r#"{"a": 1}"#).unwrap(),
            doc(r#"{"a": 1}"#)
        );
    }

    #[test]
    fn paths_and_increments_are_checked_before_anything_is_read() {
        let refused = |update: Update| update.check().unwrap_err();
        for path in ["", ".", "a.", ".a", "a..b"] {
            assert!(
                refused(Update::new().set(path, 1)).contains("a name between"),
                "{path:?}"
            );
        }
        for path in ["tags[*]", "a[0]", "a.[*]", "x]"] {
            assert!(
                refused(Update::new().unset(path)).contains("brackets"),
                "{path:?}"
            );
        }
        for path in ["_id", "_id.x"] {
            assert!(
                refused(Update::new().set(path, 1)).contains("`_id`"),
                "{path:?}"
            );
        }
        assert!(refused(Update::new().inc("n", "1")).contains("needs a number"));
        assert!(refused(Update::new().inc("n", Document::Null)).contains("needs a number"));
        assert!(
            refused(Update::new().set("ok", 1).inc("n", DocId::NIL)).contains("needs a number")
        );
        for bad in [
            Document::Null,
            Document::Float(f64::NAN),
            Document::Array(vec![]),
            Document::Binary(vec![]),
        ] {
            assert!(refused(Update::new().min("n", bad.clone())).contains("need a number, string"));
            assert!(refused(Update::new().max("n", bad)).contains("need a number, string"));
        }
        assert!(refused(Update::new().rename("a", "a")).contains("to itself"));
        for (from, to) in [("a", "a.b"), ("a.b", "a"), ("x.y", "x.y.z")] {
            assert!(
                refused(Update::new().rename(from, to)).contains("into or out of"),
                "{from} {to}"
            );
        }
        assert!(refused(Update::new().rename("_id", "id")).contains("`_id`"));
        assert!(refused(Update::new().rename("id", "_id")).contains("`_id`"));
        assert!(refused(Update::new().push("t[*]", 1)).contains("brackets"));
        assert!(refused(Update::new().pull("", 1)).contains("a name between"));
        assert!(
            Update::new()
                .set("a.b", 1)
                .set("_idx", 2)
                .inc("n", 1.5)
                .rename("a", "ab")
                .rename("a.b", "a.c")
                .min("m", "text")
                .max("m", 1.5)
                .push("t", Document::Null)
                .pull("t", Document::Array(vec![]))
                .check()
                .is_ok()
        );
    }
}
