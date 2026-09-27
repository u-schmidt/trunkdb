use crate::document::{Document, ID_FIELD};
use indexmap::IndexMap;

/// Changes to make to every document a filter finds, named by field
/// instead of written as a closure (SPEC §68): MongoDB's `$set`, `$unset`
/// and `$inc`, on dotted paths (§31). For `Collection::update_fields`.
///
/// ```
/// use trunkdb::query::Update;
///
/// let start = Update::new().set("status", "Running").inc("tries", 1).unset("error");
/// # let _ = start;
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

    /// What's wrong with the update as written, before any document is
    /// read: a path that's empty, has an empty step, uses brackets (no
    /// `[*]`: an update names one field), or reaches into `_id`, which no
    /// update changes; an `inc` by something that isn't a number.
    pub(crate) fn check(&self) -> Result<(), String> {
        for change in &self.changes {
            let (Change::Set(path, _) | Change::Unset(path) | Change::Inc(path, _)) = change;
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
            if let Change::Inc(_, by) = change
                && !matches!(by, Document::Int(_) | Document::Float(_))
            {
                return Err(format!("{path:?}: `inc` needs a number, not {by:?}"));
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
                Change::Unset(path) => unset(doc, path),
                Change::Inc(path, by) => match slot(doc, path)? {
                    (at, true) => *at = by.clone(),
                    (at, false) => *at = add(at, by).map_err(|e| format!("{path:?}: {e}"))?,
                },
            }
        }
        Ok(())
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

/// Removes the field at `path`, keeping the others in their order.
fn unset(doc: &mut Document, path: &str) {
    let (parents, name) = match path.rsplit_once('.') {
        Some((parents, name)) => (Some(parents), name),
        None => (None, path),
    };
    let mut value = doc;
    for step in parents.into_iter().flat_map(|p| p.split('.')) {
        match value {
            Document::Object(map) => match map.get_mut(step) {
                Some(child) => value = child,
                None => return,
            },
            _ => return,
        }
    }
    if let Document::Object(map) = value {
        map.shift_remove(name);
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
        assert!(
            Update::new()
                .set("a.b", 1)
                .set("_idx", 2)
                .inc("n", 1.5)
                .check()
                .is_ok()
        );
    }
}
