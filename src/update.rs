use crate::document::{DocId, Document, ID_FIELD};
use crate::map::Map;
use crate::query::{Condition, Op, equal_values};
use std::cmp::Ordering;

/// Changes to make to every document a filter finds, named by field
/// instead of written as a closure (SPEC §68, §73, §142): MongoDB's
/// `$set`, `$unset`, `$inc`, `$mul`, `$min`, `$max`, `$rename`, and
/// `$push`, `$addToSet`, `$pull` and `$pop` on arrays, on dotted paths
/// (§31). For `Collection::update_fields`.
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
    /// A `set` only where `upsert_fields` inserts (SPEC §144.3).
    SetOnInsert(String, Document),
    Unset(String),
    Inc(String, Document),
    Mul(String, Document),
    Min(String, Document),
    Max(String, Document),
    Rename(String, String),
    /// One `push` or a `push_all`: the values in order.
    Push(String, Vec<Document>),
    AddToSet(String, Document),
    Pull(String, Document),
    /// `pop_first` (`true`) or `pop_last`.
    Pop(String, bool),
}

impl Change {
    /// The paths the change names: one, or a rename's two.
    fn paths(&self) -> Vec<&str> {
        match self {
            Change::Set(path, _)
            | Change::SetOnInsert(path, _)
            | Change::Unset(path)
            | Change::Inc(path, _)
            | Change::Mul(path, _)
            | Change::Min(path, _)
            | Change::Max(path, _)
            | Change::Push(path, _)
            | Change::AddToSet(path, _)
            | Change::Pull(path, _)
            | Change::Pop(path, _) => vec![path],
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

    /// A `set` made only where `upsert_fields` inserts the document,
    /// MongoDB's `$setOnInsert` (SPEC §144.3): a `created` that later
    /// upserts leave as it is. Anywhere else, no change.
    pub fn set_on_insert(mut self, path: impl Into<String>, value: impl Into<Document>) -> Self {
        self.changes
            .push(Change::SetOnInsert(path.into(), value.into()));
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

    /// The number at `path` multiplied by `by`, an integer or a float
    /// (SPEC §142): two integers stay an integer (and overflowing `i64`
    /// fails the update), anything with a float is a float. A missing
    /// field becomes zero, `0` or `0.0` as `by` is, as MongoDB's `$mul`
    /// makes it. Anything but a number there fails the update, null
    /// included.
    pub fn mul(mut self, path: impl Into<String>, by: impl Into<Document>) -> Self {
        self.changes.push(Change::Mul(path.into(), by.into()));
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
        self.changes
            .push(Change::Push(path.into(), vec![value.into()]));
        self
    }

    /// `push` of every value, in order (SPEC §142): MongoDB's `$push`
    /// with `$each`. A missing field becomes an array of them, `[]` if
    /// there are none; anything but an array there fails the update,
    /// even with no values.
    ///
    /// ```
    /// use trunkdb::query::Update;
    ///
    /// let tagged = Update::new().push_all("tags", ["new", "urgent"]);
    /// # let _ = tagged;
    /// ```
    pub fn push_all<V: Into<Document>>(
        mut self,
        path: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let values = values.into_iter().map(Into::into).collect();
        self.changes.push(Change::Push(path.into(), values));
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

    /// The first element of the array at `path` is removed (SPEC
    /// §142): MongoDB's `$pop: -1`, a queue's next item. An empty array,
    /// or nothing at `path`, is no change; anything but an array there
    /// fails the update, null included.
    pub fn pop_first(mut self, path: impl Into<String>) -> Self {
        self.changes.push(Change::Pop(path.into(), true));
        self
    }

    /// `pop_first`, from the other end: the last element, MongoDB's
    /// `$pop: 1`.
    pub fn pop_last(mut self, path: impl Into<String>) -> Self {
        self.changes.push(Change::Pop(path.into(), false));
        self
    }

    /// What's wrong with the update as written, before any document is
    /// read: a path that's empty, has an empty step, uses brackets (no
    /// `[*]`: an update names one field), or reaches into `_id`, which no
    /// update changes; an `inc` or `mul` by something that isn't a number; a `min`
    /// or `max` by something nothing compares with; a `rename` to the
    /// same field, or into or out of itself.
    pub(crate) fn check(&self) -> Result<(), String> {
        for change in &self.changes {
            for path in change.paths() {
                check_path(path)?;
            }
            match change {
                Change::Inc(path, by) if !is_number(by) => {
                    return Err(format!("{path:?}: `inc` needs a number, not {by:?}"));
                }
                Change::Mul(path, by) if !is_number(by) => {
                    return Err(format!("{path:?}: `mul` needs a number, not {by:?}"));
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
        self.apply_as(doc, false)
    }

    /// `apply` to the document `upsert_fields` inserts, `set_on_insert`
    /// included (SPEC §144.2).
    pub(crate) fn apply_inserting(&self, doc: &mut Document) -> Result<(), String> {
        self.apply_as(doc, true)
    }

    fn apply_as(&self, doc: &mut Document, inserting: bool) -> Result<(), String> {
        for change in &self.changes {
            match change {
                Change::Set(path, value) => *slot(doc, path)?.0 = value.clone(),
                Change::SetOnInsert(path, value) => {
                    if inserting {
                        *slot(doc, path)?.0 = value.clone();
                    }
                }
                Change::Unset(path) => {
                    take(doc, path);
                }
                Change::Inc(path, by) => match slot(doc, path)? {
                    (at, true) => *at = by.clone(),
                    (at, false) => *at = add(at, by).map_err(|e| format!("{path:?}: {e}"))?,
                },
                Change::Mul(path, by) => match slot(doc, path)? {
                    (at, true) => *at = zero_like(by),
                    (at, false) => *at = multiply(at, by).map_err(|e| format!("{path:?}: {e}"))?,
                },
                Change::Min(path, value) => bound(doc, path, value, Ordering::Greater, "min")?,
                Change::Max(path, value) => bound(doc, path, value, Ordering::Less, "max")?,
                Change::Rename(from, to) => {
                    if let Some(value) = take(doc, from) {
                        *slot(doc, to)?.0 = value;
                    }
                }
                Change::Push(path, values) => {
                    array_at(doc, path, "push")?.extend(values.iter().cloned());
                }
                Change::AddToSet(path, value) => {
                    let items = array_at(doc, path, "add_to_set")?;
                    if !items.iter().any(|item| equal_values(item, value)) {
                        items.push(value.clone());
                    }
                }
                Change::Pull(path, value) => {
                    if let Some(items) = existing_array(doc, path, "pull")? {
                        items.retain(|item| !equal_values(item, value));
                    }
                }
                Change::Pop(path, first) => {
                    let op = if *first { "pop_first" } else { "pop_last" };
                    if let Some(items) = existing_array(doc, path, op)? {
                        if !*first {
                            items.pop();
                        } else if !items.is_empty() {
                            items.remove(0);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// The document `upsert_fields` starts from where nothing matches
/// (SPEC §144.2): each `eq` of the filter, at the top or in an AND, on a
/// path an update could name, and the id an `eq` on `_id` names. ORs,
/// NOTs and other comparisons say no one value, and are left out. A
/// path given two values that aren't equal, or one inside another's
/// value, can't be made: an error, before anything is read.
pub(crate) fn seed(conditions: &[Condition]) -> Result<(Option<DocId>, Document), String> {
    fn walk<'a>(
        conditions: &'a [Condition],
        id: &mut Option<DocId>,
        doc: &mut Document,
        paths: &mut Vec<&'a str>,
    ) -> Result<(), String> {
        for condition in conditions {
            match condition {
                Condition::All(inner) => walk(inner, id, doc, paths)?,
                // An `_id` that isn't an id names none; nor is it a path
                // an update could name, so it isn't seeded below either.
                Condition::Compare {
                    field,
                    op: Op::Eq,
                    value: Document::Id(named),
                } if field == ID_FIELD => {
                    if let Some(before) = *id
                        && before != *named
                    {
                        return Err(format!(
                            "the filter names two ids, {before:?} and {named:?}"
                        ));
                    }
                    *id = Some(*named);
                }
                Condition::Compare {
                    field,
                    op: Op::Eq,
                    value,
                } if check_path(field).is_ok() => {
                    let inside = |outer: &str, inner: &str| {
                        inner
                            .strip_prefix(outer)
                            .is_some_and(|rest| rest.starts_with('.'))
                    };
                    if let Some(other) = paths
                        .iter()
                        .find(|other| inside(other, field) || inside(field, other))
                    {
                        return Err(format!(
                            "{field:?} and {other:?}: the filter gives a value to a field and to one inside it, so no document to insert"
                        ));
                    }
                    paths.push(field);
                    match slot(doc, field)? {
                        (at, true) => *at = value.clone(),
                        (at, false) if equal_values(at, value) => {}
                        (_, false) => {
                            return Err(format!(
                                "{field:?}: the filter gives it two values, so no document to insert"
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    let mut id = None;
    let mut doc = Document::Object(Map::new());
    walk(conditions, &mut id, &mut doc, &mut Vec::new())?;
    Ok((id, doc))
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
        | Document::DateTime(_)
        | Document::Date(_)
        | Document::Time(_) => true,
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

/// The array at `path`, for `pull` and `pop`: `None` if nothing is
/// there; anything else there is an error.
fn existing_array<'a>(
    doc: &'a mut Document,
    path: &str,
    op: &str,
) -> Result<Option<&'a mut Vec<Document>>, String> {
    match get_mut(doc, path) {
        None => Ok(None),
        Some(Document::Array(items)) => Ok(Some(items)),
        Some(other) => Err(format!(
            "{path:?}: `{op}` needs an array there, not {}",
            kind(other)
        )),
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
            .0
            .entry(step.to_string())
            .or_insert_with(|| Document::Object(Map::new()));
        object = object_at(child, path, step)?;
    }
    match object.0.entry(name.to_string()) {
        indexmap::map::Entry::Occupied(entry) => Ok((entry.into_mut(), false)),
        indexmap::map::Entry::Vacant(entry) => Ok((entry.insert(Document::Null), true)),
    }
}

fn object_at<'a>(value: &'a mut Document, path: &str, what: &str) -> Result<&'a mut Map, String> {
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
        Document::Object(map) => map.remove(name),
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

/// `at * by` for `mul`, `by` a number (`Update::check`).
fn multiply(at: &Document, by: &Document) -> Result<Document, String> {
    Ok(match (at, by) {
        (Document::Int(a), Document::Int(b)) => Document::Int(
            a.checked_mul(*b)
                .ok_or_else(|| format!("{a} × {b} overflows a 64-bit integer"))?,
        ),
        (Document::Int(a), Document::Float(b)) => Document::Float(*a as f64 * b),
        (Document::Float(a), Document::Int(b)) => Document::Float(a * *b as f64),
        (Document::Float(a), Document::Float(b)) => Document::Float(a * b),
        (other, _) => return Err(format!("`mul` needs a number there, not {}", kind(other))),
    })
}

/// What `mul` makes a missing field: zero, of `by`'s kind.
fn zero_like(by: &Document) -> Document {
    match by {
        Document::Float(_) => Document::Float(0.0),
        _ => Document::Int(0),
    }
}

fn is_number(value: &Document) -> bool {
    matches!(value, Document::Int(_) | Document::Float(_))
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
        Document::Date(_) => "a date",
        Document::Time(_) => "a time of day",
    }
}

/// Why a document of operators isn't an update (SPEC §178): its text,
/// as `trunkdb update` prints it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ParseUpdateError {
    message: String,
}

/// An update written as MongoDB writes one (SPEC §156.2, §178): an
/// object of operators, each an object of paths and what to do there,
/// `{"$inc": {"hits": 1}, "$set": {"seen": true}}`. `$set`,
/// `$setOnInsert`, `$unset` (an object or an array of paths), `$inc`,
/// `$mul`, `$min`, `$max`, `$rename`, `$push` and `$addToSet` (each with
/// `$each`), `$pull`, and `$pop` (1 for the last element, -1 for the
/// first). A field that isn't an operator is refused: a whole document
/// isn't replaced.
impl TryFrom<Document> for Update {
    type Error = ParseUpdateError;

    fn try_from(doc: Document) -> Result<Self, ParseUpdateError> {
        from_operators(doc).map_err(|message| ParseUpdateError { message })
    }
}

/// An update from its JSON text, as `TryFrom<Document>` reads the
/// document (SPEC §178).
impl std::str::FromStr for Update {
    type Err = ParseUpdateError;

    fn from_str(text: &str) -> Result<Self, ParseUpdateError> {
        let doc: Document = text.parse().map_err(|e| ParseUpdateError {
            message: format!("the update isn't JSON: {e}"),
        })?;
        Update::try_from(doc)
    }
}

/// The operators of `doc` as an update; `Err` says why not.
fn from_operators(doc: Document) -> Result<Update, String> {
    const OPERATORS: &str = "$set $setOnInsert $unset $inc $mul $min $max $rename $push \
                             $addToSet $pull $pop";
    let Document::Object(ops) = doc else {
        return Err(
            r#"the update is an object of operators, like {"$set": {"status": "done"}}"#
                .to_string(),
        );
    };
    if ops.is_empty() {
        return Err(format!("the update has no operators; one of {OPERATORS}"));
    }
    let paths = |op: &str, args: Document| -> Result<Vec<(String, Document)>, String> {
        match args {
            Document::Object(fields) if !fields.is_empty() => Ok(fields.into_iter().collect()),
            _ => Err(format!(
                r#"{op} takes an object of paths, like {{"{op}": {{"status": …}}}}"#
            )),
        }
    };
    let mut update = Update::new();
    for (op, args) in ops {
        match op.as_str() {
            "$set" | "$setOnInsert" | "$inc" | "$mul" | "$min" | "$max" | "$pull" => {
                for (path, value) in paths(&op, args)? {
                    update = match op.as_str() {
                        "$set" => update.set(path, value),
                        "$setOnInsert" => update.set_on_insert(path, value),
                        "$inc" => update.inc(path, value),
                        "$mul" => update.mul(path, value),
                        "$min" => update.min(path, value),
                        "$max" => update.max(path, value),
                        _ => update.pull(path, value),
                    };
                }
            }
            "$unset" => {
                let names: Vec<String> = match args {
                    Document::Array(names) => names
                        .into_iter()
                        .map(|name| match name {
                            Document::String(name) => Ok(name),
                            _ => Err("$unset takes paths as strings".to_string()),
                        })
                        .collect::<Result<_, _>>()?,
                    args => paths(&op, args)?
                        .into_iter()
                        .map(|(path, _)| path)
                        .collect(),
                };
                for path in names {
                    update = update.unset(path);
                }
            }
            "$rename" => {
                for (from, to) in paths(&op, args)? {
                    let Document::String(to) = to else {
                        return Err(format!(
                            "$rename takes the new path as a string, for {from}"
                        ));
                    };
                    update = update.rename(from, to);
                }
            }
            "$push" | "$addToSet" => {
                for (path, value) in paths(&op, args)? {
                    let each = match &value {
                        Document::Object(fields) if fields.len() == 1 => {
                            match fields.get("$each") {
                                Some(Document::Array(values)) => Some(values.clone()),
                                Some(_) => {
                                    return Err(format!("{op}: $each takes an array, for {path}"));
                                }
                                None => None,
                            }
                        }
                        _ => None,
                    };
                    update = match (op.as_str(), each) {
                        ("$push", Some(values)) => update.push_all(path, values),
                        ("$push", None) => update.push(path, value),
                        (_, Some(values)) => values.into_iter().fold(update, |update, value| {
                            update.add_to_set(path.clone(), value)
                        }),
                        (_, None) => update.add_to_set(path, value),
                    };
                }
            }
            "$pop" => {
                for (path, end) in paths(&op, args)? {
                    update = match end {
                        Document::Int(1) => update.pop_last(path),
                        Document::Int(-1) => update.pop_first(path),
                        _ => {
                            return Err(format!(
                                "$pop takes 1 for the last element or -1 for the first, for {path}"
                            ));
                        }
                    };
                }
            }
            other if other.starts_with('$') => {
                return Err(format!(
                    "{other}: not an update operator; one of {OPERATORS}"
                ));
            }
            field => {
                return Err(format!(
                    r#"`{field}` isn't an operator: say how to change it, {{"$set": {{"{field}": …}}}}; a whole document isn't replaced"#
                ));
            }
        }
    }
    Ok(update)
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
            Document::Object(map) => map.0.into_keys().collect::<Vec<_>>(),
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
            let mut map = Map::new();
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

    /// SPEC §142: `mul` as `inc` multiplies, a missing field zero of
    /// `by`'s kind, overflow and anything but a number failing.
    #[test]
    fn mul_multiplies_integers_and_floats_and_makes_a_missing_field_zero() {
        let mul = |by: Document, to: &str| applied(Update::new().mul("n", by), to);
        assert_eq!(mul(3.into(), r#"{"n": 14}"#).unwrap(), doc(r#"{"n": 42}"#));
        assert_eq!(
            mul((-2).into(), r#"{"n": 5}"#).unwrap(),
            doc(r#"{"n": -10}"#)
        );
        assert_eq!(
            mul(0.5.into(), r#"{"n": 3}"#).unwrap(),
            doc(r#"{"n": 1.5}"#)
        );
        assert_eq!(
            mul(2.into(), r#"{"n": 1.25}"#).unwrap(),
            doc(r#"{"n": 2.5}"#)
        );
        assert_eq!(
            mul(1.5.into(), r#"{"n": 0.5}"#).unwrap(),
            doc(r#"{"n": 0.75}"#)
        );
        // Zero of `by`'s kind: `Int` and `Float` are told apart by Debug.
        let missing = |by: Document| format!("{:?}", mul(by, r#"{"m": 1}"#).unwrap());
        assert_eq!(
            missing(3.into()),
            format!("{:?}", doc(r#"{"m": 1, "n": 0}"#))
        );
        assert_eq!(
            missing(3.5.into()),
            format!("{:?}", doc(r#"{"m": 1, "n": 0.0}"#))
        );
        let nested = applied(Update::new().mul("a.n", 4), "{}").unwrap();
        assert_eq!(nested, doc(r#"{"a": {"n": 0}}"#));

        let overflow = mul(2.into(), &format!(r#"{{"n": {}}}"#, i64::MAX / 2 + 1)).unwrap_err();
        assert!(overflow.contains("overflows"), "{overflow}");
        assert_eq!(
            mul(2.into(), &format!(r#"{{"n": {}}}"#, i64::MAX / 2)).unwrap(),
            doc(&format!(r#"{{"n": {}}}"#, i64::MAX / 2 * 2))
        );
        for not_a_number in [r#"{"n": null}"#, r#"{"n": "2"}"#, r#"{"n": [2]}"#] {
            let error = mul(2.into(), not_a_number).unwrap_err();
            assert!(
                error.contains("`mul` needs a number there"),
                "{not_a_number}: {error}"
            );
        }
    }

    /// SPEC §142: `push_all` pushes each value in order; `pop_first` and
    /// `pop_last` take one from an end.
    #[test]
    fn push_all_and_pop_change_arrays() {
        let with = |update: Update, to: &str| applied(update, to);
        assert_eq!(
            with(
                Update::new().push_all("t", ["b", "c", "b"]),
                r#"{"t": ["a"]}"#
            )
            .unwrap(),
            doc(r#"{"t": ["a", "b", "c", "b"]}"#)
        );
        assert_eq!(
            with(Update::new().push_all("o.t", [1, 2]), "{}").unwrap(),
            doc(r#"{"o": {"t": [1, 2]}}"#)
        );
        let none: [Document; 0] = [];
        assert_eq!(
            with(Update::new().push_all("t", none.clone()), "{}").unwrap(),
            doc(r#"{"t": []}"#)
        );
        assert_eq!(
            with(Update::new().push_all("t", none.clone()), r#"{"t": [1]}"#).unwrap(),
            doc(r#"{"t": [1]}"#)
        );

        let queue = r#"{"t": [1, 2, 3]}"#;
        assert_eq!(
            with(Update::new().pop_first("t"), queue).unwrap(),
            doc(r#"{"t": [2, 3]}"#)
        );
        assert_eq!(
            with(Update::new().pop_last("t"), queue).unwrap(),
            doc(r#"{"t": [1, 2]}"#)
        );
        assert_eq!(
            with(Update::new().pop_first("t").pop_last("t"), queue).unwrap(),
            doc(r#"{"t": [2]}"#)
        );
        assert_eq!(
            with(Update::new().pop_last("o.t"), r#"{"o": {"t": ["x"]}}"#).unwrap(),
            doc(r#"{"o": {"t": []}}"#)
        );
        for same in [r#"{"t": []}"#, r#"{"a": 1}"#, r#"{"x": 1}"#] {
            for update in [Update::new().pop_first("t"), Update::new().pop_last("t")] {
                assert_eq!(with(update, same).unwrap(), doc(same), "{same}");
            }
        }

        for (update, op) in [
            (Update::new().push_all("t", none), "`push`"),
            (Update::new().pop_first("t"), "`pop_first`"),
            (Update::new().pop_last("t"), "`pop_last`"),
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
            .set("a", Document::Object(Map::new()))
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
        assert!(refused(Update::new().mul("n", "2")).contains("`mul` needs a number"));
        assert!(refused(Update::new().mul("n", Document::Null)).contains("`mul` needs a number"));
        assert!(refused(Update::new().pop_first("t[*]")).contains("brackets"));
        assert!(refused(Update::new().pop_last("_id")).contains("`_id`"));
        assert!(refused(Update::new().push_all("", [1])).contains("a name between"));
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
                .push_all("t", [1, 2])
                .mul("n", 2)
                .pop_first("t")
                .pop_last("t")
                .pull("t", Document::Array(vec![]))
                .check()
                .is_ok()
        );
    }
    #[test]
    fn set_on_insert_sets_only_where_inserting() {
        let update = Update::new().set_on_insert("first", 1).inc("hits", 1);
        assert_eq!(
            applied(update.clone(), r#"{"hits": 4}"#),
            Ok(doc(r#"{"hits": 5}"#))
        );
        let mut new = doc(r#"{"page": "/"}"#);
        update.apply_inserting(&mut new).unwrap();
        assert_eq!(new, doc(r#"{"page": "/", "first": 1, "hits": 1}"#));
        // Checked as a `set` is, and through a string it fails as one.
        assert!(Update::new().set_on_insert("_id", 1).check().is_err());
        assert!(Update::new().set_on_insert("a[0]", 1).check().is_err());
        let mut text = doc(r#"{"a": "x"}"#);
        assert!(
            Update::new()
                .set_on_insert("a.b", 1)
                .apply_inserting(&mut text)
                .is_err()
        );
    }

    /// SPEC §144.2: the document an upsert inserts starts from the
    /// filter's `eq`s, at the top and in ANDs, and its `_id`.
    #[test]
    fn the_seed_takes_the_filters_eqs() {
        use crate::query::Filter;
        let id = DocId([3; 16]);
        let seeded = |filter: Filter| seed(&filter.conditions);
        assert_eq!(
            seeded(
                Filter::new()
                    .eq("page", "/home")
                    .eq("at.site", "a")
                    .id(id)
                    .gt("hits", 3)
                    .ne("x", 1)
                    .eq("tags[*]", "t")
                    .any_of([Condition::eq("o", 1), Condition::eq("o", 2)])
                    .and(Condition::Not(Box::new(Condition::eq("n", 1))))
            ),
            Ok((Some(id), doc(r#"{"page": "/home", "at": {"site": "a"}}"#)))
        );
        assert_eq!(
            seeded(Filter::new().and(Condition::all([
                Condition::eq("a", 1),
                Condition::all([Condition::eq("b", Document::Null)])
            ]))),
            Ok((None, doc(r#"{"a": 1, "b": null}"#)))
        );
        assert_eq!(
            seeded(Filter::new().eq("_id", "not an id")),
            Ok((None, doc("{}"))),
            "an `_id` that isn't an id names none"
        );
        assert_eq!(
            seeded(Filter::new().eq("a", 1).eq("a", 1.0)),
            Ok((None, doc(r#"{"a": 1}"#))),
            "two equal values are one"
        );
        for (filter, says) in [
            (Filter::new().eq("a", 1).eq("a", 2), "two values"),
            (Filter::new().eq("a", 1).eq("a.b", 2), "one inside it"),
            (Filter::new().eq("a.b", 2).eq("a", 1), "one inside it"),
            (Filter::new().id(id).id(DocId([4; 16])), "two ids"),
        ] {
            let error = seeded(filter).unwrap_err();
            assert!(error.contains(says), "{error}");
        }
        assert_eq!(
            seeded(Filter::new().eq("ab", 1).eq("a", 2)),
            Ok((None, doc(r#"{"ab": 1, "a": 2}"#))),
            "a name that starts another's isn't inside it"
        );
    }
}
