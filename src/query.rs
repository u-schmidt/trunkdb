//! Filters: conditions, sorts, `skip`, `limit` and `select`, built in
//! code or read from text (SPEC §111), and the plans `explain` reports.

pub use crate::aggregate::{Aggregate, UnnamedColumn};
use crate::catalog::IndexMeta;
use crate::document::{DocId, Document, ID_FIELD};
pub use crate::explain::{Alternative, EntryCount, ExecutionStats, Explained, IndexRead, SortWork};
use crate::index::{KeyRange, key};
use crate::map::Map;
use crate::path::Path;
pub use crate::query_text::ParseFilterError;
use crate::select::Selection;
pub use crate::update::Update;

/// `#[non_exhaustive]`: a `match` on it outside this crate needs a `_`
/// arm, so a new operator isn't a breaking change (SPEC §45.4).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Op {
    /// Equal: single values as `compare` has them (SPEC §32, §34.1) —
    /// `5` equals `5.0`, a string never a number, NaN nothing — and
    /// arrays, objects and binary by their contents (SPEC §124): arrays
    /// element by element, objects field by field in any order.
    Eq,
    /// Not `Eq`: also where the field is missing or null, or holds a
    /// value of another kind.
    Ne,
    /// `Lt` to `Gte` hold between values of one kind that have an order
    /// (SPEC §34.1): never for arrays, objects, binary or NaN.
    Lt,
    /// Less than or equal, as `Lt`.
    Lte,
    /// Greater than, as `Lt`.
    Gt,
    /// Greater than or equal, as `Lt`.
    Gte,
    /// The field is a string containing the condition's string value,
    /// ignoring case — like LiteDB, whose string comparisons ignore case
    /// by default. "Ignoring case" is Unicode lowercasing plus `ß` = `ss`
    /// (see `fold_case`). Anything that isn't a string on either side
    /// doesn't match. Substring only, not a pattern language — no
    /// wildcards, no regex.
    Contains,
}

/// What a document must satisfy (SPEC §36): a comparison of one field,
/// a test of its shape — there at all, an array of some size (§45) — or
/// several conditions combined, nested as deep as needed.
/// `#[non_exhaustive]`, like `Op`, and so is each variant with named
/// fields (SPEC §117): outside trunkdb they're built with `Condition`'s
/// methods, and matched with `..`, so a field can be added to one
/// without breaking a program —
/// `Condition::Compare { field, op, value, .. }`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Condition {
    /// `field op value`.
    #[non_exhaustive]
    Compare {
        /// The field, a path (SPEC §31).
        field: String,
        /// How it compares.
        op: Op,
        /// What it's compared to.
        value: Document,
    },
    /// The field is there — even if it holds null, unlike `is_not_null`
    /// (SPEC §45.1). On a path with `[*]`: some element has it.
    #[non_exhaustive]
    Exists {
        /// The field, a path (SPEC §31).
        field: String,
    },
    /// The field is an array whose length `op`-compares true against
    /// `size` (SPEC §45.2) — `Op::Eq` and 0 for an empty one. `Ne` is
    /// `Eq` negated, so it also holds where there's no array at all.
    #[non_exhaustive]
    Size {
        /// The field, a path (SPEC §31).
        field: String,
        /// How the length compares.
        op: Op,
        /// The length it's compared to.
        size: usize,
    },
    /// Some element of the array at `field` meets `condition` on its own
    /// (SPEC §46): `items` has an item with `sku == "A1"` *and* `qty >
    /// 2`, where `items[*].sku == "A1" AND items[*].qty > 2` may be met
    /// by two different items. Paths in `condition` start at the
    /// element; `""` is the element itself.
    #[non_exhaustive]
    ElemMatch {
        /// The array's field, a path.
        field: String,
        /// What one element must meet, with paths starting at the element.
        condition: Box<Condition>,
    },
    /// Every one of them holds (AND) — true if there are none.
    All(Vec<Condition>),
    /// At least one of them holds (OR) — false if there are none.
    Any(Vec<Condition>),
    /// It doesn't hold (NOT). Plain two-valued logic: `not(x == 5)` is
    /// true where `x` is missing, like `x != 5` (SPEC §32).
    Not(Box<Condition>),
}

/// Building conditions, for `Filter::and` and `Filter::any_of` (SPEC
/// §36.2). Values convert as in `Filter`'s builder. `|`, `&` and `!`
/// combine them: `Condition::eq("a", 1) | !Condition::eq("b", 2)`.
impl Condition {
    /// `field op value`.
    pub fn compare(field: impl Into<String>, op: Op, value: impl Into<Document>) -> Self {
        Condition::Compare {
            field: field.into(),
            op,
            value: value.into(),
        }
    }

    /// `field == value`. Against null, also true for a missing field
    /// (SPEC §32).
    pub fn eq(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Eq, value)
    }

    /// The document's id is `id`: `eq("_id", id)`, without spelling the
    /// field (SPEC §63). An OR of them fetches several documents by id,
    /// each a lookup in the primary index (SPEC §61).
    pub fn id(id: DocId) -> Self {
        Self::eq(ID_FIELD, id)
    }

    /// `field != value` — true for a null or missing field unless `value`
    /// is null (SPEC §32).
    pub fn ne(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Ne, value)
    }

    /// `field < value`, between values of one kind that have an order
    /// (SPEC §34.1): never for arrays, objects, binary or NaN.
    pub fn lt(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Lt, value)
    }

    /// `field <= value`, as `lt`.
    pub fn lte(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Lte, value)
    }

    /// `field > value`, as `lt`.
    pub fn gt(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Gt, value)
    }

    /// `field >= value`, as `lt`.
    pub fn gte(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Gte, value)
    }

    /// The field is a string containing `needle`, ignoring case (see
    /// `Op::Contains`).
    pub fn contains(field: impl Into<String>, needle: impl Into<String>) -> Self {
        Self::compare(field, Op::Contains, needle.into())
    }

    /// The field is null or missing (SPEC §32): `eq(field, Document::Null)`.
    pub fn is_null(field: impl Into<String>) -> Self {
        Self::compare(field, Op::Eq, Document::Null)
    }

    /// The field holds one of `values`, SQL's `IN` (SPEC §115): an OR of
    /// an `eq` per value, so an index on the field reads one point per
    /// value (§36.3), and on `_id` each is a lookup by id (§61). No
    /// values: nothing matches. `!Condition::is_in(..)` is `NOT IN`.
    ///
    /// ```
    /// use trunkdb::query::Condition;
    ///
    /// let waiting = Condition::is_in("status", ["open", "blocked"]);
    /// # let _ = waiting;
    /// ```
    pub fn is_in<V: Into<Document>>(
        field: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        let field = field.into();
        Condition::Any(
            values
                .into_iter()
                .map(|value| Self::eq(field.clone(), value))
                .collect(),
        )
    }

    /// The field is there and not null: `ne(field, Document::Null)`.
    pub fn is_not_null(field: impl Into<String>) -> Self {
        Self::compare(field, Op::Ne, Document::Null)
    }

    /// The field is there, null or not (SPEC §45.1).
    pub fn exists(field: impl Into<String>) -> Self {
        Condition::Exists {
            field: field.into(),
        }
    }

    /// The field isn't there: `!Condition::exists(field)`.
    pub fn missing(field: impl Into<String>) -> Self {
        !Self::exists(field)
    }

    /// The field is an array whose length `op`-compares true against
    /// `size` (SPEC §45.2).
    pub fn size(field: impl Into<String>, op: Op, size: usize) -> Self {
        Condition::Size {
            field: field.into(),
            op,
            size,
        }
    }

    /// Some element of the array at `field` meets `condition`, with
    /// paths relative to the element — `""` for the element itself
    /// (SPEC §46):
    ///
    /// ```
    /// use trunkdb::query::Condition;
    ///
    /// let big_a1 = Condition::elem_match(
    ///     "items",
    ///     Condition::eq("sku", "A1") & Condition::gt("qty", 2),
    /// );
    /// let in_80s = Condition::elem_match(
    ///     "scores",
    ///     Condition::gte("", 80) & Condition::lt("", 90),
    /// );
    /// # let _ = (big_a1, in_80s);
    /// ```
    pub fn elem_match(field: impl Into<String>, condition: Condition) -> Self {
        Condition::ElemMatch {
            field: field.into(),
            condition: Box::new(condition),
        }
    }

    /// AND.
    pub fn all(conditions: impl IntoIterator<Item = Condition>) -> Self {
        Condition::All(conditions.into_iter().collect())
    }

    /// OR.
    pub fn any(conditions: impl IntoIterator<Item = Condition>) -> Self {
        Condition::Any(conditions.into_iter().collect())
    }

    /// Whether `doc` meets the condition, checked in memory as `find`
    /// checks a document.
    pub fn matches(&self, doc: &Document) -> bool {
        Matcher::of(self).matches(doc)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Which way a sort key runs, as `sort_asc` and `sort_desc` choose it.
pub enum SortOrder {
    /// Smallest first.
    Asc,
    /// Largest first.
    Desc,
}

/// One key of a sort: a field, and which way. Internal since SPEC §60: a
/// filter gets its sort keys from `sort_asc`, `then_desc` and the like.
#[derive(Debug, Clone)]
pub(crate) struct Sort {
    pub(crate) field: String,
    pub(crate) order: SortOrder,
}

impl Sort {
    #[cfg(test)]
    pub(crate) fn asc(field: impl Into<String>) -> Self {
        Sort {
            field: field.into(),
            order: SortOrder::Asc,
        }
    }

    #[cfg(test)]
    pub(crate) fn desc(field: impl Into<String>) -> Self {
        Sort {
            field: field.into(),
            order: SortOrder::Desc,
        }
    }
}

/// A query: conditions that must all hold (each may nest ORs, ANDs and
/// NOTs, SPEC §36), plus an optional sort, skip and limit. Evaluated by
/// scanning, or over index ranges when the conditions allow it
/// (`index_ranges`, SPEC §28.4, §36.3), or by reading an index in sort
/// order (`index_order`, SPEC §34.2, §47; the primary index for `_id`,
/// §96).
///
/// Built only through its methods (SPEC §60), so how it holds its parts
/// isn't part of the API:
///
/// ```
/// use trunkdb::query::Filter;
///
/// let newest_queued = Filter::new().eq("status", "Queued").sort_desc("created").limit(20);
/// let third_page = Filter::new().sort_asc("name").skip(40).limit(20);
/// let all = Filter::new().limit(None); // `limit` takes an `Option` too
/// # let _ = (newest_queued, third_page, all);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub(crate) conditions: Vec<Condition>,
    /// Sort keys, most significant first (SPEC §47): by the first field,
    /// documents equal in it by the second, and so on; equal in all of
    /// them, by id. Empty: no sort.
    pub(crate) sort: Vec<Sort>,
    /// How many matches to pass over, after the sort and before the
    /// limit (SPEC §72). 0: none.
    pub(crate) skip: usize,
    pub(crate) limit: Option<usize>,
    /// The paths a read hands out, besides `_id` (SPEC §106); `None`:
    /// whole documents.
    pub(crate) select: Option<Vec<String>>,
    /// Which index to read, where the planner's rules would choose
    /// another (SPEC §132); `None`: the rules choose.
    pub(crate) hint: Option<Hint>,
}

/// A filter's hint (SPEC §132): `Filter::use_index`, `Filter::no_index`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Hint {
    /// The index on these fields, as `ensure_index` takes them; `["_id"]`
    /// for the primary index.
    Index(Vec<String>),
    /// No index: every document is read.
    NoIndex,
}

/// Building a filter one call at a time (SPEC §35) — every condition is
/// ANDed to the ones before:
///
/// ```
/// use trunkdb::query::Filter;
///
/// let newest_complete = Filter::new()
///     .eq("status", "Complete")
///     .gte("seen", 10)
///     .sort_desc("started_at")
///     .limit(1);
/// # let _ = newest_complete;
/// ```
///
/// Values are anything that converts into a `Document` — `bool`, the
/// integer and float types that fit, `&str`/`String`, `DocId`, an
/// `Option` of those (`None` is null) — or a `Document` itself. Fields
/// can be dotted paths (`"address.city"`, SPEC §31).
impl Filter {
    /// Matches every document: no conditions, no sort, no skip, no limit.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a condition: the field `op`-compares true against `value`.
    pub fn condition(self, field: impl Into<String>, op: Op, value: impl Into<Document>) -> Self {
        self.and(Condition::compare(field, op, value))
    }

    /// Adds any `Condition` — an OR, a NOT, a nested group (SPEC §36).
    pub fn and(mut self, condition: Condition) -> Self {
        self.conditions.push(condition);
        self
    }

    /// Adds an OR: at least one of `conditions` holds.
    pub fn any_of(self, conditions: impl IntoIterator<Item = Condition>) -> Self {
        self.and(Condition::any(conditions))
    }

    /// The document's id is `id` (SPEC §63): `eq("_id", id)`, a lookup
    /// in the primary index (SPEC §61). Filters see a field by its stored
    /// name, so on a struct's `#[serde(rename = "_id")] id` field,
    /// `eq("id", ...)` would find nothing; this can't be misspelled.
    /// Several ids: `is_in("_id", ids)` (SPEC §115).
    pub fn id(self, id: DocId) -> Self {
        self.and(Condition::id(id))
    }

    /// `field == value`. Against null, also true for a missing field
    /// (SPEC §32).
    pub fn eq(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Eq, value)
    }

    /// `field != value` — true for a null or missing field unless `value`
    /// is null (SPEC §32).
    pub fn ne(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Ne, value)
    }

    /// `field < value` — see `Condition::lt`.
    pub fn lt(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Lt, value)
    }

    /// `field <= value`, as `lt`.
    pub fn lte(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Lte, value)
    }

    /// `field > value`, as `lt`.
    pub fn gt(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Gt, value)
    }

    /// `field >= value`, as `lt`.
    pub fn gte(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Gte, value)
    }

    /// The field is a string containing `needle`, ignoring case (see
    /// `Op::Contains`).
    pub fn contains(self, field: impl Into<String>, needle: impl Into<String>) -> Self {
        self.condition(field, Op::Contains, needle.into())
    }

    /// The field is null or missing (SPEC §32).
    pub fn is_null(self, field: impl Into<String>) -> Self {
        self.condition(field, Op::Eq, Document::Null)
    }

    /// The field holds one of `values` (SPEC §115): `Condition::is_in`.
    pub fn is_in<V: Into<Document>>(
        self,
        field: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Self {
        self.and(Condition::is_in(field, values))
    }

    /// The field is there and not null.
    pub fn is_not_null(self, field: impl Into<String>) -> Self {
        self.condition(field, Op::Ne, Document::Null)
    }

    /// The field is there, null or not — where `is_null` can't tell a
    /// stored null from a missing field (SPEC §45.1).
    pub fn exists(self, field: impl Into<String>) -> Self {
        self.and(Condition::exists(field))
    }

    /// The field isn't there at all; a stored null doesn't count.
    pub fn missing(self, field: impl Into<String>) -> Self {
        self.and(Condition::missing(field))
    }

    /// The field is an array whose length `op`-compares true against
    /// `size`: `.size("tags", Op::Eq, 0)` for no tags, `.size("tags",
    /// Op::Gte, 3)` for three or more (SPEC §45.2). Not an array — or
    /// missing — matches only `Ne`.
    pub fn size(self, field: impl Into<String>, op: Op, size: usize) -> Self {
        self.and(Condition::size(field, op, size))
    }

    /// Some element of the array at `field` meets `condition` on its own
    /// — see `Condition::elem_match` (SPEC §46).
    pub fn elem_match(self, field: impl Into<String>, condition: Condition) -> Self {
        self.and(Condition::elem_match(field, condition))
    }

    /// Sorts by `field`, smallest first — replacing any earlier sort.
    pub fn sort_asc(self, field: impl Into<String>) -> Self {
        self.sort_by(field, SortOrder::Asc)
    }

    /// Sorts by `field`, largest first — replacing any earlier sort.
    pub fn sort_desc(self, field: impl Into<String>) -> Self {
        self.sort_by(field, SortOrder::Desc)
    }

    /// Sorts by `field` — replacing any earlier sort.
    pub fn sort_by(mut self, field: impl Into<String>, order: SortOrder) -> Self {
        self.sort = Vec::new();
        self.then_by(field, order)
    }

    /// Then by `field`, smallest first, for documents the sort so far
    /// finds equal (SPEC §47): `.sort_asc("status").then_desc("created")`
    /// — by status, and within a status newest first.
    pub fn then_asc(self, field: impl Into<String>) -> Self {
        self.then_by(field, SortOrder::Asc)
    }

    /// Then by `field`, largest first.
    pub fn then_desc(self, field: impl Into<String>) -> Self {
        self.then_by(field, SortOrder::Desc)
    }

    /// Adds a sort key after the ones before — the first one if there
    /// are none.
    pub fn then_by(mut self, field: impl Into<String>, order: SortOrder) -> Self {
        self.sort.push(Sort {
            field: field.into(),
            order,
        });
        self
    }

    /// At most `n` documents: `limit(20)`, or `limit(None)` for no limit,
    /// so an `Option<usize>` can be passed through as it is. Replaces any
    /// earlier limit.
    pub fn limit(mut self, n: impl Into<Option<usize>>) -> Self {
        self.limit = n.into();
        self
    }

    /// Passes over the first `n` matches, in `sort` order, before `limit`
    /// counts (SPEC §72): page `p` (from 0) of `size` is
    /// `.skip(p * size).limit(size)`. Replaces any earlier skip; `skip(0)`
    /// is none. Without a sort the order is unspecified, so the pages of
    /// an unsorted filter may overlap.
    ///
    /// The skipped documents are still read: a deep page costs as much
    /// as all the pages before it, as SQL's `OFFSET` does. To page
    /// through many, start each page after the last value of the one
    /// before, `gt(field, last).sort_asc(field).limit(size)`, which an
    /// index on `field` reads from there.
    pub fn skip(mut self, n: usize) -> Self {
        self.skip = n;
        self
    }

    /// Hands out only these fields of each match, and its `_id` (SPEC
    /// §106), as a SQL column list does:
    ///
    /// ```
    /// use trunkdb::query::Filter;
    ///
    /// let names = Filter::new().eq("team", "core").select(["name", "address.city"]);
    /// // [{"_id": ..., "name": "Ada", "address": {"city": "London"}}, ...]
    /// # let _ = names;
    /// ```
    ///
    /// - `_id` always comes first, selected or not.
    /// - The paths follow in the order first selected, nested as stored:
    ///   `address.city` and `address.zip` give one `address` with both.
    /// - A missing field is left out; a stored `null` is handed out.
    /// - A path doesn't walk into arrays, as conditions don't (SPEC §31):
    ///   if `address` isn't an object, `address.city` selects nothing. A
    ///   step ending in `[*]` selects its array whole: `items[*].sku`
    ///   gives `items`.
    /// - A path inside one selected whole adds nothing; the empty path
    ///   selects the whole document.
    /// - `select([] as [&str; 0])` hands out `_id` alone.
    ///
    /// A second call adds to the first. Conditions and sort keys see the
    /// whole document: a field they name needn't be selected. A typed
    /// read deserializes `T` from the selection, so a field of `T`
    /// outside it is missing to `T`.
    ///
    /// Only what reads hand out changes: `find`, `find_one`, `cursor`,
    /// and `explain_run`'s counts. `count` and the writes ignore it —
    /// `update_many` too, which writes back what its closure is given and
    /// so is given the whole document.
    ///
    /// It saves decoding: the fields not selected are skipped, not built.
    /// Where an index's entries decide the filter (SPEC §100) and hold
    /// every selected field, a match is handed out from its entry without
    /// reading its document — for booleans, strings that fit the key, ids
    /// and date-times; a number or a null is read from the document,
    /// whose type and presence the key doesn't keep (§106.5).
    pub fn select<S: Into<String>>(mut self, paths: impl IntoIterator<Item = S>) -> Self {
        let select = self.select.get_or_insert_with(Vec::new);
        select.extend(paths.into_iter().map(Into::into));
        self
    }

    /// The selection, parsed (SPEC §106.4); `None` without one, or where
    /// it selects the whole document.
    pub(crate) fn selection(&self) -> Option<Selection> {
        let selection = Selection::new(self.select.as_ref()?.iter().map(String::as_str));
        (!selection.is_whole()).then_some(selection)
    }

    /// Reads the index on `fields`, where the planner's rules would read
    /// another or none (SPEC §132): SQL Server's `WITH (INDEX(...))`.
    /// `fields` as `ensure_index` takes them, `"status"` or `["status",
    /// "created"]`; `"_id"` for the primary index, which lookups by id
    /// and a sort by `_id` read.
    ///
    /// ```
    /// use trunkdb::query::Filter;
    ///
    /// // 99% of the documents are "done": the few since yesterday are
    /// // fewer, but the rules take the `Eq` (SPEC §28.4).
    /// # let yesterday = 0;
    /// let recent = Filter::new()
    ///     .eq("status", "done")
    ///     .gt("created", yesterday)
    ///     .use_index("created");
    /// # let _ = recent;
    /// ```
    ///
    /// The plan is still the rules' among that one index: the same
    /// ranges, the same walk in sort order (§34.2), only no other index.
    /// What the rules would do with no index at all, they never do:
    /// where that index can't be read for the filter — no index on
    /// `fields`, no condition it can bound, no sort with a limit it can
    /// give the order of — every read with this filter fails with
    /// `Error::Hint`, and reads nothing. A hint is never quietly
    /// dropped.
    ///
    /// The results are the same with any hint, or none: a hint chooses
    /// how they are found, not which. Replaces an earlier hint.
    pub fn use_index(mut self, fields: impl crate::IndexFields) -> Self {
        self.hint = Some(Hint::Index(fields.into_fields()));
        self
    }

    /// Reads every document and reads no index for the filter (SPEC
    /// §132): not a secondary index, and not the primary index by id —
    /// a lookup by `_id` is scanned too. Where a range or a walk in sort
    /// order would read most of the collection, a scan costs less. Never
    /// fails. Replaces an earlier hint.
    pub fn no_index(mut self) -> Self {
        self.hint = Some(Hint::NoIndex);
        self
    }

    /// The indexes the planner may choose among, as the hint leaves them
    /// (SPEC §132.2): every one without a hint, the hinted one alone, or
    /// none. A slice of `indexes`, so narrowing twice is narrowing once.
    pub(crate) fn offered<'a>(&self, indexes: &'a [IndexMeta]) -> &'a [IndexMeta] {
        match &self.hint {
            None => indexes,
            Some(Hint::NoIndex) => &[],
            Some(Hint::Index(fields)) => match indexes.iter().position(|i| i.fields == *fields) {
                Some(at) => &indexes[at..=at],
                None => &[],
            },
        }
    }

    /// Whether the planner may read the primary index for the filter,
    /// to look up ids or walk in `_id` order: without a hint, or hinted
    /// to it (SPEC §132.2). A scan reads it whatever the hint.
    fn primary_offered(&self) -> bool {
        match &self.hint {
            None => true,
            Some(Hint::NoIndex) => false,
            Some(Hint::Index(fields)) => fields.len() == 1 && fields[0] == ID_FIELD,
        }
    }

    /// The index the hint names, as `Collection::indexes` names it —
    /// `None` without one, or for `no_index`.
    pub(crate) fn hinted_index(&self) -> Option<String> {
        match &self.hint {
            Some(Hint::Index(fields)) => Some(match &fields[..] {
                [single] => single.clone(),
                fields => format!("({})", fields.join(", ")),
            }),
            _ => None,
        }
    }
}

impl Filter {
    /// Whether `doc` meets every condition of the filter, checked in
    /// memory as `find` checks a document; sort, skip, limit and select play
    /// no part.
    pub fn matches(&self, doc: &Document) -> bool {
        self.matcher().matches(doc)
    }

    /// The conditions with their paths parsed, to check document after
    /// document (SPEC §102.2).
    pub(crate) fn matcher(&self) -> Matcher<'_> {
        Matcher::All(self.conditions.iter().map(Matcher::of).collect())
    }

    /// The top-level fields whose values decide whether a document
    /// matches and where it sorts (SPEC §99.2): the first step of every
    /// path in the conditions — through `All`, `Any` and `Not`; of an
    /// `elem_match` its array's path, as the paths inside start at the
    /// element — and in the sort keys. A document cut down to these
    /// fields matches and sorts as the whole one does. `None` if a path
    /// starts at the document itself (empty, or `[*]` first), which needs
    /// all of it.
    pub(crate) fn fields(&self) -> Option<Vec<&str>> {
        self.fields_with(true)
    }

    /// `fields`, the sort keys' only with `sort`: a walk that gets its
    /// whole order from an index doesn't look at them (SPEC §99.2).
    pub(crate) fn fields_with(&self, sort: bool) -> Option<Vec<&str>> {
        fn add<'a>(path: &'a str, names: &mut Vec<&'a str>) -> Option<()> {
            let first = path.split(['.', '[']).next().unwrap_or_default();
            if first.is_empty() {
                return None;
            }
            if !names.contains(&first) {
                names.push(first);
            }
            Some(())
        }
        fn of<'a>(condition: &'a Condition, names: &mut Vec<&'a str>) -> Option<()> {
            match condition {
                Condition::Compare { field, .. }
                | Condition::Exists { field }
                | Condition::Size { field, .. }
                | Condition::ElemMatch { field, .. } => add(field, names),
                Condition::All(conditions) | Condition::Any(conditions) => {
                    conditions.iter().try_for_each(|c| of(c, names))
                }
                Condition::Not(condition) => of(condition, names),
            }
        }
        let mut names = Vec::new();
        for condition in &self.conditions {
            of(condition, &mut names)?;
        }
        if sort {
            for key in &self.sort {
                add(&key.field, &mut names)?;
            }
        }
        Some(names)
    }

    /// Whether the entries of an index on `fields` — of the primary index
    /// for none — decide this filter, and with `sort` its order too (SPEC
    /// §100.1): every path its conditions name, and with `sort` every
    /// sort key, is one of `fields` or `_id`, which every key ends with;
    /// the conditions are comparisons, every `Op`, through `All`, `Any`
    /// and `Not`. Not `Exists`, which tells a missing field from a null
    /// one where a key doesn't (§32), nor `Size` or `ElemMatch`, which
    /// look into arrays. Then `Some`: which of `fields` it names, whose
    /// values must be exact for an entry to decide. A filter without
    /// conditions (or sort) names none.
    pub(crate) fn covered_by(&self, fields: &[String], sort: bool) -> Option<Vec<bool>> {
        let mut named = vec![false; fields.len()];
        let mut name = |path: &str| {
            if path == ID_FIELD {
                return Some(());
            }
            let at = fields.iter().position(|field| field == path)?;
            named[at] = true;
            Some(())
        };
        fn of(condition: &Condition, name: &mut impl FnMut(&str) -> Option<()>) -> Option<()> {
            match condition {
                Condition::Compare { field, .. } => name(field),
                Condition::All(conditions) | Condition::Any(conditions) => {
                    conditions.iter().try_for_each(|c| of(c, name))
                }
                Condition::Not(condition) => of(condition, name),
                Condition::Exists { .. } | Condition::Size { .. } | Condition::ElemMatch { .. } => {
                    None
                }
            }
        }
        for condition in &self.conditions {
            of(condition, &mut name)?;
        }
        if sort {
            for key in &self.sort {
                name(&key.field)?;
            }
        }
        Some(named)
    }

    /// Applies conditions, then sort, then skip, then limit — in that
    /// order, matching SQL's `WHERE` -> `ORDER BY` -> `OFFSET` -> `LIMIT`
    /// (sorting an already-filtered set is both correct and cheaper than
    /// sorting everything first). Then cuts each result down to the
    /// selection (`select`, SPEC §106).
    pub fn apply(&self, docs: impl IntoIterator<Item = Document>) -> Vec<Document> {
        let found = self.apply_to(docs, |doc| doc);
        match self.selection() {
            Some(selection) => found
                .into_iter()
                .map(|doc| selection.project(doc))
                .collect(),
            None => found,
        }
    }

    /// `apply` for items that carry a document rather than being one —
    /// e.g. `(DocId, Document)` pairs, so `delete_many` keeps each id
    /// with its document through filtering, sorting and limiting. The
    /// items come back as they were: a selection isn't applied.
    pub(crate) fn apply_to<T>(
        &self,
        items: impl IntoIterator<Item = T>,
        doc_of: impl Fn(&T) -> &Document,
    ) -> Vec<T> {
        let matcher = self.matcher();
        let results: Vec<T> = items
            .into_iter()
            .filter(|item| matcher.matches(doc_of(item)))
            .collect();
        self.arrange(results, doc_of)
    }

    /// `apply_to` for matches already checked: sort, then skip, then
    /// limit.
    pub(crate) fn arrange<T>(
        &self,
        mut results: Vec<T>,
        doc_of: impl Fn(&T) -> &Document,
    ) -> Vec<T> {
        if self.sort.is_empty() {
            results.drain(..self.skip.min(results.len()));
            if let Some(limit) = self.limit {
                results.truncate(limit);
            }
            return results;
        }
        // Each item's sort values looked up once, not on every comparison
        // (SPEC §102.5): `k` per item, side by side.
        let paths: Vec<Path> = self.sort.iter().map(|key| Path::new(&key.field)).collect();
        let k = paths.len();
        let mut order: Vec<usize> = (0..results.len()).collect();
        {
            let values: Vec<&Document> = (results.iter())
                .flat_map(|item| paths.iter().map(|path| path.value_or_null(doc_of(item))))
                .collect();
            let of = |i: usize| &values[i * k..(i + 1) * k];
            // Stable: equal in every key, they keep the order they came in.
            order.sort_by(|&a, &b| compare_values(&self.sort, of(a), of(b)));
        }
        let end = match self.limit {
            Some(limit) => self.skip.saturating_add(limit),
            None => order.len(),
        };
        let wanted = &order[self.skip.min(order.len())..end.min(order.len())];
        let mut items: Vec<Option<T>> = results.into_iter().map(Some).collect();
        wanted
            .iter()
            .map(|&i| items[i].take().expect("each item once"))
            .collect()
    }
}

/// `a | b`: OR. Extends `a` if it's an OR already, so `a | b | c` is one
/// OR of three.
impl std::ops::BitOr for Condition {
    type Output = Condition;

    fn bitor(self, other: Condition) -> Condition {
        match self {
            Condition::Any(mut branches) => {
                branches.push(other);
                Condition::Any(branches)
            }
            first => Condition::Any(vec![first, other]),
        }
    }
}

/// `a & b`: AND, extending `a` if it's an AND already.
impl std::ops::BitAnd for Condition {
    type Output = Condition;

    fn bitand(self, other: Condition) -> Condition {
        match self {
            Condition::All(mut conditions) => {
                conditions.push(other);
                Condition::All(conditions)
            }
            first => Condition::All(vec![first, other]),
        }
    }
}

/// `!a`: NOT.
impl std::ops::Not for Condition {
    type Output = Condition;

    fn not(self) -> Condition {
        Condition::Not(Box::new(self))
    }
}

/// How `Collection::find` runs a filter — see `Collection::explain`.
/// The variants with named fields are `#[non_exhaustive]` (SPEC §117):
/// matched with `..`, `QueryPlan::Index { field, .. }`, so a field can be
/// added to one without breaking a program.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum QueryPlan {
    /// Every document of the collection is read and checked.
    Scan,
    /// Only the documents in a range of the index on `field` are read,
    /// then checked against the whole filter.
    #[non_exhaustive]
    Index {
        /// The index, as `Collection::indexes` names it: `(status,
        /// created)` for a compound one.
        field: String,
    },
    /// The index on the sort field `field` is read in sort order, and
    /// documents are checked one by one until `limit` of them match; the
    /// rest are never read (SPEC §34.2). For `_id`, the primary index
    /// (SPEC §96).
    #[non_exhaustive]
    IndexOrder {
        /// The index read, named as for `Index`: `_id` for the primary
        /// index.
        field: String,
    },
    /// Several index ranges are read — one per branch of an OR, on the
    /// indexes on `fields` — each document once, then checked against
    /// the whole filter (SPEC §36.3).
    #[non_exhaustive]
    IndexUnion {
        /// The indexes read, one per branch, named as for `Index`.
        fields: Vec<String>,
    },
    /// The filter names its documents by id — `eq("_id", id)`, or an OR
    /// of such — and they are looked up in the primary index, as `get`
    /// does, then checked against the whole filter (SPEC §61).
    ById,
}

/// The ids `conditions`, all holding, name their documents by: the
/// fewest any of them names (`ids_for`). `None` if none names any.
fn ids_for_all(conditions: &[Condition]) -> Option<Vec<DocId>> {
    conditions
        .iter()
        .filter_map(ids_for)
        .min_by_key(|ids| ids.len())
}

/// The ids a document must have one of to meet `condition` (SPEC §61):
/// an `Eq` on `_id` with an id names one, an `All` what its conditions
/// name, an `Any` its branches' ids together if every branch names some.
/// Anything else names none.
fn ids_for(condition: &Condition) -> Option<Vec<DocId>> {
    match condition {
        Condition::Compare {
            field,
            op: Op::Eq,
            value: Document::Id(id),
        } if field == ID_FIELD => Some(vec![*id]),
        Condition::All(conditions) => ids_for_all(conditions),
        Condition::Any(branches) => branches
            .iter()
            .map(ids_for)
            .collect::<Option<Vec<_>>>()
            .map(|ids| ids.concat()),
        _ => None,
    }
}

/// Index ranges whose union holds every document a condition can match
/// (SPEC §36.3), and how good a choice that is: `ByValue` (one `Eq`) beats
/// `Union` (an OR) beats `ByRange`; among equal kinds, bounds on more
/// fields beat fewer — a compound index narrowed by two conditions
/// beats one index narrowed by one (SPEC §43.3).
struct Bounds<'a> {
    ranges: Vec<IndexRange<'a>>,
    kind: BoundKind,
    fields: usize,
}

impl Bounds<'_> {
    fn beats(&self, other: &Bounds) -> bool {
        (self.kind, std::cmp::Reverse(self.fields)) < (other.kind, std::cmp::Reverse(other.fields))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BoundKind {
    ByValue,
    Union,
    ByRange,
}

/// The best bounds for conditions that must all hold — `None` if no index
/// can bound any of them. The comparisons on one indexed field are
/// intersected, so `a >= 10 AND a <= 20` reads just that stretch (on a
/// path with `[*]` only one of them counts, SPEC §42.3); a
/// nested group is bounded on its own; a compound index by `Eq` on its
/// first fields and a range on the next (`compound_bounds`); the best of
/// all that wins, the first among equals. `Ne` and `Contains` never use
/// an index, nor does a `Not`. A sparse index (SPEC §44) has no entry for
/// a null: it bounds only by comparisons that null fails
/// (`rules_out_null`), and a compound one only if such a comparison is on
/// one of its fields.
fn bounds_for_all<'a>(conditions: &[Condition], indexes: &'a [IndexMeta]) -> Option<Bounds<'a>> {
    let mut best: Option<Bounds<'a>> = None;
    let mut consider = |candidate: Bounds<'a>| {
        if best.as_ref().is_none_or(|b| candidate.beats(b)) {
            best = Some(candidate);
        }
    };
    for (i, condition) in conditions.iter().enumerate() {
        match condition {
            Condition::Compare { field, .. } => {
                let Some(index) = indexes
                    .iter()
                    .find(|index| index.single() == Some(field.as_str()))
                else {
                    continue;
                };
                // Each field once, at its first comparison.
                let seen = conditions[..i]
                    .iter()
                    .any(|c| matches!(c, Condition::Compare { field: f, .. } if f == field));
                if seen {
                    continue;
                }
                // Each comparison that bounds the index: whether it's an
                // `Eq`, its range, and itself, for `explain_run` (§94.2).
                let mut on_field: Vec<(bool, KeyRange, &Condition)> = conditions
                    .iter()
                    .filter_map(|c| match c {
                        Condition::Compare {
                            field: f,
                            op,
                            value,
                        } if f == field && (!index.sparse || rules_out_null(op, value)) => {
                            Some((matches!(op, Op::Eq), key::range_for(op, value)?, c))
                        }
                        _ => None,
                    })
                    .collect();
                let by_value = on_field.iter().any(|(eq, ..)| *eq);
                if is_multi(field) && !on_field.is_empty() {
                    // Different elements may meet different comparisons —
                    // `tags[*] > 5 AND tags[*] < 3` holds for `[1, 10]` —
                    // so their ranges can't be intersected (SPEC §42.3).
                    // One of them bounds the documents: an `Eq`'s if any.
                    let one = on_field.iter().position(|(eq, ..)| *eq).unwrap_or(0);
                    on_field = vec![on_field.swap_remove(one)];
                }
                let by: Vec<Condition> = on_field.iter().map(|(.., c)| (*c).clone()).collect();
                let range = on_field
                    .into_iter()
                    .map(|(_, range, _)| range)
                    .reduce(KeyRange::intersect);
                if let Some(range) = range {
                    let kind = match by_value {
                        true => BoundKind::ByValue,
                        false => BoundKind::ByRange,
                    };
                    consider(Bounds {
                        ranges: vec![IndexRange { index, range, by }],
                        kind,
                        fields: 1,
                    });
                }
            }
            nested => {
                if let Some(bounds) = bounds_for(nested, indexes) {
                    consider(bounds);
                }
            }
        }
    }
    for index in indexes.iter().filter(|index| index.is_compound()) {
        if index.sparse && !excludes_null(index, conditions) {
            continue;
        }
        if let Some(bounds) = compound_bounds(index, conditions) {
            consider(bounds);
        }
    }
    best
}

/// Bounds for one condition: an `All` like the filter's own list, an
/// `Any` only if every branch can be bounded (the union of theirs), a
/// `Not` never — nor `Exists` or `Size`, which no index can tell
/// (SPEC §45.3). An `ElemMatch` like its condition moved out to the
/// array's elements (`within`, SPEC §46.3).
fn bounds_for<'a>(condition: &Condition, indexes: &'a [IndexMeta]) -> Option<Bounds<'a>> {
    match condition {
        Condition::Compare { .. } => bounds_for_all(std::slice::from_ref(condition), indexes),
        Condition::All(conditions) => bounds_for_all(conditions, indexes),
        Condition::Any(branches) => {
            let mut ranges = Vec::new();
            for branch in branches {
                ranges.extend(bounds_for(branch, indexes)?.ranges);
            }
            Some(Bounds {
                ranges,
                kind: BoundKind::Union,
                fields: 1,
            })
        }
        Condition::ElemMatch { field, condition } => bounds_for(&within(field, condition), indexes),
        Condition::Not(_) | Condition::Exists { .. } | Condition::Size { .. } => None,
    }
}

/// `condition` about an element of the array at `field`, rewritten as a
/// condition about the whole document: every path gets `field[*]` in
/// front — `sku` becomes `items[*].sku`, `""` becomes `items[*]` (SPEC
/// §46.3). Where an element meets `condition`, the document meets what
/// comes out for every comparison that can bound an index: an `Eq` or a
/// range holds for that element, so for some element (§42.1). What it
/// doesn't imply — a `Ne` (no element equal), anything under a `Not` —
/// never bounds one, so only bounds are taken from it, never matches.
fn within(field: &str, condition: &Condition) -> Condition {
    let path = |inner: &str| match inner.is_empty() || inner.starts_with("[*]") {
        true => format!("{field}[*]{inner}"),
        false => format!("{field}[*].{inner}"),
    };
    let all = |conditions: &[Condition]| conditions.iter().map(|c| within(field, c)).collect();
    match condition {
        Condition::Compare {
            field: inner,
            op,
            value,
        } => Condition::Compare {
            field: path(inner),
            op: op.clone(),
            value: value.clone(),
        },
        Condition::Exists { field: inner } => Condition::Exists { field: path(inner) },
        Condition::Size {
            field: inner,
            op,
            size,
        } => Condition::Size {
            field: path(inner),
            op: op.clone(),
            size: *size,
        },
        Condition::ElemMatch {
            field: inner,
            condition,
        } => Condition::ElemMatch {
            field: path(inner),
            condition: condition.clone(),
        },
        Condition::All(conditions) => Condition::All(all(conditions)),
        Condition::Any(branches) => Condition::Any(all(branches)),
        Condition::Not(condition) => Condition::Not(Box::new(within(field, condition))),
    }
}

/// Whether `field op value` fails for a null or missing field — so a
/// document it holds for has an entry in a sparse index (SPEC §44). On a
/// path with `[*]`, for an element: the one that meets it isn't null.
fn rules_out_null(op: &Op, value: &Document) -> bool {
    !value_matches(op, &Document::Null, value)
}

/// Whether `conditions`, all holding, keep out every document a sparse
/// `index` has no entry for: one of them compares one of its fields in a
/// way null fails. On one field without `[*]`, that makes its value not
/// null; on several, not null in all of them.
fn excludes_null(index: &IndexMeta, conditions: &[Condition]) -> bool {
    conditions.iter().any(|c| match c {
        Condition::Compare { field, op, value } => {
            index.fields.contains(field) && rules_out_null(op, value)
        }
        _ => false,
    })
}

/// How a compound index bounds conditions that must all hold (SPEC
/// §43.3). Its keys sort by the first field, then the second, ...: an
/// `Eq` on each of its first fields narrows to the keys that begin with
/// those values, and range comparisons on the field after them narrow
/// further. Nothing on its first field, no bounds.
fn compound_bounds<'a>(index: &'a IndexMeta, conditions: &[Condition]) -> Option<Bounds<'a>> {
    let (prefix, equal, mut by) = equal_prefix(index, conditions, index.fields.len());
    let range = index
        .fields
        .get(equal)
        .and_then(|next| field_range(index, conditions, next));
    let fields = equal + range.is_some() as usize;
    let range = match range {
        Some((range, on_next)) => {
            by.extend(on_next);
            range.under(&prefix)
        }
        None if equal > 0 => KeyRange::prefixed(&prefix),
        None => return None,
    };
    let kind = match equal > 0 {
        true => BoundKind::ByValue,
        false => BoundKind::ByRange,
    };
    Some(Bounds {
        ranges: vec![IndexRange { index, range, by }],
        kind,
        fields,
    })
}

/// How many of `index`'s first fields, up to `max`, an `Eq` among
/// `conditions` fixes — and those values encoded one after another, as
/// its keys begin, and the `Eq`s that fix them.
fn equal_prefix(
    index: &IndexMeta,
    conditions: &[Condition],
    max: usize,
) -> (Vec<u8>, usize, Vec<Condition>) {
    let mut prefix = Vec::new();
    let mut by = Vec::new();
    for (i, field) in index.fields.iter().take(max).enumerate() {
        let equal = conditions.iter().find_map(|c| match c {
            Condition::Compare {
                field: f,
                op: Op::Eq,
                value,
            } if f == field => Some((key::range_for_in(&Op::Eq, value, index.fields.len())?, c)),
            _ => None,
        });
        match equal {
            Some((range, condition)) => {
                prefix.extend(range.start);
                by.push(condition.clone());
            }
            None => return (prefix, i, by),
        }
    }
    (prefix, max, by)
}

/// The comparisons on `field` as one range of `index`'s values for it —
/// relative to where that value starts in a key — and the comparisons.
fn field_range(
    index: &IndexMeta,
    conditions: &[Condition],
    field: &str,
) -> Option<(KeyRange, Vec<Condition>)> {
    let (ranges, by): (Vec<KeyRange>, Vec<Condition>) = conditions
        .iter()
        .filter_map(|c| match c {
            Condition::Compare {
                field: f,
                op,
                value,
            } if f == field => Some((key::range_for_in(op, value, index.fields.len())?, c.clone())),
            _ => None,
        })
        .unzip();
    Some((ranges.into_iter().reduce(KeyRange::intersect)?, by))
}

impl Filter {
    /// The ids of the only documents that can match, in id order, each
    /// once — `None` unless the conditions name documents by id
    /// (`ids_for`). Then `find` looks them up in the primary index and
    /// needs no other index (SPEC §61); each is still checked against
    /// the whole filter, so an id that doesn't exist, or whose document
    /// fails another condition, is no match.
    pub(crate) fn ids(&self) -> Option<Vec<DocId>> {
        if !self.primary_offered() {
            return None; // hinted elsewhere (SPEC §132.2)
        }
        let mut ids = ids_for_all(&self.conditions)?;
        ids.sort_unstable();
        ids.dedup();
        Some(ids)
    }

    /// The index ranges `find` reads, among `indexes` — `None` for a full
    /// scan. Rule-based, not cost-based (SPEC §28.4, §36.3): an `Eq` on an
    /// indexed field, else an OR whose every branch an index can bound,
    /// else a range on an indexed field. The ranges may hold documents
    /// the filter rejects, never the other way around; the same document
    /// may be in several.
    pub(crate) fn index_ranges<'a>(&self, indexes: &'a [IndexMeta]) -> Option<Vec<IndexRange<'a>>> {
        let indexes = self.offered(indexes);
        if indexes.is_empty() {
            // Not even an OR of no branches, which no index bounds as
            // well as any: with none to read, a scan (SPEC §132.2).
            return None;
        }
        bounds_for_all(&self.conditions, indexes).map(|b| b.ranges)
    }

    /// The index `find` reads in sort order (SPEC §34.2) — for a filter
    /// with a `sort` and a `limit`, that doesn't name its documents by id
    /// (`ids`) — and how: see `OrderedRead`. An index
    /// on the first sort field, unless the conditions
    /// find documents by value some other way: an `Eq` on another indexed
    /// field, or an OR of indexed branches (a few documents found by value
    /// beat walking in order). Or a compound index with the sort field
    /// after fields every one of which an `Eq` fixes: `status ==
    /// "Queued"`, sorted by `created`, on `(status, created)` — found by
    /// value and in order at once (SPEC §43.3). The more fields fixed, the
    /// better; the first among equals. The sort field's own range
    /// comparisons narrow the range. A sparse index only where the
    /// conditions keep out the documents it lacks (SPEC §44).
    ///
    /// With several sort keys (SPEC §47), the index gives the order of
    /// as many as its next fields are, in the same direction as the
    /// first; among indexes fixing as many fields, the one that serves
    /// more keys wins. The rest are sorted in memory, among documents
    /// equal in the served ones.
    ///
    /// A sort by `_id` walks the primary index (SPEC §96): see
    /// `id_order`.
    pub(crate) fn index_order<'a>(&self, indexes: &'a [IndexMeta]) -> Option<OrderedRead<'a>> {
        let indexes = self.offered(indexes);
        let sort = self.sort.first()?;
        self.limit?;
        if self.ids().is_some() {
            return None; // a few documents by id beat any walk (SPEC §61)
        }
        if sort.field == ID_FIELD {
            return self.id_order(indexes);
        }
        let mut best: Option<Walk> = None;
        for index in indexes {
            // A multikey index holds a document once per element, in
            // element order: no order of documents (SPEC §42.3).
            if index.single().is_some_and(is_multi) {
                continue;
            }
            if index.sparse && !excludes_null(index, &self.conditions) {
                continue;
            }
            let Some(at) = index.fields.iter().position(|f| *f == sort.field) else {
                continue;
            };
            let (prefix, equal, fixing) = equal_prefix(index, &self.conditions, at);
            if equal != at {
                continue;
            }
            let served = self
                .sort
                .iter()
                .zip(&index.fields[at..])
                .take_while(|(key, field)| key.field == **field && key.order == sort.order)
                .count();
            let better =
                |(.., fixed, most): &(_, _, _, usize, usize)| (at, served) > (*fixed, *most);
            if best.as_ref().is_none_or(better) {
                best = Some((index, prefix, fixing, at, served));
            }
        }
        let (index, prefix, mut by, fixed, served) = best?;
        if fixed == 0 && self.found_by_value(&sort.field, indexes) {
            return None;
        }
        let range = field_range(index, &self.conditions, &sort.field).map(|(range, on_sort)| {
            by.extend(on_sort);
            range
        });
        let range = match fixed {
            0 => range,
            _ => Some(range.map_or_else(|| KeyRange::prefixed(&prefix), |r| r.under(&prefix))),
        };
        Some(OrderedRead {
            index: Some(index),
            range,
            by,
            fixed,
            served,
            by_id: false,
        })
    }

    /// `index_order` for a sort whose first key is `_id` (SPEC §96): the
    /// primary index, which holds every document in `_id` order — its
    /// key is the id, and every document an object carrying it as `_id`
    /// (§95). Ids are unique, so it serves every sort key: no two
    /// documents tie in the first. The `_id` comparisons with an id
    /// narrow the walk, exactly; any other condition is only checked.
    ///
    /// Where the conditions find documents by value, the range they find
    /// them by is walked instead, if `Eq`s fix every field of its index
    /// (SPEC §103): its keys then differ only in the id they end with, so
    /// they're in id order too. The `_id` comparisons narrow it the same
    /// way, under the fixed values. Otherwise no walk, as for any sort.
    fn id_order<'a>(&self, indexes: &'a [IndexMeta]) -> Option<OrderedRead<'a>> {
        let (ranges, mut by): (Vec<KeyRange>, Vec<Condition>) = self
            .conditions
            .iter()
            .filter_map(|c| match c {
                Condition::Compare {
                    field,
                    op,
                    value: Document::Id(id),
                } if field == ID_FIELD => Some((key::primary_range(op, *id)?, c.clone())),
                _ => None,
            })
            .unzip();
        let on_id = ranges.into_iter().reduce(KeyRange::intersect);
        if !self.found_by_value(ID_FIELD, indexes) {
            if !self.primary_offered() {
                return None; // hinted to an index it can't walk (SPEC §132.2)
            }
            return Some(OrderedRead {
                index: None,
                range: on_id,
                by,
                fixed: 0,
                served: self.sort.len(),
                by_id: true,
            });
        }
        let index = self.fixed_whole(indexes)?;
        let (prefix, _, mut fixing) = equal_prefix(index, &self.conditions, index.fields.len());
        fixing.append(&mut by);
        let range = on_id.map_or_else(|| KeyRange::prefixed(&prefix), |r| r.under(&prefix));
        Some(OrderedRead {
            index: Some(index),
            range: Some(range),
            by: fixing,
            fixed: index.fields.len(),
            served: self.sort.len(),
            by_id: true,
        })
    }

    /// The index the range read takes (`index_ranges`), if that is one
    /// range and an `Eq` fixes each of the index's fields (SPEC §103.2):
    /// walked, it reads what the range read would, in id order. Never
    /// another index, which might hold many more documents.
    fn fixed_whole<'a>(&self, indexes: &'a [IndexMeta]) -> Option<&'a IndexMeta> {
        let ranges = self.index_ranges(indexes)?;
        let [read] = &ranges[..] else {
            return None; // an OR: several ranges, each in id order
        };
        // A sparse one only where the conditions keep its nulls out
        // (§44): `index_ranges` checked.
        let index = read.index;
        let (_, equal, _) = equal_prefix(index, &self.conditions, index.fields.len());
        (equal == index.fields.len()).then_some(index)
    }

    /// Whether the conditions on fields other than `sort_field` find
    /// documents by value — an `Eq` on an indexed field, or an OR of
    /// indexed branches: a few documents found so beat a walk in sort
    /// order (§34.2).
    fn found_by_value(&self, sort_field: &str, indexes: &[IndexMeta]) -> bool {
        let others: Vec<Condition> = self
            .conditions
            .iter()
            .filter(|c| !matches!(c, Condition::Compare { field, .. } if field == sort_field))
            .cloned()
            .collect();
        bounds_for_all(&others, indexes).is_some_and(|b| b.kind != BoundKind::ByRange)
    }
}

/// An index `index_order` could walk: it, the prefix its fixed fields
/// encode to, the `Eq`s that fix them, how many, and how many sort keys
/// it serves.
type Walk<'a> = (&'a IndexMeta, Vec<u8>, Vec<Condition>, usize, usize);

/// A range of an index `find` reads (`Filter::index_ranges`).
#[derive(Debug)]
pub(crate) struct IndexRange<'a> {
    pub index: &'a IndexMeta,
    pub range: KeyRange,
    /// The conditions the range comes from, as the index sees them
    /// (SPEC §94.2): what `explain_run` reports as its bounds.
    pub by: Vec<Condition>,
}

/// How `find` reads an index in sort order (`Filter::index_order`).
#[derive(Debug)]
pub(crate) struct OrderedRead<'a> {
    /// `None` for the primary index, in `_id` order (SPEC §96).
    pub index: Option<&'a IndexMeta>,
    /// The part of it to read — `None` for all of it.
    pub range: Option<KeyRange>,
    /// The conditions `range` comes from (§94.2): the `Eq`s that fix
    /// its first fields, the comparisons on the sort field.
    pub by: Vec<Condition>,
    /// How many of its first fields an `Eq` fixes: the first sort
    /// field is the one after them.
    pub fixed: usize,
    /// How many sort keys, from the first, its fields give the order of
    /// (SPEC §47) — at least one.
    pub served: usize,
    /// Whether it's walked in id order, each key one document's (SPEC
    /// §96, §103): the primary index, or a range of one whose `Eq`s
    /// fix every field.
    pub by_id: bool,
}

impl OrderedRead<'_> {
    /// The index's name, as `explain` gives it: `_id` for the primary
    /// index.
    pub fn name(&self) -> String {
        self.index
            .map_or_else(|| ID_FIELD.to_string(), IndexMeta::name)
    }
}

/// A read whose index entries decide a filter (SPEC §100): each entry,
/// where its values are exact, gives a document of the fields the
/// filter names and `_id`, which matches and sorts as the whole one
/// does.
pub(crate) struct Covering<'a> {
    /// The index's fields — none for the primary index.
    fields: &'a [String],
    /// Which of them the filter names (`Filter::covered_by`).
    named: Vec<bool>,
    /// Whether it names `_id`: without, the entry's document goes
    /// without, and a filter that names nothing decides with no work.
    id: bool,
    /// The named fields' paths, parsed (SPEC §102.4); `None` for the
    /// others.
    paths: Vec<Option<Path<'a>>>,
}

impl<'a> Covering<'a> {
    /// For a read of `index` (`None`: the primary index), if its entries
    /// decide `filter` — and with `sort`, its order. Never a multikey
    /// index: an entry is one element, not the array (§42). Nor where
    /// one named field is inside another, `a` and `a.b`: one document
    /// can't hold both values as the key has them (§31).
    pub fn new(filter: &Filter, index: Option<&'a IndexMeta>, sort: bool) -> Option<Self> {
        let fields: &[String] = index.map_or(&[], |index| &index.fields);
        if fields.iter().any(|field| is_multi(field)) {
            return None;
        }
        let named = filter.covered_by(fields, sort)?;
        let paths: Vec<&String> = (fields.iter().zip(&named))
            .filter_map(|(field, named)| named.then_some(field))
            .collect();
        let inside = |outer: &str, inner: &str| {
            inner.len() > outer.len()
                && inner.starts_with(outer)
                && inner.as_bytes()[outer.len()] == b'.'
        };
        if paths.iter().any(|a| paths.iter().any(|b| inside(a, b))) {
            return None;
        }
        let mut id = filter.conditions.iter().any(names_id);
        if sort {
            id |= filter.sort.iter().any(|key| key.field == ID_FIELD);
        }
        Some(Covering::naming(fields, named, id))
    }

    fn naming(fields: &'a [String], named: Vec<bool>, id: bool) -> Self {
        let paths = (fields.iter().zip(&named))
            .map(|(field, named)| named.then(|| Path::new(field)))
            .collect();
        Covering {
            fields,
            named,
            id,
            paths,
        }
    }

    /// The same read, its entries' documents holding only what `filter`'s
    /// sort names (SPEC §102.4): for matches the entries decided already,
    /// kept for a sort in memory, which reads nothing else.
    pub fn for_sort(self, filter: &Filter) -> Self {
        let sorts_by = |field: &str| filter.sort.iter().any(|key| key.field == field);
        let named = self.fields.iter().map(|field| sorts_by(field)).collect();
        Covering::naming(self.fields, named, sorts_by(ID_FIELD))
    }

    /// The document `key` tells of: the named fields' values at their
    /// paths, and `_id` if named. `None` if a named value isn't exact
    /// (`key::decode_part`) — the document must be read — or the key is
    /// damaged.
    pub fn entry(&self, key: &[u8]) -> Option<Document> {
        let mut doc = Document::Object(Map::new());
        self.fill(key, &mut doc)?;
        Some(doc)
    }

    /// Whether the document `key` tells of matches the filter `matcher`
    /// was compiled from — `None` where the entry can't tell (`entry`).
    /// `scratch` is filled in place, so deciding entry after entry
    /// allocates next to nothing.
    pub fn decide(&self, key: &[u8], matcher: &Matcher, scratch: &mut Document) -> Option<bool> {
        self.fill(key, scratch)?;
        Some(matcher.matches(scratch))
    }

    /// `entry` into `doc`, an object, its slots reused.
    fn fill(&self, key: &[u8], doc: &mut Document) -> Option<()> {
        let Document::Object(map) = doc else {
            *doc = Document::Object(Map::new());
            return self.fill(key, doc);
        };
        if self.id {
            *crate::path::get_or_insert(map, ID_FIELD) = Document::Id(key::doc_id(key));
        }
        if !self.named.contains(&true) {
            return Some(());
        }
        let mut parts = key::each_part(key::value_part(key));
        for path in &self.paths {
            let part = parts.next()?;
            if let Some(path) = path {
                key::decode_part_into(part, self.fields.len(), path.slot(map))?;
            }
        }
        // A damaged key with more values than the index has fields.
        parts.next().is_none().then_some(())
    }
}

impl<'a> Covering<'a> {
    /// The bytes of `key` that decide (SPEC §101.2): its values up to the
    /// end of the last one the filter names. Two keys that agree on them
    /// tell of documents the filter can't tell apart. `None` where the
    /// filter names `_id`, which every key holds a different one of, and
    /// for a damaged key with fewer values than that.
    fn deciding_len(&self, key: &[u8]) -> Option<usize> {
        if self.id {
            return None;
        }
        let Some(last) = self.named.iter().rposition(|named| *named) else {
            return Some(0); // nothing named: every key decides alike
        };
        let mut parts = key::each_part(key::value_part(key));
        (0..=last).try_fold(0, |len, _| Some(len + parts.next()?.len()))
    }

    /// Entry after entry with `filter` (SPEC §101.2), on the bytes of
    /// its entries where they tell (§104).
    pub fn decider(self, filter: &'a Filter) -> Decider<'a> {
        let on_bytes = EntryMatcher::new(filter, self.fields);
        Decider {
            covering: self,
            matcher: filter.matcher(),
            on_bytes,
            scratch: Document::Null,
            last: Vec::new(),
            decided: None,
        }
    }
}

/// A `Covering` deciding the entries of a walk, one after another (SPEC
/// §101.2): an entry whose deciding bytes agree with the last decided
/// one's takes its decision, without a value decoded or a condition
/// checked. For `tenant == x` that's one decision for the whole range.
pub(crate) struct Decider<'a> {
    covering: Covering<'a>,
    matcher: Matcher<'a>,
    /// The filter on the entries' bytes (SPEC §104), tried before
    /// `matcher` on the entry's values.
    on_bytes: EntryMatcher<'a>,
    scratch: Document,
    /// The deciding bytes of the last entry decided, and how — `None`
    /// where it couldn't be, or its bytes don't count (`deciding_len`).
    last: Vec<u8>,
    decided: Option<bool>,
}

impl Decider<'_> {
    /// `Covering::decide` on `key`, or the last decision where `key`
    /// agrees with it.
    pub fn decide(&mut self, key: &[u8]) -> Option<bool> {
        let len = self.covering.deciding_len(key);
        if let (Some(len), Some(decided)) = (len, self.decided)
            && key[..len] == self.last[..]
        {
            return Some(decided);
        }
        #[cfg(test)]
        DECISIONS.with(|n| n.set(n.get() + 1));
        let decided = self.on_bytes.matches(key).or_else(|| {
            #[cfg(test)]
            FILLS.with(|n| n.set(n.get() + 1));
            self.covering.decide(key, &self.matcher, &mut self.scratch)
        });
        self.decided = len.and(decided);
        if let (Some(len), Some(_)) = (len, self.decided) {
            self.last.clear();
            self.last.extend_from_slice(&key[..len]);
        }
        decided
    }

    /// One decision for every key from `lowest` to `highest`, a leaf's
    /// ends (SPEC §101.3) — `None` unless they agree on their deciding
    /// bytes. Keys are in order and every value's encoding ends where it
    /// ends, whatever follows (§28.1): every key between two that share
    /// those bytes shares them too, and so the decision.
    ///
    /// Or, for a filter that accepts one interval of keys, true where
    /// both ends match on their bytes (SPEC §104.4): every key between
    /// them does.
    pub fn decide_all(&mut self, lowest: &[u8], highest: &[u8]) -> Option<bool> {
        let len = self.covering.deciding_len(lowest);
        if let Some(len) = len
            && highest.get(..len) == Some(&lowest[..len])
        {
            return self.decide(lowest);
        }
        if !self.on_bytes.interval {
            return None;
        }
        let on_bytes = |key: &[u8]| {
            #[cfg(test)]
            DECISIONS.with(|n| n.set(n.get() + 1));
            self.on_bytes.matches(key)
        };
        (on_bytes(lowest) == Some(true) && on_bytes(highest) == Some(true)).then_some(true)
    }
}

/// A filter compiled against an index's fields, deciding an entry on its
/// key's bytes (SPEC §104.2): each comparison with its value encoded as
/// the key encodes it (`key::compare_part`). `None` where the bytes
/// can't tell — `contains` on a string, an inexact part equal to the
/// value — and the entry's values decide instead.
pub(crate) struct EntryMatcher<'f> {
    /// The filter's conditions, all holding; `None` if one isn't
    /// a comparison, an AND, an OR or a NOT.
    all: Option<Vec<OnBytes<'f>>>,
    fields: usize,
    /// Whether the filter accepts one interval of keys (SPEC §104.4):
    /// comparisons, nested in ANDs only, other than `ne` and
    /// `contains`, on the index's fields, each after fields an `eq`
    /// among them fixes.
    pub interval: bool,
}

/// One condition of an `EntryMatcher`.
enum OnBytes<'f> {
    /// `op` with `value`, encoded (`None` where no key holds it), on
    /// the key's part `at` — `None` for the id at its end.
    Compare {
        at: Option<usize>,
        op: &'f Op,
        value: &'f Document,
        encoded: Option<Vec<u8>>,
    },
    All(Vec<OnBytes<'f>>),
    Any(Vec<OnBytes<'f>>),
    Not(Box<OnBytes<'f>>),
}

impl<'f> EntryMatcher<'f> {
    pub fn new(filter: &'f Filter, fields: &[String]) -> Self {
        let n = fields.len();
        fn of<'f>(c: &'f Condition, fields: &[String]) -> Option<OnBytes<'f>> {
            let all =
                |cs: &'f [Condition]| cs.iter().map(|c| of(c, fields)).collect::<Option<Vec<_>>>();
            Some(match c {
                Condition::Compare { field, op, value } => {
                    let at = match field == ID_FIELD {
                        true => None,
                        false => Some(fields.iter().position(|f| f == field)?),
                    };
                    let encoded = match at {
                        Some(_) => key::encode_for(value, fields.len()),
                        None => None,
                    };
                    OnBytes::Compare {
                        at,
                        op,
                        value,
                        encoded,
                    }
                }
                Condition::All(cs) => OnBytes::All(all(cs)?),
                Condition::Any(cs) => OnBytes::Any(all(cs)?),
                Condition::Not(c) => OnBytes::Not(Box::new(of(c, fields)?)),
                Condition::Exists { .. } | Condition::Size { .. } | Condition::ElemMatch { .. } => {
                    return None;
                }
            })
        }
        let all: Option<Vec<OnBytes>> = filter.conditions.iter().map(|c| of(c, fields)).collect();
        let interval = all.as_deref().is_some_and(is_interval);
        EntryMatcher {
            all,
            fields: n,
            interval,
        }
    }

    /// Whether the entry `key` stands for matches, as far as its bytes
    /// tell (SPEC §104.2).
    pub fn matches(&self, key: &[u8]) -> Option<bool> {
        let all = self.all.as_ref()?;
        let mut parts: [&[u8]; key::MAX_COMPOUND_FIELDS] = [&[]; key::MAX_COMPOUND_FIELDS];
        let mut each = key::each_part(key::value_part(key));
        for part in parts.iter_mut().take(self.fields) {
            *part = each.next()?;
        }
        if each.next().is_some() {
            return None; // damaged: more values than fields
        }
        let entry = Entry {
            parts: &parts[..self.fields],
            key,
            fields: self.fields,
        };
        all_of(all, &entry)
    }
}

/// An entry's key, taken apart.
struct Entry<'k> {
    parts: &'k [&'k [u8]],
    key: &'k [u8],
    fields: usize,
}

impl OnBytes<'_> {
    fn matches(&self, entry: &Entry) -> Option<bool> {
        match self {
            OnBytes::Compare {
                at,
                op,
                value,
                encoded,
            } => {
                let Some(at) = at else {
                    let ord = match value {
                        Document::Id(id) => Some(key::doc_id(entry.key).cmp(id)),
                        _ => None,
                    };
                    return Some(!matches!(op, Op::Contains) && holds(op, ord));
                };
                let part = entry.parts[*at];
                // An array, object or binary value in a compound key is
                // its tag alone (SPEC §43): equal to one asked for or
                // not, only the document tells (§124.3).
                if matches!(op, Op::Eq | Op::Ne) && key::is_other(part) && is_whole(value) {
                    return None;
                }
                if let Op::Contains = op {
                    // Case folding needs the text (SPEC §25).
                    return match (key::is_string(part), value) {
                        (true, Document::String(_)) => None,
                        _ => Some(false),
                    };
                }
                let ord = key::compare_part(part, encoded.as_deref(), entry.fields)?;
                Some(holds(op, ord))
            }
            OnBytes::All(all) => all_of(all, entry),
            OnBytes::Any(any) => {
                let mut undecided = false;
                for c in any {
                    match c.matches(entry) {
                        Some(true) => return Some(true),
                        None => undecided = true,
                        Some(false) => {}
                    }
                }
                (!undecided).then_some(false)
            }
            OnBytes::Not(c) => c.matches(entry).map(|m| !m),
        }
    }
}

/// Every one of `all` holds, three-valued: a false one decides, else an
/// undecided one leaves it undecided.
fn all_of(all: &[OnBytes], entry: &Entry) -> Option<bool> {
    let mut undecided = false;
    for c in all {
        match c.matches(entry) {
            Some(false) => return Some(false),
            None => undecided = true,
            Some(true) => {}
        }
    }
    (!undecided).then_some(true)
}

/// Whether `all`, all holding, accept one interval of keys (SPEC §104.4):
/// a key between two that match on their bytes matches too.
fn is_interval(all: &[OnBytes]) -> bool {
    let mut compares = Vec::new();
    fn flat<'f>(all: &[OnBytes<'f>], out: &mut Vec<(usize, &'f Op)>) -> bool {
        all.iter().all(|c| match c {
            OnBytes::Compare {
                at: Some(at), op, ..
            } if matches!(op, Op::Eq | Op::Lt | Op::Lte | Op::Gt | Op::Gte) => {
                out.push((*at, *op));
                true
            }
            OnBytes::All(inner) => flat(inner, out),
            _ => false,
        })
    }
    flat(all, &mut compares)
        && compares.iter().all(|(at, _)| {
            (0..*at).all(|before| {
                compares
                    .iter()
                    .any(|(i, op)| *i == before && **op == Op::Eq)
            })
        })
}

#[cfg(test)]
thread_local! {
    /// Entries a `Decider` decided on their values, rather than by the
    /// last decision (SPEC §101.2), for tests.
    pub(crate) static DECISIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Entries the bytes didn't decide, their values put into a document
    /// to be checked (SPEC §104.3), for tests.
    pub(crate) static FILLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether `condition` names `_id` anywhere.
fn names_id(condition: &Condition) -> bool {
    match condition {
        Condition::Compare { field, .. } => field == ID_FIELD,
        Condition::All(conditions) | Condition::Any(conditions) => conditions.iter().any(names_id),
        Condition::Not(condition) => names_id(condition),
        _ => false,
    }
}

/// The value at `path` in `doc` (SPEC §31): a field name, or several
/// joined by dots — `address.city` is the `city` field of the object in
/// the `address` field. `None` if a step is missing or isn't an object;
/// arrays aren't walked into. A dot always separates, so a key that
/// itself contains a dot can't be reached by a path. The empty path is
/// `doc` itself — an element, in `Condition::elem_match` (SPEC §46.1).
pub(crate) fn field_value<'a>(doc: &'a Document, path: &str) -> Option<&'a Document> {
    Path::new(path).field_value(doc)
}

/// The value a condition or sort sees at `path`: a missing field reads
/// as `Null` (SPEC §32), so `x == null` finds documents without `x`, and
/// every condition treats "missing" and "null" alike.
pub(crate) fn value_or_null<'a>(doc: &'a Document, path: &str) -> &'a Document {
    field_value(doc, path).unwrap_or(&Document::Null)
}

/// Whether `path` has an `[*]` step (SPEC §42): it reaches each element
/// of an array, so it reaches any number of values, not one.
pub(crate) fn is_multi(path: &str) -> bool {
    path.contains("[*]")
}

/// Every value at `path` in `doc` (SPEC §42). A step is a field name
/// followed by any number of `[*]`, each of which goes on with every
/// element of the array there. The rules:
/// - a missing field is null, as everywhere (§32) — so an element
///   without the field gives a null;
/// - `[*]` on anything but an array gives nothing: a missing, null or
///   scalar `tags` has no elements;
/// - a path without `[*]` gives exactly `value_or_null`;
/// - a path that is empty or starts with `[*]` starts at `doc` itself,
///   for an element in `Condition::elem_match` (SPEC §46.1).
pub(crate) fn values_at<'a>(doc: &'a Document, path: &str) -> Vec<&'a Document> {
    Path::new(path).values(doc)
}

/// A filter's conditions with their paths parsed once (SPEC §102.2):
/// built when a read starts (`Filter::matcher`), then asked about every
/// document, as `Condition::matches` would be.
pub(crate) enum Matcher<'f> {
    /// A comparison on a path with `[*]` holds if it holds for any value
    /// there (SPEC §42.1) — except `Ne`, which stays what it is
    /// everywhere: `Eq` negated. So `tags[*] != "x"` means no tag is
    /// `"x"`. `needle`: a `Contains` string's value, case-folded once.
    Compare {
        path: Path<'f>,
        op: &'f Op,
        value: &'f Document,
        needle: Option<String>,
    },
    Exists(Path<'f>),
    Size {
        path: Path<'f>,
        op: &'f Op,
        size: Document,
    },
    ElemMatch {
        path: Path<'f>,
        condition: Box<Matcher<'f>>,
    },
    All(Vec<Matcher<'f>>),
    Any(Vec<Matcher<'f>>),
    Not(Box<Matcher<'f>>),
}

impl<'f> Matcher<'f> {
    pub fn of(condition: &'f Condition) -> Self {
        match condition {
            Condition::Compare { field, op, value } => Matcher::Compare {
                path: Path::new(field),
                op,
                value,
                needle: match (op, value) {
                    (Op::Contains, Document::String(needle)) => Some(fold_case(needle)),
                    _ => None,
                },
            },
            Condition::Exists { field } => Matcher::Exists(Path::new(field)),
            Condition::Size { field, op, size } => Matcher::Size {
                path: Path::new(field),
                op,
                size: Document::Int(*size as i64),
            },
            Condition::ElemMatch { field, condition } => Matcher::ElemMatch {
                path: Path::new(field),
                condition: Box::new(Matcher::of(condition)),
            },
            Condition::All(conditions) => {
                Matcher::All(conditions.iter().map(Matcher::of).collect())
            }
            Condition::Any(conditions) => {
                Matcher::Any(conditions.iter().map(Matcher::of).collect())
            }
            Condition::Not(condition) => Matcher::Not(Box::new(Matcher::of(condition))),
        }
    }

    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Matcher::Compare {
                path,
                op,
                value,
                needle,
            } => {
                let holds = |v: &Document| match (op, needle) {
                    (Op::Contains, Some(needle)) => {
                        matches!(v, Document::String(haystack) if fold_case(haystack).contains(needle.as_str()))
                    }
                    _ => value_matches(op, v, value),
                };
                match (path.is_multi(), op) {
                    (false, _) => holds(path.value_or_null(doc)),
                    (true, Op::Ne) => {
                        !(path.values(doc).into_iter()).any(|v| value_matches(&Op::Eq, v, value))
                    }
                    (true, _) => path.values(doc).into_iter().any(holds),
                }
            }
            Matcher::Exists(path) => !path.present(doc).is_empty(),
            // The lengths of the arrays at the path — none if it isn't
            // one; with `[*]`, one per element that is one (SPEC §45.2).
            Matcher::Size { path, op, size } => {
                let lengths: Vec<Document> = (path.values(doc).into_iter())
                    .filter_map(|value| match value {
                        Document::Array(elements) => Some(Document::Int(elements.len() as i64)),
                        _ => None,
                    })
                    .collect();
                any_matches(op, size, &lengths)
            }
            Matcher::ElemMatch { path, condition } => {
                path.values(doc).into_iter().any(|value| match value {
                    Document::Array(elements) => elements.iter().any(|e| condition.matches(e)),
                    _ => false,
                })
            }
            Matcher::All(matchers) => matchers.iter().all(|m| m.matches(doc)),
            Matcher::Any(matchers) => matchers.iter().any(|m| m.matches(doc)),
            Matcher::Not(matcher) => !matcher.matches(doc),
        }
    }
}

/// `op` against `value` for any of `values` — `Ne` for none being equal.
fn any_matches<'a>(
    op: &Op,
    value: &Document,
    values: impl IntoIterator<Item = &'a Document>,
) -> bool {
    let mut values = values.into_iter();
    match op {
        Op::Ne => !values.any(|v| value_matches(&Op::Eq, v, value)),
        op => values.any(|v| value_matches(op, v, value)),
    }
}

/// `field_value op value`, for one value.
fn value_matches(op: &Op, field_value: &Document, value: &Document) -> bool {
    if let Op::Contains = op {
        return match (field_value, value) {
            (Document::String(haystack), Document::String(needle)) => {
                fold_case(haystack).contains(&fold_case(needle))
            }
            _ => false,
        };
    }
    // Compared whole, by their contents, with `=` and `!=` only (SPEC
    // §124.2): they have no order.
    if is_whole(field_value) || is_whole(value) {
        return match op {
            Op::Eq => equal_values(field_value, value),
            Op::Ne => !equal_values(field_value, value),
            _ => false,
        };
    }
    holds(op, compare(field_value, value))
}

/// An array, an object or binary data: a value `=` and `!=` compare by
/// its contents and nothing orders (SPEC §124).
pub(crate) fn is_whole(value: &Document) -> bool {
    matches!(
        value,
        Document::Array(_) | Document::Object(_) | Document::Binary(_)
    )
}

/// Equal, as a filter's `Eq` sees it (SPEC §124.1), and `add_to_set`
/// and `pull` (§73): single values as `compare` has them (`5` and `5.0`
/// equal, NaN equal to nothing); arrays element by element, in order;
/// objects field by field, in any order; binary byte by byte.
pub(crate) fn equal_values(a: &Document, b: &Document) -> bool {
    match (a, b) {
        (Document::Array(x), Document::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| equal_values(x, y))
        }
        (Document::Object(x), Document::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, value)| y.get(key).is_some_and(|other| equal_values(value, other)))
        }
        (Document::Binary(x), Document::Binary(y)) => x == y,
        _ => equal(a, b),
    }
}

/// `value` in a form in which values `equal_values` finds equal are
/// identical (SPEC §124.4), for grouping: an object's fields by name,
/// and a `Float` with a whole value an `i64` holds as that `Int`, at
/// every depth. NaN stays as it is, equal to nothing.
pub(crate) fn canonical(value: &Document) -> Document {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    match value {
        Document::Float(f) if f.fract() == 0.0 && *f >= -TWO_POW_63 && *f < TWO_POW_63 => {
            Document::Int(*f as i64)
        }
        Document::Array(items) => Document::Array(items.iter().map(canonical).collect()),
        Document::Object(fields) => {
            let mut sorted: Vec<(&String, &Document)> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut map = Map::new();
            for (name, value) in sorted {
                map.insert(name.clone(), canonical(value));
            }
            Document::Object(map)
        }
        other => other.clone(),
    }
}

/// Whether `op` holds of two values that compare as `ord` (`compare`):
/// what `value_matches` and an entry decided on its bytes (SPEC §104.2)
/// answer. Not for `Op::Contains`.
fn holds(op: &Op, ord: Option<std::cmp::Ordering>) -> bool {
    match (op, ord) {
        (Op::Eq, Some(std::cmp::Ordering::Equal)) => true,
        (Op::Ne, ord) => ord != Some(std::cmp::Ordering::Equal),
        (Op::Lt, Some(std::cmp::Ordering::Less)) => true,
        (Op::Lte, Some(o)) => o != std::cmp::Ordering::Greater,
        (Op::Gt, Some(std::cmp::Ordering::Greater)) => true,
        (Op::Gte, Some(o)) => o != std::cmp::Ordering::Less,
        _ => false,
    }
}

/// Case folding for `Op::Contains`: Unicode lowercasing, plus `ß` → `ss`
/// — `to_lowercase` leaves `ß` alone, so without it "Straße" wouldn't
/// contain "STRASSE", a likely search in German text. Not full Unicode
/// case folding (that needs the Unicode `CaseFolding` table, i.e. a
/// dependency), but it covers the everyday cases.
fn fold_case(s: &str) -> String {
    s.to_lowercase().replace('ß', "ss")
}

/// `a == b` as a filter's `Eq` sees it — what a unique index forbids
/// twice (SPEC §33).
pub(crate) fn equal(a: &Document, b: &Document) -> bool {
    compare(a, b) == Some(std::cmp::Ordering::Equal)
}

/// Only values of one kind compare; `Null` equals `Null` (SPEC §32), so
/// `Eq`/`Lte`/`Gte` against null match null and missing fields, `Ne`
/// everything else, and `Lt`/`Gt` nothing. Numbers compare by their exact
/// value, `Int` against `Float` too (SPEC §34.1).
pub(crate) fn compare(a: &Document, b: &Document) -> Option<std::cmp::Ordering> {
    use Document::*;
    match (a, b) {
        (Null, Null) => Some(std::cmp::Ordering::Equal),
        (Int(x), Int(y)) => x.partial_cmp(y),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => int_vs_float(*x, *y),
        (Float(x), Int(y)) => int_vs_float(*y, *x).map(std::cmp::Ordering::reverse),
        (String(x), String(y)) => x.partial_cmp(y),
        (Bool(x), Bool(y)) => x.partial_cmp(y),
        // By their bytes: creation order, for UUIDv7 ids. Missing until
        // §59, so no filter on an id matched (not even `eq("_id", id)`).
        // Sorting still ranks ids as unordered (`sort_rank`).
        (Id(x), Id(y)) => Some(x.cmp(y)),
        // In time order (SPEC §69); never equal to a string or number.
        (DateTime(x), DateTime(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// `i` against `f` by exact value — not `i as f64`, which rounds beyond
/// 2^53: `2^53 + 1` would equal the float `2^53` and yet be greater than
/// the int `2^53`, and a sort can't be consistent on top of that. `None`
/// for NaN.
fn int_vs_float(i: i64, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering::*;
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if f.is_nan() {
        return None;
    }
    if f >= TWO_POW_63 {
        return Some(Less); // above every i64
    }
    if f < -TWO_POW_63 {
        return Some(Greater); // below every i64
    }
    // Within i64's range: the whole part is exact as an i64, and a
    // fractional part decides between equal whole parts.
    let whole = f.trunc();
    Some(
        i.cmp(&(whole as i64))
            .then_with(|| whole.partial_cmp(&f).expect("neither is NaN")),
    )
}

/// Kinds of value in sort order (SPEC §34.1): the order of the index's
/// type tags (`index::key`), and a last kind for values nothing orders.
fn sort_rank(value: &Document) -> u8 {
    match value {
        Document::Null => 0,
        Document::Bool(_) => 1,
        Document::Float(f) if f.is_nan() => UNORDERED,
        Document::Int(_) | Document::Float(_) => 2,
        Document::String(_) => 3,
        Document::Id(_) => 4,
        Document::DateTime(_) => 5,
        _ => UNORDERED,
    }
}

const UNORDERED: u8 = 6;

/// Whether `sort_order` puts `value` among the values nothing orders —
/// arrays, objects, binary, NaN. No index holds them.
pub(crate) fn is_unordered(value: &Document) -> bool {
    sort_rank(value) == UNORDERED
}

/// `a` against `b` by the sort `keys` — by the first, where that's
/// equal by the second, and so on (SPEC §47) — on values looked up
/// already (SPEC §102.5): `a` and `b` hold each item's value for each
/// key, in order.
fn compare_values(keys: &[Sort], a: &[&Document], b: &[&Document]) -> std::cmp::Ordering {
    (keys.iter().zip(a.iter().zip(b)))
        .map(|(key, (a, b))| sort_order(a, b, key.order))
        .find(|ord| ord.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// The order a sort puts two field values in (SPEC §34.1) — a total
/// order, so any sort is well-defined: null (and missing) before bools
/// before numbers before strings before ids (SPEC §62) before date-times
/// (SPEC §69), each by value, reversed for `Desc`;
/// values nothing orders come after all of them in both directions, as
/// equals. It is the order of an index's keys, so reading an index gives
/// what sorting in memory gives.
pub(crate) fn sort_order(a: &Document, b: &Document, order: SortOrder) -> std::cmp::Ordering {
    let (rank_a, rank_b) = (sort_rank(a), sort_rank(b));
    if rank_a == UNORDERED || rank_b == UNORDERED {
        return rank_a.cmp(&rank_b);
    }
    let ord = rank_a
        .cmp(&rank_b)
        .then_with(|| compare(a, b).expect("values of one orderable kind compare"));
    match order {
        SortOrder::Asc => ord,
        SortOrder::Desc => ord.reverse(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths, matching and sorting as they were before SPEC §102: each
    /// path taken apart on every call, every step hashed, every sort key
    /// looked up on every comparison. The reference §102 is checked
    /// against.
    mod before_102 {
        use super::super::{Condition, Document, Sort, any_matches, sort_order, value_matches};

        pub fn field_value<'a>(doc: &'a Document, path: &str) -> Option<&'a Document> {
            if path.is_empty() {
                return Some(doc);
            }
            path.split('.').try_fold(doc, |value, name| match value {
                Document::Object(map) => map.get(name),
                _ => None,
            })
        }

        pub fn value_or_null<'a>(doc: &'a Document, path: &str) -> &'a Document {
            field_value(doc, path).unwrap_or(&Document::Null)
        }

        pub fn walk<'a>(doc: &'a Document, path: &str, missing_is_null: bool) -> Vec<&'a Document> {
            let missing = missing_is_null.then_some(&Document::Null);
            let mut values = vec![doc];
            for (i, step) in path.split('.').enumerate() {
                let name = step.trim_end_matches("[*]");
                let fan_outs = (step.len() - name.len()) / 3;
                if i > 0 || !name.is_empty() {
                    values = values
                        .into_iter()
                        .filter_map(|value| match value {
                            Document::Object(map) => map.get(name).or(missing),
                            _ => missing,
                        })
                        .collect();
                }
                for _ in 0..fan_outs {
                    values = values
                        .into_iter()
                        .flat_map(|value| match value {
                            Document::Array(elements) => elements.iter().collect(),
                            _ => Vec::new(),
                        })
                        .collect();
                }
            }
            values
        }

        pub fn matches(condition: &Condition, doc: &Document) -> bool {
            match condition {
                Condition::Compare { field, op, value } => {
                    if field.contains("[*]") {
                        return any_matches(op, value, walk(doc, field, true));
                    }
                    value_matches(op, value_or_null(doc, field), value)
                }
                Condition::Exists { field } => !walk(doc, field, false).is_empty(),
                Condition::Size { field, op, size } => {
                    let lengths: Vec<Document> = walk(doc, field, true)
                        .into_iter()
                        .filter_map(|value| match value {
                            Document::Array(elements) => Some(Document::Int(elements.len() as i64)),
                            _ => None,
                        })
                        .collect();
                    any_matches(op, &Document::Int(*size as i64), &lengths)
                }
                Condition::ElemMatch { field, condition } => {
                    walk(doc, field, true).into_iter().any(|value| match value {
                        Document::Array(elements) => elements.iter().any(|e| matches(condition, e)),
                        _ => false,
                    })
                }
                Condition::All(conditions) => conditions.iter().all(|c| matches(c, doc)),
                Condition::Any(conditions) => conditions.iter().any(|c| matches(c, doc)),
                Condition::Not(condition) => !matches(condition, doc),
            }
        }

        pub fn compare_by(keys: &[Sort], a: &Document, b: &Document) -> std::cmp::Ordering {
            keys.iter()
                .map(|key| {
                    let (a, b) = (value_or_null(a, &key.field), value_or_null(b, &key.field));
                    sort_order(a, b, key.order)
                })
                .find(|ord| ord.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
        }
    }

    /// Names from a few letters, the empty one, and ones holding a dot or
    /// `[*]`, which a path can't reach or reads as a step's end.
    const NAMES: [&str; 8] = ["a", "b", "c", "", "a.b", "[*]", "a[*]", "b[*]x"];

    fn random_doc(rng: &mut crate::testing::XorShift, depth: usize) -> Document {
        match rng.below(if depth == 0 { 4 } else { 7 }) {
            0 => Document::Int(rng.below(5) as i64 - 2),
            1 => Document::String(["", "a", "Straße", "STRASSE x"][rng.below(4)].to_string()),
            2 => Document::Null,
            3 => Document::Float(f64::NAN),
            4 => Document::Array(
                (0..rng.below(4))
                    .map(|_| random_doc(rng, depth - 1))
                    .collect(),
            ),
            _ => Document::Object(
                (0..rng.below(12))
                    .map(|_| {
                        let name = NAMES[rng.below(NAMES.len())].to_string();
                        (name, random_doc(rng, depth - 1))
                    })
                    .collect(),
            ),
        }
    }

    fn random_path(rng: &mut crate::testing::XorShift) -> String {
        let steps = ["a", "b", "c", "", "[*]", "a[*]", "b[*][*]", "c[*]x", "a.b"];
        (0..rng.below(4))
            .map(|_| steps[rng.below(steps.len())])
            .collect::<Vec<_>>()
            .join(".")
    }

    fn random_condition(rng: &mut crate::testing::XorShift, depth: usize) -> Condition {
        let ops = [
            Op::Eq,
            Op::Ne,
            Op::Lt,
            Op::Lte,
            Op::Gt,
            Op::Gte,
            Op::Contains,
        ];
        let op = ops[rng.below(ops.len())].clone();
        if depth == 0 || rng.below(3) == 0 {
            let field = random_path(rng);
            return match rng.below(8) {
                0 => Condition::Exists { field },
                1 => Condition::Size {
                    field,
                    op,
                    size: rng.below(3),
                },
                2 => Condition::ElemMatch {
                    field,
                    condition: Box::new(random_condition(rng, depth.saturating_sub(1))),
                },
                _ => Condition::Compare {
                    field,
                    op,
                    value: random_doc(rng, 0),
                },
            };
        }
        let children = (0..rng.below(4))
            .map(|_| random_condition(rng, depth - 1))
            .collect();
        match rng.below(3) {
            0 => Condition::All(children),
            1 => Condition::Any(children),
            _ => !random_condition(rng, depth - 1),
        }
    }

    /// SPEC §102.1: a parsed path finds what taking it apart on every
    /// call found, on documents with names a path can't reach.
    #[test]
    fn a_parsed_path_finds_what_the_string_did() {
        let mut rng = crate::testing::XorShift(0x0102_9A74_0102_9A74);
        let fixed = [
            "", ".", "..", "[*]", "a", "a.", ".a", "a[*]", "a[*][*]", "a[*]b", "a.b[*]",
        ];
        for i in 0..4000 {
            let doc = random_doc(&mut rng, 4);
            let path = match fixed.get(i % 40) {
                Some(path) => path.to_string(),
                None => random_path(&mut rng),
            };
            let parsed = crate::path::Path::new(&path);
            // The same values in the document, not equal ones: NaN isn't.
            // A null by value, as a missing field reads as a constant one.
            let at = |value: &Document| match value {
                Document::Null => std::ptr::null(),
                value => value as *const Document,
            };
            let all = |values: Vec<&Document>| values.into_iter().map(at).collect::<Vec<_>>();
            assert_eq!(
                parsed.field_value(&doc).map(at),
                before_102::field_value(&doc, &path).map(at),
                "{path:?} in {doc}"
            );
            let (values, present) = (parsed.values(&doc), parsed.present(&doc));
            assert_eq!(
                all(values),
                all(before_102::walk(&doc, &path, true)),
                "{path:?}"
            );
            assert_eq!(
                all(present),
                all(before_102::walk(&doc, &path, false)),
                "{path:?}"
            );
            assert_eq!(parsed.is_multi(), path.contains("[*]"), "{path:?}");
        }
    }

    /// SPEC §102.2: a compiled condition answers as the condition did,
    /// on every kind of condition, nested, and on paths of every shape.
    #[test]
    fn a_matcher_answers_as_the_conditions_did() {
        let mut rng = crate::testing::XorShift(0x0102_3A7C_0102_3A7C);
        let (mut held, mut checked) = (0, 0);
        for _ in 0..4000 {
            let condition = random_condition(&mut rng, 3);
            let matcher = Matcher::of(&condition);
            for _ in 0..5 {
                let doc = random_doc(&mut rng, 4);
                let expected = before_102::matches(&condition, &doc);
                assert_eq!(matcher.matches(&doc), expected, "{condition:?} on {doc}");
                assert_eq!(condition.matches(&doc), expected, "{condition:?} on {doc}");
                held += usize::from(expected);
                checked += 1;
            }
        }
        // Both answers, plenty of each.
        assert!(
            held > checked / 10 && held < checked * 9 / 10,
            "{held} of {checked}"
        );
    }

    /// SPEC §102.3: a name found by scanning a small object, or by hash in
    /// a larger one, is the one `IndexMap::get` finds.
    #[test]
    fn a_field_is_found_by_scan_or_by_hash_alike() {
        for len in 0..=20 {
            let map: Map = (0..len)
                .map(|i| (format!("f{i}"), Document::Int(i as i64)))
                .collect();
            for name in (0..=21)
                .map(|i| format!("f{i}"))
                .chain(["".into(), "f".into()])
            {
                assert_eq!(
                    crate::path::get(&map, &name),
                    map.get(&name),
                    "{len} {name}"
                );
            }
        }
        // Names that are each other's prefixes, changed or added one after
        // another, as `IndexMap`'s own entries would be.
        for len in 0..=12 {
            let names: Vec<String> = (1..=len).map(|n| "k".repeat(n)).collect();
            let map: Map = (names.iter().rev())
                .map(|name| (name.clone(), Document::Int(name.len() as i64)))
                .collect();
            let (mut changed, mut expected) = (map.clone(), map);
            for (i, name) in ["", "kk", "k", "kkkkkkkkkkkkk", "kkkk", "x"]
                .iter()
                .enumerate()
            {
                *crate::path::get_or_insert(&mut changed, name) = Document::Int(100 + i as i64);
                *expected.0.entry(name.to_string()).or_insert(Document::Null) =
                    Document::Int(100 + i as i64);
                let order = |m: &Map| m.keys().cloned().collect::<Vec<_>>();
                assert_eq!(order(&changed), order(&expected), "{len} fields, {name:?}");
                assert_eq!(changed, expected, "{len} fields, {name:?}");
            }
        }
    }

    /// SPEC §102.5: a sort with its keys looked up once orders, skips and
    /// limits as comparing every pair by path did — ties stable,
    /// unordered values last both ways.
    #[test]
    fn a_sort_on_values_looked_up_once_orders_as_before() {
        let mut rng = crate::testing::XorShift(0x0102_5027_0102_5027);
        let fields = ["a", "b", "a.b", "c[*]", ""];
        for _ in 0..600 {
            let items: Vec<(usize, Document)> = (0..rng.below(60))
                .map(|i| (i, random_doc(&mut rng, 3)))
                .collect();
            let mut f = Filter::new();
            for _ in 0..1 + rng.below(3) {
                let field = fields[rng.below(fields.len())];
                f = f.then_by(field, [SortOrder::Asc, SortOrder::Desc][rng.below(2)]);
            }
            let f = f
                .skip([0, 0, 3, 50][rng.below(4)])
                .limit([None, Some(0), Some(1), Some(7)][rng.below(4)]);
            let mut expected = items.clone();
            expected.sort_by(|(_, a), (_, b)| before_102::compare_by(&f.sort, a, b));
            let expected: Vec<usize> = expected
                .into_iter()
                .skip(f.skip)
                .take(f.limit.unwrap_or(usize::MAX))
                .map(|(i, _)| i)
                .collect();
            let arranged: Vec<usize> = f
                .arrange(items, |(_, doc)| doc)
                .into_iter()
                .map(|(i, _)| i)
                .collect();
            assert_eq!(arranged, expected, "{:?}", f.sort);
        }
    }

    /// SPEC §102: a read parses its filter's paths once, and a sort looks
    /// each key up once per item.
    #[test]
    fn paths_are_parsed_and_looked_up_once() {
        let docs: Vec<Document> = (0..1000)
            .map(|i| {
                Document::Object(
                    [
                        ("n".to_string(), Document::Int(i % 37)),
                        ("m".to_string(), Document::Int(i)),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect();
        let f = Filter::new()
            .gte("n", 3)
            .lt("n", 30)
            .sort_desc("n")
            .then_asc("m")
            .limit(10);
        let parsed = crate::path::PATHS_PARSED.with(|n| n.replace(0));
        let looked_up = crate::path::LOOKUPS.with(|n| n.replace(0));
        let found = f.apply(docs);
        let parsed = crate::path::PATHS_PARSED.with(|n| n.replace(parsed));
        let looked_up = crate::path::LOOKUPS.with(|n| n.replace(looked_up));
        assert_eq!(found.len(), 10);
        // Two conditions and two sort keys.
        assert_eq!(parsed, 4);
        // One per condition checked — the second only where the first
        // held — and two per match sorted.
        let first_held = (0..1000).filter(|i| i % 37 >= 3).count();
        let matched = (0..1000).filter(|i| (3..30).contains(&(i % 37))).count();
        assert_eq!(looked_up, 1000 + first_held + 2 * matched);
    }

    /// SPEC §99.2: the top-level field of every path a filter looks at —
    /// in every kind of condition, nested, and the sort keys — once each;
    /// none for a path that starts at the document itself.
    #[test]
    fn a_filter_names_the_fields_it_looks_at() {
        let names = |f: Filter| {
            f.fields()
                .map(|n| n.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        let f = Filter::new()
            .eq("a.b", 1)
            .exists("e")
            .size("tags[*]", Op::Eq, 2)
            .elem_match("items", Condition::eq("sku", "A1"))
            .and(!(Condition::eq("n", 1) | Condition::missing("m.x")))
            .any_of([Condition::eq("a.c", 2), Condition::gt("z", 0)])
            .sort_desc("s.t")
            .then_asc("_id");
        let want = ["a", "e", "tags", "items", "n", "m", "z", "s", "_id"];
        assert_eq!(names(f.clone()), Some(want.map(String::from).to_vec()));
        // The sort keys only when asked.
        let without_sort = f.fields_with(false).unwrap();
        assert_eq!(without_sort, ["a", "e", "tags", "items", "n", "m", "z"]);
        assert_eq!(names(Filter::new()), Some(vec![]));
        // Inside an element: paths there don't name top-level fields.
        let inner = Filter::new().elem_match("rows", Condition::eq("", 1));
        assert_eq!(names(inner), Some(vec!["rows".to_string()]));
        // From the document itself: all of it.
        assert_eq!(names(Filter::new().eq("", 1)), None);
        assert_eq!(names(Filter::new().eq("[*].a", 1)), None);
        assert_eq!(
            names(Filter::new().any_of([Condition::exists("[*]")])),
            None
        );
        assert_eq!(names(Filter::new().sort_asc("")), None);
    }

    /// Ids sort after strings, by when they were made, and before the
    /// values nothing orders, which come last in both directions (SPEC
    /// §62) — the order of their index keys.
    #[test]
    fn ids_sort_after_strings_in_the_order_they_were_made() {
        use std::cmp::Ordering::{Greater, Less};
        let (a, b) = (Document::Id(DocId([1; 16])), Document::Id(DocId([2; 16])));
        let (text, array) = (Document::String("z".into()), Document::Array(vec![]));
        assert_eq!(sort_order(&text, &a, SortOrder::Asc), Less);
        assert_eq!(sort_order(&a, &b, SortOrder::Asc), Less);
        assert_eq!(sort_order(&a, &b, SortOrder::Desc), Greater);
        assert_eq!(sort_order(&b, &array, SortOrder::Asc), Less);
        assert_eq!(sort_order(&b, &array, SortOrder::Desc), Less);
        assert!(!is_unordered(&a) && is_unordered(&array));
    }

    /// SPEC §69: date-times compare with date-times, in time order, and
    /// sort after ids; a string or number never equals one.
    #[test]
    fn datetimes_compare_in_time_order_and_sort_after_ids() {
        use std::cmp::Ordering::Less;
        let at = |secs| Document::DateTime(crate::DateTime::from_unix(secs, 0).unwrap());
        let (before, after) = (at(-5), at(1_790_000_000));
        let item = doc(&[("seen", after.clone())]);
        assert!(Filter::new().eq("seen", after.clone()).matches(&item));
        assert!(Filter::new().gt("seen", before.clone()).matches(&item));
        assert!(!Filter::new().lt("seen", before.clone()).matches(&item));
        assert!(
            !Filter::new()
                .eq("seen", "2026-09-21T14:13:20Z")
                .matches(&item)
        );
        assert!(!Filter::new().gte("seen", 0).matches(&item));
        assert!(Filter::new().ne("seen", 1_790_000_000).matches(&item));

        let id = Document::Id(DocId([0xFF; 16]));
        assert_eq!(sort_order(&id, &before, SortOrder::Asc), Less);
        assert_eq!(sort_order(&before, &after, SortOrder::Asc), Less);
        assert_eq!(
            sort_order(&after, &Document::Array(vec![]), SortOrder::Desc),
            Less
        );
        assert!(!is_unordered(&after));
    }

    /// Which filters name their documents by id (SPEC §61): an `Eq` on
    /// `_id` with an id, at the top or in an `All`, the fewest of several,
    /// an OR only if every branch names some; each id once, in order.
    #[test]
    fn ids_are_what_a_filter_names_its_documents_by() {
        let (a, b, c) = (DocId([1; 16]), DocId([2; 16]), DocId([3; 16]));
        let id = |id: DocId| Condition::eq("_id", id);
        let ids = |f: Filter| f.ids();
        assert_eq!(ids(Filter::new().eq("_id", a)), Some(vec![a]));
        assert_eq!(ids(Filter::new().eq("x", 1).eq("_id", a)), Some(vec![a]));
        assert_eq!(
            ids(Filter::new().and(Condition::All(vec![id(b)]))),
            Some(vec![b])
        );
        let or = Filter::new().any_of([id(c), id(a), id(c)]);
        assert_eq!(ids(or.clone()), Some(vec![a, c]));
        assert_eq!(ids(or.eq("_id", b)), Some(vec![b]));
        assert_eq!(
            ids(Filter::new().any_of([id(a), Condition::eq("x", 1)])),
            None
        );

        assert_eq!(ids(Filter::new()), None);
        assert_eq!(ids(Filter::new().eq("_id", a.to_string())), None);
        assert_eq!(ids(Filter::new().ne("_id", a)), None);
        assert_eq!(ids(Filter::new().lt("_id", a)), None);
        assert_eq!(ids(Filter::new().eq("id", a)), None);
        assert_eq!(ids(Filter::new().eq("ref._id", a)), None);
        assert_eq!(ids(Filter::new().and(!id(a))), None);
    }

    /// SPEC §63: `id` is `eq("_id", id)`, on a `Filter` and a
    /// `Condition` alike, so it names its documents as that does.
    #[test]
    fn id_is_eq_on_the_id_field() {
        let (a, b) = (DocId([1; 16]), DocId([2; 16]));
        let ids = |f: Filter| f.ids();
        assert_eq!(ids(Filter::new().id(a)), Some(vec![a]));
        assert_eq!(ids(Filter::new().eq("x", 1).id(b)), Some(vec![b]));
        let or = Filter::new().any_of([b, a].map(Condition::id));
        assert_eq!(ids(or), Some(vec![a, b]));

        let item = doc(&[("_id", Document::Id(a)), ("id", Document::Id(b))]);
        assert!(Filter::new().id(a).matches(&item));
        assert!(!Filter::new().id(b).matches(&item), "not the `id` field");
        assert!(Condition::id(a).matches(&item));
        assert!(!Condition::id(b).matches(&item));
    }

    /// SPEC §59: ids compare with ids, by their bytes, so a filter on
    /// `_id` or on a reference finds it; with anything else, not at all.
    #[test]
    fn ids_compare_with_ids() {
        use crate::document::DocId;
        let (a, b) = (DocId([1; 16]), DocId([2; 16]));
        let item = doc(&[("_id", Document::Id(a)), ("owner", Document::Id(b))]);
        assert!(Filter::new().eq("_id", a).matches(&item));
        assert!(Filter::new().eq("owner", b).matches(&item));
        assert!(!Filter::new().eq("owner", a).matches(&item));
        assert!(Filter::new().ne("owner", a).matches(&item));
        assert!(Filter::new().lt("_id", b).matches(&item));
        assert!(
            !Filter::new().eq("owner", b.to_string()).matches(&item),
            "a string isn't an id"
        );
    }

    fn doc(pairs: &[(&str, Document)]) -> Document {
        let mut map = Map::new();
        for (k, v) in pairs {
            map.insert(k.to_string(), v.clone());
        }
        Document::Object(map)
    }

    /// `{tags: [1, "a", [2]], items: [{n: 1}, {m: 2}, 3], s: "x",
    /// o: {t: [5]}}` — arrays of mixed things, objects missing the
    /// field, a scalar where an array might be.
    fn with_arrays() -> Document {
        let array = |values: Vec<Document>| Document::Array(values);
        doc(&[
            (
                "tags",
                array(vec![
                    Document::Int(1),
                    "a".into(),
                    array(vec![Document::Int(2)]),
                ]),
            ),
            (
                "items",
                array(vec![
                    doc(&[("n", Document::Int(1))]),
                    doc(&[("m", Document::Int(2))]),
                    Document::Int(3),
                ]),
            ),
            ("s", "x".into()),
            ("o", doc(&[("t", array(vec![Document::Int(5)]))])),
        ])
    }

    /// `Exists` tells a stored null from a missing field, which no
    /// comparison can (SPEC §45.1); through `[*]` some element must
    /// have the field.
    #[test]
    fn exists_is_true_for_a_stored_null_and_false_for_a_missing_field() {
        let null = doc(&[("nick", Document::Null)]);
        let missing = doc(&[("name", "Ada".into())]);
        let set = doc(&[("nick", "Bob".into())]);
        let not_an_object = Document::Int(7);
        let docs = [&null, &missing, &set, &not_an_object];
        let exists = |field: &str| docs.map(|d| Condition::exists(field).matches(d));
        assert_eq!(exists("nick"), [true, false, true, false]);
        let missing_nick = docs.map(|d| Condition::missing("nick").matches(d));
        assert_eq!(missing_nick, [false, true, false, true]);
        // `is_null` can't tell the first two apart.
        let is_null = docs.map(|d| Condition::is_null("nick").matches(d));
        assert_eq!(is_null, [true, true, false, true]);

        let d = with_arrays();
        for (path, there) in [
            ("tags", true),
            ("s", true),
            ("o.t", true),
            ("o.u", false),
            // `s` is a string: nothing below it.
            ("s.x", false),
            ("tags[*]", true),
            ("items[*].n", true),
            ("items[*].m", true),
            ("items[*].k", false),
            // A scalar has no elements; nor does a missing field.
            ("s[*]", false),
            ("nope[*]", false),
        ] {
            assert_eq!(Condition::exists(path).matches(&d), there, "{path}");
        }
        let empty = doc(&[("tags", Document::Array(vec![]))]);
        let nulls = doc(&[("tags", Document::Array(vec![Document::Null]))]);
        assert!(Condition::exists("tags").matches(&empty));
        assert!(!Condition::exists("tags[*]").matches(&empty));
        assert!(Condition::exists("tags[*]").matches(&nulls));
    }

    /// `Size` compares an array's length (SPEC §45.2); without an array
    /// only `Ne` holds, as `Ne` is `Eq` negated.
    #[test]
    fn size_compares_the_length_of_an_array() {
        let array = |n: usize| doc(&[("tags", Document::Array(vec![Document::Int(1); n]))]);
        let docs = [
            array(0),
            array(1),
            array(3),
            doc(&[("tags", "abc".into())]),
            doc(&[("tags", Document::Null)]),
            doc(&[]),
        ];
        let size = |op: Op, n: usize| {
            docs.each_ref()
                .map(|d| Condition::size("tags", op.clone(), n).matches(d))
        };
        assert_eq!(size(Op::Eq, 0), [true, false, false, false, false, false]);
        assert_eq!(size(Op::Eq, 3), [false, false, true, false, false, false]);
        assert_eq!(size(Op::Ne, 0), [false, true, true, true, true, true]);
        assert_eq!(size(Op::Gt, 0), [false, true, true, false, false, false]);
        assert_eq!(size(Op::Gte, 1), [false, true, true, false, false, false]);
        assert_eq!(size(Op::Lt, 2), [true, true, false, false, false, false]);
        assert_eq!(size(Op::Lte, 3), [true, true, true, false, false, false]);
        assert_eq!(size(Op::Contains, 0), [false; 6]);

        // Through `[*]`: any element's array — here `[[2]]`'s one.
        let d = with_arrays();
        assert!(Condition::size("tags[*]", Op::Eq, 1).matches(&d));
        assert!(!Condition::size("tags[*]", Op::Eq, 3).matches(&d));
        assert!(Condition::size("tags", Op::Eq, 3).matches(&d));
        assert!(Condition::size("o.t", Op::Eq, 1).matches(&d));
        assert!(Condition::size("tags[*]", Op::Ne, 3).matches(&d));
        assert!(!Condition::size("tags[*]", Op::Ne, 1).matches(&d));
    }

    #[test]
    fn exists_missing_and_size_build_what_they_say() {
        let f = Filter::new().exists("a").missing("b").size("c", Op::Gte, 2);
        let text = format!("{:?}", f.conditions);
        let want = format!(
            "{:?}",
            vec![
                Condition::Exists { field: "a".into() },
                Condition::Not(Box::new(Condition::Exists { field: "b".into() })),
                Condition::Size {
                    field: "c".into(),
                    op: Op::Gte,
                    size: 2
                },
            ]
        );
        assert_eq!(text, want);
    }

    /// No index tells whether a field is there, or an array's length
    /// (SPEC §45.3): alone they scan, next to an indexed comparison they
    /// check what it finds, and in an OR they make the OR scan.
    #[test]
    fn exists_and_size_never_bound_an_index() {
        let indexes = [index("nick"), index("tags[*]"), sparse(&["team"])];
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].index.name());
        assert_eq!(plan(Filter::new().exists("nick")), None);
        assert_eq!(plan(Filter::new().missing("nick")), None);
        assert_eq!(plan(Filter::new().size("tags[*]", Op::Eq, 0)), None);
        assert_eq!(plan(Filter::new().exists("team")), None);
        assert_eq!(
            plan(Filter::new().missing("nick").eq("tags[*]", "rust")).as_deref(),
            Some("tags[*]")
        );
        let or = Filter::new().any_of([Condition::eq("nick", "a"), Condition::exists("team")]);
        assert_eq!(plan(or), None);
        let order = Filter::new().exists("team").sort_asc("team").limit(5);
        assert!(order.index_order(&indexes).is_none());
    }

    /// `{items: [{sku: "A1", qty: 1}, {sku: "B2", qty: 5}], scores: [72,
    /// 95], rows: [[1, 2], []], orders: [{lines: [{sku: "C3"}]}]}`.
    fn order() -> Document {
        let array = |values: Vec<Document>| Document::Array(values);
        let item = |sku: &str, qty: i64| doc(&[("sku", sku.into()), ("qty", qty.into())]);
        doc(&[
            ("items", array(vec![item("A1", 1), item("B2", 5)])),
            ("scores", array(vec![72.into(), 95.into()])),
            (
                "rows",
                array(vec![array(vec![1.into(), 2.into()]), array(vec![])]),
            ),
            (
                "orders",
                array(vec![doc(&[(
                    "lines",
                    array(vec![doc(&[("sku", "C3".into())])]),
                )])]),
            ),
        ])
    }

    /// One element must meet the whole condition (SPEC §46.1) — where
    /// the same comparisons on `[*]` paths may be met by two elements.
    #[test]
    fn elem_match_needs_one_element_to_meet_it_all() {
        let d = order();
        let holds = |c: Condition| c.matches(&d);
        let sku_qty = |sku: &str, qty: i64| Condition::eq("sku", sku) & Condition::gt("qty", qty);
        // A1 with qty 1, B2 with qty 5: no A1 with qty > 2.
        assert!(holds(
            Condition::eq("items[*].sku", "A1") & Condition::gt("items[*].qty", 2)
        ));
        assert!(!holds(Condition::elem_match("items", sku_qty("A1", 2))));
        assert!(holds(Condition::elem_match("items", sku_qty("B2", 2))));
        // `""` is the element itself: no score in the 80s, one in the 90s.
        let between = |low: i64, high: i64| {
            Condition::elem_match("scores", Condition::gte("", low) & Condition::lt("", high))
        };
        assert!(holds(
            Condition::gte("scores[*]", 80) & Condition::lt("scores[*]", 90)
        ));
        assert!(!holds(between(80, 90)));
        assert!(holds(between(90, 100)));
        // An element that is itself an array: `[*]` and `size` on it.
        assert!(holds(Condition::elem_match(
            "rows",
            Condition::size("", Op::Eq, 0)
        )));
        assert!(holds(Condition::elem_match(
            "rows",
            Condition::eq("[*]", 2)
        )));
        assert!(!holds(Condition::elem_match(
            "rows",
            Condition::eq("[*]", 3)
        )));
        // Nested, and under a path with `[*]`.
        let c3 = || Condition::eq("sku", "C3");
        assert!(holds(Condition::elem_match(
            "orders",
            Condition::elem_match("lines", c3())
        )));
        assert!(holds(Condition::elem_match("orders[*].lines", c3())));
        assert!(!holds(Condition::elem_match("orders", c3())));
        // Not an array, or not there: no element, so it never holds —
        // and its negation always does.
        for field in ["missing", "items[*].sku", "scores[*]"] {
            let any = Condition::elem_match(field, Condition::All(vec![]));
            assert!(!holds(any.clone()), "{field}");
            assert!(holds(!any), "{field}");
        }
        assert!(holds(Condition::elem_match(
            "items",
            Condition::All(vec![])
        )));
        assert!(!holds(Condition::elem_match(
            "items",
            Condition::Any(vec![])
        )));
        // Inside it, `Not` and `Ne` are about the one element.
        assert!(holds(Condition::elem_match(
            "items",
            Condition::ne("sku", "A1")
        )));
        assert!(holds(Condition::elem_match(
            "items",
            !Condition::eq("sku", "A1")
        )));
        assert!(!holds(Condition::ne("items[*].sku", "A1")));
    }

    /// An `ElemMatch` bounds like its condition on `field[*]` paths
    /// (SPEC §46.3): an index on `items[*].sku` finds the items, the
    /// check picks the one that meets it all.
    #[test]
    fn elem_match_bounds_through_the_elements_indexes() {
        let indexes = [
            index("items[*].sku"),
            index("scores[*]"),
            index("rows[*][*]"),
        ];
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].index.name());
        let f = Filter::new().elem_match(
            "items",
            Condition::eq("sku", "A1") & Condition::gt("qty", 2),
        );
        assert_eq!(plan(f.clone()).as_deref(), Some("items[*].sku"));
        let range = &f.index_ranges(&indexes).unwrap()[0].range;
        let key = |v: &str| key::secondary(&v.into(), crate::DocId([0; 16])).unwrap();
        assert!(range.contains(&key("A1")) && !range.contains(&key("B2")));

        let in_80s =
            Filter::new().elem_match("scores", Condition::gte("", 80) & Condition::lt("", 90));
        assert_eq!(plan(in_80s).as_deref(), Some("scores[*]"));
        let row = Filter::new().elem_match("rows", Condition::eq("[*]", 2));
        assert_eq!(plan(row).as_deref(), Some("rows[*][*]"));
        let either = Filter::new().elem_match(
            "items",
            Condition::eq("sku", "A1") | Condition::eq("sku", "B2"),
        );
        assert_eq!(plan(either).as_deref(), Some("items[*].sku"));
        // Nothing to bound by: a `Ne`, a `Not`, a field without an index,
        // an OR with a branch no index bounds.
        for inner in [
            Condition::ne("sku", "A1"),
            !Condition::eq("sku", "A1"),
            Condition::gt("qty", 2),
            Condition::eq("sku", "A1") | Condition::gt("qty", 2),
        ] {
            assert_eq!(
                plan(Filter::new().elem_match("items", inner.clone())),
                None,
                "{inner:?}"
            );
        }
        // The path it's on is part of the rewritten one.
        assert_eq!(
            plan(Filter::new().elem_match("other", Condition::eq("sku", "A1"))),
            None
        );
    }

    #[test]
    fn values_at_walks_into_arrays_only_at_brackets() {
        let d = with_arrays();
        let at = |path| values_at(&d, path).into_iter().cloned().collect::<Vec<_>>();
        let tags = match field_value(&d, "tags") {
            Some(Document::Array(tags)) => tags.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(at("tags[*]"), tags);
        assert_eq!(at("tags[*][*]"), vec![Document::Int(2)]);
        // An element without the field gives null, as a missing field
        // does; so does one that isn't an object.
        assert_eq!(
            at("items[*].n"),
            vec![Document::Int(1), Document::Null, Document::Null]
        );
        assert_eq!(at("o.t[*]"), vec![Document::Int(5)]);
        // No array, no elements: a scalar, a missing field, null.
        assert_eq!(at("s[*]"), vec![]);
        assert_eq!(at("missing[*]"), vec![]);
        assert_eq!(at("items[*].n[*]"), vec![]);
        // Without brackets, exactly one value, as before.
        assert_eq!(at("tags"), vec![field_value(&d, "tags").unwrap().clone()]);
        assert_eq!(at("missing"), vec![Document::Null]);
        assert_eq!(at("items.n"), vec![Document::Null]);
    }

    #[test]
    fn a_comparison_on_elements_holds_if_it_holds_for_any_of_them() {
        let d = with_arrays();
        let holds = |c: Condition| c.matches(&d);
        assert!(holds(Condition::eq("tags[*]", "a")));
        assert!(!holds(Condition::eq("tags[*]", "b")));
        assert!(holds(Condition::gt("tags[*]", 0)));
        assert!(!holds(Condition::lt("tags[*]", 1)));
        assert!(holds(Condition::lte("tags[*]", 1)));
        assert!(holds(Condition::contains("tags[*]", "A")));
        assert!(holds(Condition::eq("items[*].n", 1)));
        assert!(holds(Condition::is_null("items[*].n")));
        assert!(holds(Condition::eq("tags[*][*]", 2)));
        // `Ne` is `Eq` negated: no element equal.
        assert!(!holds(Condition::ne("tags[*]", "a")));
        assert!(holds(Condition::ne("tags[*]", "b")));
        assert!(holds(!Condition::eq("tags[*]", "b")));
        // No elements: nothing holds but `Ne`.
        assert!(!holds(Condition::eq("s[*]", "x")));
        assert!(!holds(Condition::is_null("missing[*]")));
        assert!(holds(Condition::ne("s[*]", "x")));
        // Without brackets an array is one value, equal to no scalar.
        assert!(!holds(Condition::eq("tags", "a")));
        assert!(holds(Condition::ne("tags", "a")));
    }

    /// Several elements may be in range; nothing ties them to one order
    /// of documents, so a multikey index is never read for a sort.
    #[test]
    fn a_multikey_index_is_not_read_in_sort_order() {
        let indexes = [index("tags[*]"), index("age")];
        let sorted = |field: &str| Filter::new().sort_asc(field).limit(5);
        assert!(sorted("tags[*]").index_order(&indexes).is_none());
        assert!(sorted("age").index_order(&indexes).is_some());
        let ranges = Filter::new()
            .eq("tags[*]", "a")
            .index_ranges(&indexes)
            .unwrap();
        assert_eq!(ranges[0].index.name(), "tags[*]");
    }

    fn compound(fields: &[&str]) -> IndexMeta {
        IndexMeta {
            fields: fields.iter().map(|f| f.to_string()).collect(),
            root: 1,
            unique: false,
            sparse: false,
        }
    }

    /// `Eq` on a compound index's first fields, then a range on the next:
    /// the more fields it narrows, the better it is; nothing on its first
    /// field, no use.
    #[test]
    fn a_compound_index_is_chosen_by_how_many_fields_it_narrows() {
        let indexes = [
            index("status"),
            compound(&["status", "created"]),
            compound(&["a", "b", "c"]),
        ];
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].index.name());
        let queued = || Filter::new().eq("status", "Queued");
        // One field narrowed either way: the first among equals.
        assert_eq!(plan(queued()).as_deref(), Some("status"));
        assert_eq!(
            plan(queued().gt("created", 5)).as_deref(),
            Some("(status, created)")
        );
        assert_eq!(
            plan(queued().eq("created", 5)).as_deref(),
            Some("(status, created)")
        );
        // A range on the first field is a range.
        assert_eq!(plan(Filter::new().gt("a", 1)).as_deref(), Some("(a, b, c)"));
        // Not on the first field: nothing to narrow by.
        assert_eq!(plan(Filter::new().eq("b", 1).eq("c", 2)), None);
        // `Eq` on all three: all three narrow.
        assert_eq!(
            plan(Filter::new().eq("c", 3).eq("a", 1).eq("b", 2)).as_deref(),
            Some("(a, b, c)")
        );
        let ranges = Filter::new()
            .eq("a", 1)
            .eq("b", 2)
            .lt("c", 3)
            .index_ranges(&indexes)
            .unwrap();
        let k = |c: i64| key::compound(&[&1.into(), &2.into(), &c.into()], crate::DocId([0; 16]));
        assert!(ranges[0].range.contains(&k(2)) && !ranges[0].range.contains(&k(4)));
    }

    /// Read in sort order when the sort field follows fields an `Eq`
    /// fixes — the more fixed, the better; not when one before it is
    /// free.
    #[test]
    fn a_compound_index_gives_the_order_after_its_fixed_fields() {
        let indexes = [index("created"), compound(&["status", "created"])];
        let order = |f: Filter| f.limit(20).index_order(&indexes).map(|o| o.name());
        let newest = |f: Filter| f.sort_desc("created");
        assert_eq!(
            order(newest(Filter::new().eq("status", "Queued"))).as_deref(),
            Some("(status, created)")
        );
        assert_eq!(order(newest(Filter::new())).as_deref(), Some("created"));
        assert_eq!(
            order(newest(Filter::new().gt("status", "A"))).as_deref(),
            Some("created")
        );
        assert_eq!(
            order(Filter::new().eq("status", "Queued").sort_asc("status")).as_deref(),
            Some("(status, created)")
        );
        // The range read: the fixed value, then the sort field's range.
        let range = newest(Filter::new().eq("status", "Queued").gt("created", 5))
            .limit(20)
            .index_order(&indexes)
            .unwrap()
            .range;
        let k = |status: &str, created: i64| {
            key::compound(&[&status.into(), &created.into()], crate::DocId([0; 16]))
        };
        let range = range.unwrap();
        assert!(range.contains(&k("Queued", 6)) && !range.contains(&k("Queued", 4)));
        assert!(!range.contains(&k("Done", 6)));
    }

    fn sparse(fields: &[&str]) -> IndexMeta {
        IndexMeta {
            sparse: true,
            ..compound(fields)
        }
    }

    /// Values chosen to share encodings, or to sit next to each other in
    /// them (SPEC §104.7): numbers past 2^53 a few apart, `Int` beside
    /// `Float`, strings cut alike in a key of one field and of three,
    /// and what no key encodes.
    fn colliding_values() -> Vec<Document> {
        let big = 1i64 << 60;
        let long = |n: usize, end: &str| Document::from("y".repeat(n) + end);
        let id = |n: u8| Document::Id(crate::DocId([n; 16]));
        let time = |s: i64, n: u32| Document::DateTime(crate::DateTime::from_unix(s, n).unwrap());
        vec![
            Document::Null,
            Document::Bool(false),
            Document::Bool(true),
            Document::Int(-1),
            Document::Int(0),
            Document::Int(3),
            Document::Int(big),
            Document::Int(big + 1),
            Document::Int(big + 300),
            Document::Int(i64::MAX),
            Document::Int(i64::MIN),
            Document::Float(-0.0),
            Document::Float(2.5),
            Document::Float(3.0),
            Document::Float(big as f64),
            Document::Float(f64::INFINITY),
            Document::Float(f64::NEG_INFINITY),
            Document::Float(f64::NAN),
            "".into(),
            "a".into(),
            "a\0".into(),
            "ab".into(),
            "Straße".into(),
            long(340, "1"),
            long(340, "2"),
            long(2000, "1"),
            long(2000, "2"),
            id(1),
            id(2),
            time(0, 0),
            time(0, 1),
            time(-5, 0),
            Document::Array(vec![Document::Int(1)]),
            Document::Object(Map::new()),
            Document::Binary(vec![1]),
        ]
    }

    /// A random condition on `fields` and `_id`, values from `values`.
    fn byte_condition(
        rng: &mut crate::testing::XorShift,
        depth: usize,
        fields: &[&str],
        values: &[Document],
    ) -> Condition {
        if depth == 0 || rng.below(3) == 0 {
            let ops = [
                Op::Eq,
                Op::Ne,
                Op::Lt,
                Op::Lte,
                Op::Gt,
                Op::Gte,
                Op::Contains,
            ];
            let field = match rng.below(fields.len() + 1) {
                i if i < fields.len() => fields[i],
                _ => ID_FIELD,
            };
            let op = ops[rng.below(ops.len())].clone();
            return Condition::compare(field, op, values[rng.below(values.len())].clone());
        }
        let children = (0..rng.below(4))
            .map(|_| byte_condition(rng, depth - 1, fields, values))
            .collect();
        match rng.below(3) {
            0 => Condition::All(children),
            1 => Condition::Any(children),
            _ => !byte_condition(rng, depth - 1, fields, values),
        }
    }

    /// A document with `values` at `fields` (a missing one now and then)
    /// and `id`, and its key in an index on `fields` — `None` where a
    /// one-field index holds no entry for it.
    fn entry_of(
        fields: &[&str],
        values: &[Option<Document>],
        id: crate::DocId,
    ) -> (Document, Option<Vec<u8>>) {
        let mut doc = Map::new();
        doc.insert(ID_FIELD.to_string(), Document::Id(id));
        let mut doc = Document::Object(doc);
        for (field, value) in fields.iter().zip(values) {
            if let (Some(value), Document::Object(map)) = (value, &mut doc) {
                *Path::new(field).slot(map) = value.clone();
            }
        }
        let at: Vec<&Document> = fields.iter().map(|f| value_or_null(&doc, f)).collect();
        let key = match at.len() {
            0 => Some(key::primary(id)),
            1 => key::secondary(at[0], id),
            _ => Some(key::compound(&at, id)),
        };
        (doc, key)
    }

    /// SPEC §104.1, §104.2: where an entry's bytes decide a filter, they
    /// decide it as its document does — on the primary index, one field,
    /// a nested one, and compound keys of two and three, whose strings
    /// are cut shorter; and they decide most of the time.
    #[test]
    fn entries_decide_on_their_bytes_as_their_documents_do() {
        let mut rng = crate::testing::XorShift(0x0104_B17E_0104_B17E);
        let values = colliding_values();
        let indexes: [&[&str]; 5] = [&[], &["a"], &["o.p"], &["a", "b"], &["a", "o.p", "c"]];
        let (mut decided, mut asked) = (0, 0);
        for fields in indexes {
            let names: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            for _ in 0..1500 {
                let conditions = (0..1 + rng.below(2))
                    .map(|_| byte_condition(&mut rng, 3, fields, &values))
                    .collect();
                let filter = Filter {
                    conditions,
                    ..Filter::default()
                };
                let on_bytes = EntryMatcher::new(&filter, &names);
                let matcher = filter.matcher();
                for _ in 0..8 {
                    let at: Vec<Option<Document>> = (0..fields.len())
                        .map(|_| match rng.below(10) {
                            0 => None,
                            _ => Some(values[rng.below(values.len())].clone()),
                        })
                        .collect();
                    let id = crate::DocId([[1, 2, 7][rng.below(3)]; 16]);
                    let (doc, Some(key)) = entry_of(fields, &at, id) else {
                        continue;
                    };
                    asked += 1;
                    if let Some(answer) = on_bytes.matches(&key) {
                        decided += 1;
                        assert_eq!(answer, matcher.matches(&doc), "{filter:?} on {doc}");
                    }
                }
            }
        }
        assert!(decided > asked * 3 / 4, "{decided} of {asked}");
    }

    /// SPEC §104.4: for a filter that accepts one interval of keys, every
    /// key between two that match on their bytes matches — and only such
    /// filters count as one.
    #[test]
    fn keys_between_two_matching_ends_of_an_interval_match() {
        let mut rng = crate::testing::XorShift(0x0104_1A7E_0104_1A7E);
        let encoded: Vec<Document> = colliding_values()
            .into_iter()
            .filter(|v| key::encode_value(v).is_some())
            .collect();
        // One type at a time, so that ranges hold, and all of them.
        let kinds: [fn(&Document) -> bool; 3] = [
            |v| matches!(v, Document::Int(_) | Document::Float(_)),
            |v| matches!(v, Document::String(_)),
            |_| true,
        ];
        let mut intervals = 0;
        for (kind, fields) in (kinds.iter()).flat_map(|k| [(k, &["a"][..]), (k, &["a", "b"])]) {
            let values: Vec<Document> = encoded.iter().filter(|v| kind(v)).cloned().collect();
            let close =
                |rng: &mut crate::testing::XorShift| values[rng.below(values.len())].clone();
            let names: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            let mut entries: Vec<(Vec<u8>, Document)> = (0..400)
                .filter_map(|i| {
                    let at = fields
                        .iter()
                        .map(|_| Some(close(&mut rng)))
                        .collect::<Vec<_>>();
                    let (doc, key) = entry_of(fields, &at, crate::DocId([i as u8; 16]));
                    Some((key?, doc))
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for _ in 0..600 {
                let range = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
                let kind = rng.below(7);
                let mut on = |field: &str, ops: &[Op]| {
                    Condition::compare(field, ops[rng.below(ops.len())].clone(), close(&mut rng))
                };
                let (conditions, interval) = match kind {
                    0 => (vec![on("a", &range), on("a", &range)], true),
                    1 if fields.len() == 2 => (vec![on("b", &range), on("a", &[Op::Eq])], true),
                    2 => (
                        vec![Condition::All(vec![on("a", &range), on("a", &range)])],
                        true,
                    ),
                    3 => (vec![on("a", &range), on("a", &[Op::Ne])], false),
                    4 => (
                        vec![Condition::Any(vec![on("a", &range), on("a", &range)])],
                        false,
                    ),
                    5 => (vec![!on("a", &range)], false),
                    _ if fields.len() == 2 => (vec![on("b", &range), on("a", &range[1..])], false),
                    _ => (
                        vec![
                            on("a", &range),
                            Condition::gt(ID_FIELD, crate::DocId([9; 16])),
                        ],
                        false,
                    ),
                };
                let filter = Filter {
                    conditions,
                    ..Filter::default()
                };
                let on_bytes = EntryMatcher::new(&filter, &names);
                let matcher = filter.matcher();
                assert_eq!(on_bytes.interval, interval, "{filter:?}");
                if !on_bytes.interval {
                    continue;
                }
                let ends: Vec<usize> = (0..entries.len())
                    .filter(|&i| on_bytes.matches(&entries[i].0) == Some(true))
                    .collect();
                if let (Some(&first), Some(&last)) = (ends.first(), ends.last()) {
                    intervals += 1;
                    for (key, doc) in &entries[first..=last] {
                        assert!(matcher.matches(doc), "{filter:?}: {doc} in {key:?}");
                    }
                }
            }
        }
        assert!(intervals > 300, "{intervals}");
    }

    /// SPEC §103.2: a sort by `_id` after `Eq`s walks the range they find
    /// the documents by, where they fix every field of its index — and
    /// only that range, the one `index_ranges` reads.
    #[test]
    fn an_eq_fixing_its_index_whole_is_walked_by_id() {
        let indexes = [
            index("tenant"),
            index("status"),
            compound(&["status", "created"]),
            compound(&["a", "b"]),
            index("tags[*]"),
            sparse(&["s"]),
        ];
        let walked = |f: Filter, order: SortOrder| {
            let f = match order {
                SortOrder::Asc => f.sort_asc("_id"),
                SortOrder::Desc => f.sort_desc("_id"),
            };
            f.then_asc("tenant").limit(20).index_order(&indexes)
        };
        for order in [SortOrder::Asc, SortOrder::Desc] {
            let name = |f: Filter| walked(f, order).map(|o| (o.name(), o.fixed, o.served));
            let one = name(Filter::new().eq("tenant", "x"));
            assert_eq!(one, Some(("tenant".to_string(), 1, 2)));
            let o = walked(Filter::new().eq("tenant", "x"), order).unwrap();
            assert!(o.by_id && o.index.is_some());
            // Two `Eq`s on indexed fields: the range the range read takes.
            let both = Filter::new().eq("status", "y").eq("tenant", "x");
            let read = both.index_ranges(&indexes).unwrap()[0].index.name();
            assert_eq!(name(both).map(|n| n.0), Some(read));
            // A compound index fixed whole is walked; fixed in part, not.
            let ab = name(Filter::new().eq("b", 2).eq("a", 1));
            assert_eq!(ab, Some(("(a, b)".to_string(), 2, 2)));
            assert_eq!(name(Filter::new().eq("a", 1)), None);
            assert_eq!(name(Filter::new().eq("a", 1).gt("b", 1)), None);
            // `(status, created)` beats `status` in the range read, and
            // isn't fixed whole.
            assert_eq!(name(Filter::new().eq("status", "y").gt("created", 1)), None);
            // A multikey path, by an `Eq` on it.
            let tags = name(Filter::new().eq("tags[*]", "t"));
            assert_eq!(tags, Some(("tags[*]".to_string(), 1, 2)));
            // An OR: several ranges, not walked.
            let either = [Condition::eq("tenant", "x"), Condition::eq("tenant", "z")];
            assert_eq!(name(Filter::new().any_of(either)), None);
            // A sparse index lacks the nulls an `Eq` on null finds: it
            // finds nothing by value, so the primary index is walked.
            let null = name(Filter::new().eq("s", Document::Null));
            assert_eq!(null, Some(("_id".to_string(), 0, 2)));
            assert_eq!(
                name(Filter::new().eq("s", 1)),
                Some(("s".to_string(), 1, 2))
            );
            // Without an `Eq` on an indexed field: the primary index.
            let primary = walked(Filter::new().gt("tenant", "x"), order).unwrap();
            assert!(primary.by_id && primary.index.is_none());
        }
        // Without a limit, or with ids: no walk.
        let unlimited = Filter::new().eq("tenant", "x").sort_asc("_id");
        assert!(unlimited.index_order(&indexes).is_none());
        let id = crate::DocId([7; 16]);
        let named = Filter::new()
            .eq("tenant", "x")
            .eq("_id", id)
            .sort_asc("_id");
        assert!(named.limit(20).index_order(&indexes).is_none());
        // A sort on another field than `_id` walks no range by id.
        let other = Filter::new()
            .eq("status", "y")
            .sort_asc("created")
            .limit(20);
        assert!(!other.index_order(&indexes).unwrap().by_id);
    }

    /// SPEC §103.3: `_id` comparisons with an id narrow the walk to the
    /// ids they hold, under the fixed values: exactly, as on the primary
    /// index; `ne`, and a comparison with what isn't an id, don't.
    #[test]
    fn id_comparisons_narrow_a_walk_under_its_fixed_values() {
        let indexes = [index("tenant"), compound(&["a", "b"])];
        let id = |n: u8| crate::DocId([n; 16]);
        let k = |tenant: &str, n: u8| key::secondary(&tenant.into(), id(n)).unwrap();
        let range = |f: Filter| {
            let order = f.eq("tenant", "x").sort_desc("_id").limit(20);
            order.index_order(&indexes).unwrap().range.unwrap()
        };
        let holds = |range: &KeyRange| (1..=9).filter(|&n| range.contains(&k("x", n))).collect();
        let held: Vec<u8> = holds(&range(Filter::new()));
        assert_eq!(held, (1..=9).collect::<Vec<_>>());
        let cases: [(Filter, Vec<u8>); 7] = [
            (Filter::new().gt("_id", id(4)), vec![5, 6, 7, 8, 9]),
            (Filter::new().gte("_id", id(4)), vec![4, 5, 6, 7, 8, 9]),
            (Filter::new().lt("_id", id(4)), vec![1, 2, 3]),
            (Filter::new().lte("_id", id(4)), vec![1, 2, 3, 4]),
            (
                Filter::new().gt("_id", id(2)).lt("_id", id(6)),
                vec![3, 4, 5],
            ),
            (Filter::new().ne("_id", id(4)), (1..=9).collect()),
            (Filter::new().gt("_id", "4"), (1..=9).collect()),
        ];
        for (filter, expected) in cases {
            let range = range(filter);
            assert_eq!(holds(&range), expected);
            // Never another tenant's ids.
            assert!(
                ["w", "xx", "y", ""]
                    .iter()
                    .all(|t| (0..=255).all(|n| !range.contains(&k(t, n))))
            );
        }
        // The walk reports the `Eq` and the comparisons it's bounded by.
        let order = Filter::new()
            .gt("_id", id(4))
            .eq("tenant", "x")
            .sort_asc("_id")
            .limit(20);
        let by = order.index_order(&indexes).unwrap().by;
        assert_eq!(
            by,
            vec![Condition::eq("tenant", "x"), Condition::gt("_id", id(4))]
        );
        // On a compound index, under both fixed values.
        let order = Filter::new()
            .eq("a", 1)
            .eq("b", 2)
            .lt("_id", id(4))
            .sort_asc("_id");
        let range = order
            .limit(20)
            .index_order(&indexes)
            .unwrap()
            .range
            .unwrap();
        let k = |a: i64, b: i64, n: u8| key::compound(&[&a.into(), &b.into()], id(n));
        assert!(range.contains(&k(1, 2, 3)) && !range.contains(&k(1, 2, 4)));
        assert!(!range.contains(&k(1, 3, 3)) && !range.contains(&k(2, 2, 3)));
    }

    /// SPEC §100.1: a read covers a filter whose every path is one of its
    /// index's fields or `_id`, in comparisons of every kind, nested any
    /// way — never `Exists`, `Size` or `ElemMatch`, a field it lacks, or a
    /// multikey index; and its sort keys, when asked, the same way.
    #[test]
    fn a_read_covers_only_what_its_entries_hold() {
        let single = compound(&["a"]);
        let nested = compound(&["o.p"]);
        let two = sparse(&["a", "b"]);
        let multi = compound(&["tags[*]"]);
        let covers =
            |f: &Filter, index: Option<&IndexMeta>| Covering::new(f, index, false).is_some();
        let id = DocId([3; 16]);
        let a = |op: Op| Condition::compare("a", op, 1);
        // Every op, also `Ne` and `Contains`, nested every way.
        for op in [
            Op::Eq,
            Op::Ne,
            Op::Lt,
            Op::Lte,
            Op::Gt,
            Op::Gte,
            Op::Contains,
        ] {
            let f = Filter::new().and(!(a(op.clone()) | Condition::id(id)) & a(Op::Ne));
            assert!(covers(&f, Some(&single)), "{op:?}");
            assert!(covers(&f, Some(&two)), "{op:?}");
            assert!(!covers(&f, Some(&nested)), "{op:?}");
            assert!(!covers(&f, None), "{op:?}");
        }
        assert_eq!(
            Filter::new().eq("b", 1).covered_by(&two.fields, false),
            Some(vec![false, true])
        );
        // Nothing named: any index, the primary one too.
        for index in [None, Some(&single), Some(&two)] {
            assert!(covers(&Filter::new(), index));
            assert!(covers(&Filter::new().gt("_id", id), index));
        }
        // A path is a field of the index as it is: not part of one, nor
        // what one is part of.
        assert!(covers(&Filter::new().eq("o.p", 1), Some(&nested)));
        assert!(!covers(&Filter::new().eq("o", 1), Some(&nested)));
        assert!(!covers(&Filter::new().eq("o.p.q", 1), Some(&nested)));
        // Not a field it lacks, anywhere in the filter.
        let other = Filter::new().any_of([a(Op::Eq), Condition::eq("c", 1)]);
        assert!(!covers(&other, Some(&two)));
        assert!(!covers(
            &Filter::new().and(!Condition::eq("c", 1)),
            Some(&two)
        ));
        // Not what tells missing from null, or looks into arrays.
        for shape in [
            Condition::exists("a"),
            Condition::missing("a"),
            Condition::size("a", Op::Eq, 0),
            Condition::elem_match("a", Condition::eq("", 1)),
        ] {
            assert!(
                !covers(&Filter::new().and(shape.clone()), Some(&single)),
                "{shape:?}"
            );
            let nested_in = Filter::new().and(Condition::any([a(Op::Eq), shape.clone()]));
            assert!(!covers(&nested_in, Some(&single)), "{shape:?}");
        }
        // Never a multikey index, whatever the filter.
        assert!(!covers(&Filter::new().eq("tags[*]", 1), Some(&multi)));
        assert!(!covers(&Filter::new(), Some(&multi)));
        // Sort keys count only when asked: the index's fields and `_id`.
        let sorted = Filter::new().eq("a", 1).sort_desc("b").then_asc("_id");
        assert!(Covering::new(&sorted, Some(&two), true).is_some());
        assert!(Covering::new(&sorted, Some(&single), false).is_some());
        assert!(Covering::new(&sorted, Some(&single), true).is_none());
        assert_eq!(sorted.covered_by(&two.fields, true), Some(vec![true, true]));
    }

    /// An entry gives the document of the named fields, at their paths,
    /// and its `_id` — or nothing, where a named value isn't exact or two
    /// fields' paths collide.
    #[test]
    fn an_entry_gives_the_named_fields_and_its_id() {
        let id = DocId([5; 16]);
        let two = compound(&["a", "o.p"]);
        let key = crate::index::key::compound(&[&Document::Int(4), &Document::from("x")], id);
        let doc = |f: &Filter| Covering::new(f, Some(&two), false).unwrap().entry(&key);
        let both = Filter::new()
            .eq("a", 1)
            .eq("o.p", "y")
            .gt("_id", DocId([0; 16]));
        let Some(Document::Object(mut got)) = doc(&both) else {
            panic!("no document");
        };
        assert_eq!(got.remove("_id"), Some(Document::Id(id)));
        assert_eq!(got.len(), 2);
        assert_eq!(
            value_or_null(&Document::Object(got.clone()), "o.p"),
            &Document::from("x")
        );
        assert!(equal(
            value_or_null(&Document::Object(got), "a"),
            &Document::Int(4)
        ));
        // Only the named ones: an inexact value no filter names is no
        // obstacle.
        let long = Document::from("z".repeat(2000));
        let key = crate::index::key::compound(&[&Document::Int(4), &long], id);
        let only_a = Filter::new().eq("a", 4);
        let entry = Covering::new(&only_a, Some(&two), false)
            .unwrap()
            .entry(&key);
        assert!(entry.is_some_and(|doc| only_a.matches(&doc)));
        let on_p = Filter::new().eq("o.p", "z");
        assert_eq!(
            Covering::new(&on_p, Some(&two), false).unwrap().entry(&key),
            None
        );
        // `a` and `a.b` in one index: one document can't hold both as
        // the key has them, so a filter naming both isn't covered; one
        // naming either is.
        let inside = compound(&["a", "a.b"]);
        let both = Filter::new().eq("a", 1).eq("a.b", Document::Null);
        assert!(Covering::new(&both, Some(&inside), false).is_none());
        assert!(Covering::new(&Filter::new().eq("a.b", 1), Some(&inside), false).is_some());
        let ab = compound(&["ab", "a"]);
        assert!(Covering::new(&Filter::new().eq("ab", 1).eq("a", 1), Some(&ab), false).is_some());
        // The primary index: the id alone, and only if named.
        let primary = Covering::new(&Filter::new().gt("_id", id), None, false).unwrap();
        let mut only_id = Map::new();
        only_id.insert("_id".to_string(), Document::Id(id));
        assert_eq!(primary.entry(&id.0), Some(Document::Object(only_id)));
        let nothing = Covering::new(&Filter::new(), None, false).unwrap();
        let empty = Document::Object(Map::new());
        assert_eq!(nothing.entry(&id.0), Some(empty));
        // Deciding entry after entry in one scratch document: each
        // entry's own values, whatever the one before left.
        let f = Filter::new().eq("a", 4).eq("o.p", "x");
        let covering = Covering::new(&f, Some(&two), false).unwrap();
        let mut scratch = Document::Null;
        for (a, p, matches) in [
            (4, "x", true),
            (4, "y", false),
            (5, "x", false),
            (4, "x", true),
        ] {
            let key = crate::index::key::compound(&[&Document::Int(a), &Document::from(p)], id);
            assert_eq!(
                covering.decide(&key, &f.matcher(), &mut scratch),
                Some(matches)
            );
        }
        assert_eq!(
            covering.decide(&key, &f.matcher(), &mut scratch),
            None,
            "the cut string"
        );
    }

    /// A sparse index has no entry for a null (SPEC §44): it bounds only
    /// by comparisons null fails, and is read in order only where the
    /// filter keeps nulls out.
    #[test]
    fn a_sparse_index_is_only_used_where_nulls_are_ruled_out() {
        let indexes = [sparse(&["nick"]), sparse(&["tags[*]"])];
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].index.name());
        assert_eq!(
            plan(Filter::new().eq("nick", "ada")).as_deref(),
            Some("nick")
        );
        assert_eq!(plan(Filter::new().gt("nick", "m")).as_deref(), Some("nick"));
        assert_eq!(plan(Filter::new().is_null("nick")), None);
        assert_eq!(plan(Filter::new().lte("nick", Document::Null)), None);
        assert_eq!(plan(Filter::new().gte("nick", Document::Null)), None);
        // An element that is null meets the first, one that is 5 the
        // second: the range read is the second's.
        let both = Filter::new().is_null("tags[*]").gt("tags[*]", 3);
        let ranges = both.index_ranges(&indexes).unwrap();
        let five = key::secondary(&Document::Int(5), crate::DocId([0; 16])).unwrap();
        assert!(ranges[0].range.contains(&five), "{:?}", ranges[0].range);

        let order = |f: Filter| f.limit(5).index_order(&indexes).map(|o| o.name());
        assert_eq!(order(Filter::new().sort_asc("nick")), None);
        assert_eq!(
            order(Filter::new().is_not_null("nick").sort_asc("nick")).as_deref(),
            Some("nick")
        );
        assert_eq!(order(Filter::new().is_null("nick").sort_asc("nick")), None);
    }

    /// A sparse compound index lacks the documents null in all of its
    /// fields: a comparison that null fails on any of them keeps those
    /// out.
    #[test]
    fn a_sparse_compound_index_needs_one_field_that_is_not_null() {
        let indexes = [sparse(&["a", "b"])];
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].index.name());
        assert_eq!(plan(Filter::new().eq("a", 1)).as_deref(), Some("(a, b)"));
        assert_eq!(plan(Filter::new().is_null("a")), None);
        assert_eq!(
            plan(Filter::new().is_null("a").eq("b", 2)).as_deref(),
            Some("(a, b)")
        );
        assert_eq!(
            plan(Filter::new().is_null("a").is_not_null("b")).as_deref(),
            Some("(a, b)")
        );
        let order = |f: Filter| f.limit(5).index_order(&indexes).map(|o| o.name());
        assert_eq!(order(Filter::new().is_null("a").sort_asc("b")), None);
        assert_eq!(
            order(Filter::new().is_null("a").gt("b", 0).sort_asc("b")).as_deref(),
            Some("(a, b)")
        );
    }

    /// With several sort keys (SPEC §47): an index serves as many as its
    /// next fields are, in the first key's direction; one that fixes
    /// more fields wins, then one that serves more keys.
    #[test]
    fn an_index_serves_the_sort_keys_its_fields_follow() {
        let indexes = [
            index("created"),
            compound(&["status", "created"]),
            compound(&["status", "created", "tenant"]),
        ];
        let read = |f: Filter| {
            f.limit(10)
                .index_order(&indexes)
                .map(|o| (o.name(), o.fixed, o.served))
        };
        let status_created = || Filter::new().sort_asc("status").then_asc("created");
        assert_eq!(
            read(status_created()),
            Some(("(status, created)".into(), 0, 2))
        );
        assert_eq!(
            read(status_created().then_asc("tenant")),
            Some(("(status, created, tenant)".into(), 0, 3))
        );
        // The other direction for a later key: the index serves the keys
        // before it.
        assert_eq!(
            read(Filter::new().sort_asc("status").then_desc("created")),
            Some(("(status, created)".into(), 0, 1))
        );
        assert_eq!(
            read(Filter::new().sort_desc("status").then_desc("created")),
            Some(("(status, created)".into(), 0, 2))
        );
        // Not the index's next field: only the first key.
        assert_eq!(
            read(Filter::new().sort_asc("created").then_asc("status")),
            Some(("created".into(), 0, 1))
        );
        // Fixed fields beat served keys.
        assert_eq!(
            read(
                Filter::new()
                    .eq("status", "Queued")
                    .sort_asc("created")
                    .then_asc("tenant")
            ),
            Some(("(status, created, tenant)".into(), 1, 2))
        );
        assert_eq!(
            read(Filter::new().eq("status", "Queued").sort_asc("created")),
            Some(("(status, created)".into(), 1, 1))
        );
        // Even against an index that would serve more keys.
        let fixing_or_serving = [
            compound(&["created", "tenant"]),
            compound(&["status", "created"]),
        ];
        let f = Filter::new()
            .eq("status", "Queued")
            .sort_asc("created")
            .then_asc("tenant")
            .limit(10);
        let o = f.index_order(&fixing_or_serving).unwrap();
        assert_eq!(
            (o.name(), o.fixed, o.served),
            ("(status, created)".into(), 1, 1)
        );
        // `then_*` adds; `sort_*` starts over.
        let f = status_created().sort_desc("tenant");
        assert_eq!(f.sort.len(), 1);
        assert_eq!(f.sort[0].field, "tenant");
    }

    #[test]
    fn several_sort_keys_sort_by_the_first_then_the_next() {
        let d = |a: i64, b: &str| doc(&[("a", a.into()), ("b", b.into())]);
        let docs = vec![d(2, "x"), d(1, "y"), d(2, "a"), d(1, "b"), d(1, "y")];
        let sorted = |f: Filter| {
            f.apply(docs.clone())
                .iter()
                .map(|doc| format!("{:?}{:?}", value_or_null(doc, "a"), value_or_null(doc, "b")))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            sorted(Filter::new().sort_asc("a").then_desc("b")),
            r#"Int(1)String("y") Int(1)String("y") Int(1)String("b") Int(2)String("x") Int(2)String("a")"#
        );
        assert_eq!(
            sorted(Filter::new().sort_desc("b").then_asc("a")),
            r#"Int(1)String("y") Int(1)String("y") Int(2)String("x") Int(1)String("b") Int(2)String("a")"#
        );
        let literal = Filter {
            sort: vec![Sort::desc("a"), Sort::asc("b")],
            ..Filter::default()
        };
        assert_eq!(
            sorted(literal),
            r#"Int(2)String("a") Int(2)String("x") Int(1)String("b") Int(1)String("y") Int(1)String("y")"#
        );
    }

    /// `tags[*] > 5 AND tags[*] < 3` holds for `[1, 10]`: one element is
    /// above 5, another below 3. Intersected, the two ranges would be
    /// empty; one of them alone holds the document.
    #[test]
    fn ranges_on_elements_are_not_intersected() {
        let indexes = [index("tags[*]")];
        let d = doc(&[("tags", Document::Array(vec![1.into(), 10.into()]))]);
        let filter = Filter::new().gt("tags[*]", 5).lt("tags[*]", 3);
        assert!(filter.matches(&d));
        let ranges = filter.index_ranges(&indexes).unwrap();
        assert_eq!(ranges.len(), 1);
        let key = key::secondary(&Document::Int(10), crate::DocId([0; 16])).unwrap();
        assert!(ranges[0].range.contains(&key), "{:?}", ranges[0].range);

        // An `Eq` among them is the one that bounds.
        let filter = Filter::new().gt("tags[*]", 5).eq("tags[*]", 1);
        let ranges = filter.index_ranges(&indexes).unwrap();
        let key = key::secondary(&Document::Int(1), crate::DocId([0; 16])).unwrap();
        assert!(ranges[0].range.contains(&key));
    }

    #[test]
    fn and_of_comparisons() {
        let filter = Filter {
            conditions: vec![
                Condition::Compare {
                    field: "age".into(),
                    op: Op::Gte,
                    value: Document::Int(18),
                },
                Condition::Compare {
                    field: "name".into(),
                    op: Op::Eq,
                    value: Document::String("Udo".into()),
                },
            ],
            ..Default::default()
        };

        let matching = doc(&[
            ("age", Document::Int(30)),
            ("name", Document::String("Udo".into())),
        ]);
        let too_young = doc(&[
            ("age", Document::Int(10)),
            ("name", Document::String("Udo".into())),
        ]);

        assert!(filter.matches(&matching));
        assert!(!filter.matches(&too_young));
    }

    fn contains(field: &str, needle: &str) -> Filter {
        Filter {
            conditions: vec![Condition::Compare {
                field: field.into(),
                op: Op::Contains,
                value: Document::String(needle.into()),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn contains_matches_substrings_ignoring_case() {
        let title = doc(&[("title", Document::String("Senior Rust Developer".into()))]);

        assert!(contains("title", "rust").matches(&title));
        assert!(contains("title", "RUST DEV").matches(&title));
        assert!(contains("title", "senior rust developer").matches(&title));
        assert!(!contains("title", "python").matches(&title));
        assert!(!contains("title", "rustdev").matches(&title));
    }

    #[test]
    fn contains_folds_umlauts_and_sharp_s() {
        let street = doc(&[("street", Document::String("Müllerstraße 5".into()))]);

        assert!(contains("street", "MÜLLER").matches(&street));
        assert!(contains("street", "strasse").matches(&street));
        assert!(contains("street", "STRASSE").matches(&street));
        assert!(contains("street", "straße").matches(&street));
        assert!(
            !contains("street", "muller").matches(&street),
            "ü is not u — folding case, not stripping accents"
        );
    }

    #[test]
    fn contains_with_an_empty_needle_matches_every_string() {
        let filter = contains("title", "");
        assert!(filter.matches(&doc(&[("title", Document::String("anything".into()))])));
        assert!(filter.matches(&doc(&[("title", Document::String("".into()))])));
    }

    #[test]
    fn contains_only_matches_strings() {
        let filter = contains("n", "1");
        assert!(!filter.matches(&doc(&[("n", Document::Int(123))])));
        assert!(!filter.matches(&doc(&[("other", Document::String("1".into()))])));

        let non_string_needle = Filter {
            conditions: vec![Condition::Compare {
                field: "n".into(),
                op: Op::Contains,
                value: Document::Int(1),
            }],
            ..Default::default()
        };
        assert!(!non_string_needle.matches(&doc(&[("n", Document::String("1".into()))])));
    }

    #[test]
    fn the_builder_builds_what_the_literal_spells_out() {
        let cond = |field: &str, op, value| Condition::Compare {
            field: field.into(),
            op,
            value,
        };
        let built = Filter::new()
            .eq("a", 1)
            .ne("b", "x")
            .lt("c", 1.5)
            .lte("d", true)
            .gt("e", 2u8)
            .gte("f.g", -3i64)
            .contains("h", "Rust")
            .is_null("i")
            .is_not_null("j")
            .condition("k", Op::Eq, Document::Array(vec![]))
            .sort_asc("x")
            .sort_desc("y") // replaces the first
            .limit(10)
            .limit(5) // replaces the first
            .skip(3)
            .skip(1); // replaces the first
        let spelled_out = Filter {
            conditions: vec![
                cond("a", Op::Eq, Document::Int(1)),
                cond("b", Op::Ne, Document::String("x".into())),
                cond("c", Op::Lt, Document::Float(1.5)),
                cond("d", Op::Lte, Document::Bool(true)),
                cond("e", Op::Gt, Document::Int(2)),
                cond("f.g", Op::Gte, Document::Int(-3)),
                cond("h", Op::Contains, Document::String("Rust".into())),
                cond("i", Op::Eq, Document::Null),
                cond("j", Op::Ne, Document::Null),
                cond("k", Op::Eq, Document::Array(vec![])),
            ],
            sort: vec![Sort {
                field: "y".into(),
                order: SortOrder::Desc,
            }],
            skip: 1,
            limit: Some(5),
            select: None,
            hint: None,
        };
        // `Filter` has no `PartialEq` (a `Float` NaN isn't equal to itself).
        assert_eq!(format!("{built:?}"), format!("{spelled_out:?}"));
        assert_eq!(
            format!("{:?}", Filter::new()),
            format!("{:?}", Filter::default())
        );
    }

    #[test]
    fn or_and_not_nest_as_deep_as_needed() {
        let ada = doc(&[
            ("name", Document::String("Ada".into())),
            ("age", Document::Int(36)),
            ("role", Document::String("admin".into())),
        ]);
        let bob = doc(&[
            ("name", Document::String("Bob".into())),
            ("age", Document::Int(17)),
        ]);
        let who = |c: Condition| {
            [("Ada", &ada), ("Bob", &bob)]
                .into_iter()
                .filter(|(_, d)| c.matches(d))
                .map(|(name, _)| name)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            who(Condition::any([
                Condition::eq("name", "Ada"),
                Condition::lt("age", 18)
            ])),
            ["Ada", "Bob"]
        );
        assert_eq!(
            who(Condition::all([
                Condition::eq("name", "Ada"),
                Condition::lt("age", 18)
            ])),
            Vec::<&str>::new()
        );
        assert_eq!(who(!Condition::eq("name", "Ada")), ["Bob"]);
        // NOT of a comparison on a missing field is true (SPEC §32).
        assert_eq!(who(!Condition::eq("role", "admin")), ["Bob"]);
        assert_eq!(who(!Condition::is_null("role")), ["Ada"]);
        // Empty groups: AND of nothing holds, OR of nothing doesn't.
        assert_eq!(who(Condition::all([])), ["Ada", "Bob"]);
        assert_eq!(who(Condition::any([])), Vec::<&str>::new());
        // Nested: (adult AND admin) OR (NOT adult AND name contains "o").
        let nested = Condition::any([
            Condition::all([Condition::gte("age", 18), Condition::eq("role", "admin")]),
            Condition::all([!Condition::gte("age", 18), Condition::contains("name", "O")]),
        ]);
        assert_eq!(who(nested.clone()), ["Ada", "Bob"]);
        assert_eq!(who(!nested), Vec::<&str>::new());

        // Through the builder: each call is ANDed with the rest.
        let filter = Filter::new()
            .any_of([Condition::eq("name", "Ada"), Condition::eq("name", "Bob")])
            .and(!Condition::lt("age", 18));
        assert!(filter.matches(&ada) && !filter.matches(&bob));
        assert_eq!(
            format!("{:?}", filter.conditions),
            format!(
                "{:?}",
                [
                    Condition::Any(vec![
                        Condition::eq("name", "Ada"),
                        Condition::eq("name", "Bob")
                    ]),
                    Condition::Not(Box::new(Condition::lt("age", 18))),
                ]
            )
        );
    }

    #[test]
    fn operators_build_the_same_trees_as_the_functions() {
        let (a, b, c) = (
            || Condition::eq("a", 1),
            || Condition::eq("b", 2),
            || Condition::eq("c", 3),
        );
        let same = |x: Condition, y: Condition| assert_eq!(format!("{x:?}"), format!("{y:?}"));
        // Chains flatten into one group; a group on the right stays one.
        same(a() | b() | c(), Condition::any([a(), b(), c()]));
        same(a() & b() & c(), Condition::all([a(), b(), c()]));
        same(
            a() | (b() | c()),
            Condition::any([a(), Condition::any([b(), c()])]),
        );
        // `&` binds tighter than `|`, as in Rust.
        same(
            a() | b() & c(),
            Condition::any([a(), Condition::all([b(), c()])]),
        );
        same(
            !(a() | b()),
            Condition::Not(Box::new(Condition::any([a(), b()]))),
        );
        same(
            !!a(),
            Condition::Not(Box::new(Condition::Not(Box::new(a())))),
        );
    }

    fn city_is(path: &str, city: &str) -> Filter {
        Filter {
            conditions: vec![Condition::Compare {
                field: path.into(),
                op: Op::Eq,
                value: Document::String(city.into()),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_dotted_path_walks_into_nested_objects() {
        let berlin = || Document::String("Berlin".into());
        let person = doc(&[(
            "address",
            doc(&[("city", berlin()), ("geo", doc(&[("zone", berlin())]))]),
        )]);

        assert!(city_is("address.city", "Berlin").matches(&person));
        assert!(city_is("address.geo.zone", "Berlin").matches(&person));
        assert!(!city_is("address.city", "Paris").matches(&person));
        assert!(!city_is("address.zip", "Berlin").matches(&person));
        assert!(!city_is("address.city.name", "Berlin").matches(&person));
        assert!(
            !city_is("city", "Berlin").matches(&person),
            "no deep search"
        );
    }

    #[test]
    fn a_path_never_enters_arrays_or_reaches_dotted_keys() {
        let berlin = || Document::String("Berlin".into());
        let in_array = doc(&[("address", Document::Array(vec![doc(&[("city", berlin())])]))]);
        let dotted_key = doc(&[("address.city", berlin())]);

        assert!(!city_is("address.city", "Berlin").matches(&in_array));
        assert!(!city_is("address.0.city", "Berlin").matches(&in_array));
        assert!(!city_is("address.city", "Berlin").matches(&dotted_key));
        for bad in ["", ".", "address.", ".address.city", "address..city"] {
            assert!(!city_is(bad, "Berlin").matches(&dotted_key), "{bad:?}");
        }
    }

    #[test]
    fn sort_by_a_nested_field() {
        let at = |n| doc(&[("meta", doc(&[("rank", Document::Int(n))]))]);
        let sorted = Filter {
            sort: vec![Sort {
                field: "meta.rank".into(),
                order: SortOrder::Desc,
            }],
            ..Default::default()
        }
        .apply(vec![at(2), at(3), at(1)]);
        assert_eq!(sorted, vec![at(3), at(2), at(1)]);
    }

    #[test]
    fn a_missing_field_is_null() {
        let when = |op: Op, value: Document| Filter {
            conditions: vec![Condition::Compare {
                field: "nick".into(),
                op,
                value,
            }],
            ..Default::default()
        };
        let null = doc(&[("nick", Document::Null)]);
        let missing = doc(&[("name", Document::String("Ada".into()))]);
        let not_an_object = Document::Int(7);
        let set = doc(&[("nick", Document::String("Bob".into()))]);
        let bob = || Document::String("Bob".into());

        for (filter, matching) in [
            (when(Op::Eq, Document::Null), [true, true, true, false]),
            (when(Op::Ne, Document::Null), [false, false, false, true]),
            (when(Op::Lte, Document::Null), [true, true, true, false]),
            (when(Op::Gte, Document::Null), [true, true, true, false]),
            (when(Op::Lt, Document::Null), [false; 4]),
            (when(Op::Gt, Document::Null), [false; 4]),
            (when(Op::Eq, bob()), [false, false, false, true]),
            (when(Op::Ne, bob()), [true, true, true, false]),
            (when(Op::Gt, Document::Int(0)), [false; 4]),
            (
                when(Op::Contains, Document::String("".into())),
                [false, false, false, true],
            ),
        ] {
            let found = [&null, &missing, &not_an_object, &set].map(|d| filter.matches(d));
            assert_eq!(found, matching, "{:?}", filter.conditions[0]);
        }
        // Through a path too: `address` isn't there, so neither is `city`.
        let on_path = Filter::new().is_null("address.city");
        assert!(on_path.matches(&missing) && on_path.matches(&set));
    }

    #[test]
    fn sort_orders_ascending_and_descending() {
        let docs = vec![
            doc(&[("age", Document::Int(30))]),
            doc(&[("age", Document::Int(10))]),
            doc(&[("age", Document::Int(20))]),
        ];

        let asc = Filter {
            sort: vec![Sort {
                field: "age".into(),
                order: SortOrder::Asc,
            }],
            ..Default::default()
        }
        .apply(docs.clone());
        assert_eq!(
            asc,
            vec![
                doc(&[("age", Document::Int(10))]),
                doc(&[("age", Document::Int(20))]),
                doc(&[("age", Document::Int(30))]),
            ]
        );

        let desc = Filter {
            sort: vec![Sort {
                field: "age".into(),
                order: SortOrder::Desc,
            }],
            ..Default::default()
        }
        .apply(docs);
        assert_eq!(
            desc,
            vec![
                doc(&[("age", Document::Int(30))]),
                doc(&[("age", Document::Int(20))]),
                doc(&[("age", Document::Int(10))]),
            ]
        );
    }

    #[test]
    fn limit_caps_the_result_count() {
        let docs = vec![
            doc(&[("age", Document::Int(1))]),
            doc(&[("age", Document::Int(2))]),
            doc(&[("age", Document::Int(3))]),
        ];

        let filter = Filter {
            limit: Some(2),
            ..Default::default()
        };
        assert_eq!(filter.apply(docs).len(), 2);
    }

    #[test]
    fn most_recent_row_is_sort_desc_plus_limit_one() {
        // The time-series workload's query (SPEC §5.1): ORDER BY tst DESC LIMIT 1.
        let docs = vec![
            doc(&[("tst", Document::Int(100))]),
            doc(&[("tst", Document::Int(300))]),
            doc(&[("tst", Document::Int(200))]),
        ];

        let filter = Filter {
            sort: vec![Sort {
                field: "tst".into(),
                order: SortOrder::Desc,
            }],
            limit: Some(1),
            ..Default::default()
        };

        assert_eq!(
            filter.apply(docs),
            vec![doc(&[("tst", Document::Int(300))])]
        );
    }

    /// One total order over every kind of value (SPEC §34.1), in both
    /// directions: kinds in index order, values nothing orders last, and
    /// equal values in the order they came.
    #[test]
    fn sort_orders_every_kind_of_value_totally() {
        let big = 1i64 << 53;
        let v = |value: Document| doc(&[("v", value), ("tag", Document::Int(0))]);
        let missing = doc(&[("tag", Document::Int(1))]);
        let unordered = [
            v(Document::Array(vec![])),
            v(Document::Float(f64::NAN)),
            v(Document::Binary(vec![1])),
        ];
        // Ascending, as `sort` must return them (the null and the
        // missing field are equal, so they keep their input order).
        let ascending = vec![
            v(Document::Null),
            missing.clone(),
            v(Document::Bool(false)),
            v(Document::Bool(true)),
            v(Document::Float(f64::NEG_INFINITY)),
            v(Document::Int(-3)),
            v(Document::Float(0.5)),
            v(Document::Float(big as f64)),
            v(Document::Int(big + 1)),
            v(Document::Float(1e300)),
            v(Document::String("".into())),
            v(Document::String("B".into())),
            v(Document::String("a".into())),
        ];
        // Compared as text: NaN isn't equal to itself.
        let text = |docs: Vec<Document>| format!("{docs:?}");
        let sorted = |order, docs: Vec<Document>| {
            text(
                Filter {
                    sort: vec![Sort {
                        field: "v".into(),
                        order,
                    }],
                    ..Default::default()
                }
                .apply(docs),
            )
        };

        // Input: everything reversed, with the unordered values mixed in.
        let mut input: Vec<Document> = ascending.iter().rev().cloned().collect();
        input.insert(3, unordered[0].clone());
        input.insert(0, unordered[1].clone());
        input.push(unordered[2].clone());
        let unordered_in_input_order = [&unordered[1], &unordered[0], &unordered[2]];

        let mut expected = ascending.clone();
        // Null and missing are equal: reversed input keeps them reversed.
        expected.swap(0, 1);
        expected.extend(unordered_in_input_order.iter().map(|d| (*d).clone()));
        assert_eq!(sorted(SortOrder::Asc, input.clone()), text(expected));

        // Reversed, except null and missing: equal, so in input order —
        // which the reversed input already gave them.
        let mut expected: Vec<Document> = ascending.iter().rev().cloned().collect();
        expected.extend(unordered_in_input_order.iter().map(|d| (*d).clone()));
        assert_eq!(sorted(SortOrder::Desc, input), text(expected));
    }

    #[test]
    fn ints_and_floats_compare_by_exact_value() {
        use std::cmp::Ordering::*;
        let big = 1i64 << 53;
        for (int, float, expected) in [
            (1, 1.0, Equal),
            (0, -0.0, Equal),
            (1, 1.5, Less),
            (-1, -1.5, Greater),
            (big + 1, big as f64, Greater), // `as f64` would say Equal
            (big, big as f64, Equal),
            (i64::MAX, 9_223_372_036_854_775_808.0, Less),
            (i64::MIN, -9_223_372_036_854_775_808.0, Equal),
            (i64::MIN, -1e19, Greater),
            (0, f64::INFINITY, Less),
            (0, f64::NEG_INFINITY, Greater),
        ] {
            let (i, f) = (Document::Int(int), Document::Float(float));
            assert_eq!(compare(&i, &f), Some(expected), "{int} vs {float}");
            assert_eq!(
                compare(&f, &i),
                Some(expected.reverse()),
                "{float} vs {int}"
            );
        }
        assert_eq!(compare(&Document::Int(0), &Document::Float(f64::NAN)), None);
    }

    #[test]
    fn conditions_sort_and_limit_compose() {
        let docs = vec![
            doc(&[("age", Document::Int(10))]), // filtered out
            doc(&[("age", Document::Int(30))]),
            doc(&[("age", Document::Int(20))]),
            doc(&[("age", Document::Int(40))]),
        ];

        let filter = Filter {
            conditions: vec![Condition::Compare {
                field: "age".into(),
                op: Op::Gte,
                value: Document::Int(18),
            }],
            sort: vec![Sort {
                field: "age".into(),
                order: SortOrder::Asc,
            }],
            skip: 0,
            limit: Some(2),
            select: None,
            hint: None,
        };

        assert_eq!(
            filter.apply(docs.clone()),
            vec![
                doc(&[("age", Document::Int(20))]),
                doc(&[("age", Document::Int(30))]),
            ]
        );
        // SPEC §72: skip after the sort, before the limit.
        let second_page = filter.clone().skip(1);
        assert_eq!(
            second_page.apply(docs.clone()),
            vec![
                doc(&[("age", Document::Int(30))]),
                doc(&[("age", Document::Int(40))]),
            ]
        );
        assert_eq!(filter.clone().skip(2).apply(docs.clone()).len(), 1);
        assert_eq!(filter.clone().skip(4).apply(docs.clone()), vec![]);
        assert_eq!(filter.limit(None).skip(1).apply(docs).len(), 2);
    }

    fn cond(field: &str, op: Op, value: Document) -> Condition {
        Condition::Compare {
            field: field.into(),
            op,
            value,
        }
    }

    fn index(field: &str) -> IndexMeta {
        IndexMeta {
            fields: vec![field.into()],
            root: 1,
            unique: false,
            sparse: false,
        }
    }

    #[test]
    fn index_range_prefers_eq_and_intersects_ranges() {
        let indexes = [index("age"), index("name")];
        let filter = |conditions| Filter {
            conditions,
            ..Default::default()
        };
        let field_of = |f: &Filter| {
            let ranges = f.index_ranges(&indexes)?;
            assert_eq!(ranges.len(), 1, "one range, not a union");
            Some(ranges[0].index.name())
        };

        // No condition on an indexed field, or only unusable ones.
        assert_eq!(
            field_of(&filter(vec![cond("city", Op::Eq, Document::Int(1))])),
            None
        );
        assert_eq!(
            field_of(&filter(vec![
                cond("age", Op::Ne, Document::Int(1)),
                cond("name", Op::Contains, Document::String("a".into())),
                cond("name", Op::Eq, Document::Array(vec![])),
            ])),
            None
        );
        // An Eq wins over an earlier range condition.
        let f = filter(vec![
            cond("age", Op::Gt, Document::Int(1)),
            cond("name", Op::Eq, Document::String("Ada".into())),
        ]);
        assert_eq!(field_of(&f), Some("name".to_string()));
        // Both range conditions on one field narrow the range.
        let f = filter(vec![
            cond("age", Op::Gte, Document::Int(10)),
            cond("age", Op::Lte, Document::Int(20)),
        ]);
        let range = f.index_ranges(&indexes).unwrap().remove(0).range;
        let k = |n| key::secondary(&Document::Int(n), crate::DocId([0; 16])).unwrap();
        assert!(range.contains(&k(15)) && !range.contains(&k(9)) && !range.contains(&k(21)));
    }

    /// Which index ranges a filter with ORs, ANDs and NOTs reads (SPEC
    /// §36.3) — `None` for a scan.
    #[test]
    fn index_ranges_union_the_branches_of_an_or() {
        let indexes = [index("age"), index("name")];
        let fields = |f: Filter| -> Option<Vec<String>> {
            let ranges = f.index_ranges(&indexes)?;
            Some(ranges.iter().map(|r| r.index.name()).collect())
        };
        let ada = || Condition::eq("name", "Ada");
        let young = || Condition::lt("age", 18);
        let unindexed = || Condition::eq("city", "Berlin");

        // An OR whose every branch an index can bound: their union.
        assert_eq!(
            fields(Filter::new().any_of([ada(), young()])),
            Some(vec!["name".into(), "age".into()])
        );
        // A branch in an AND needs just one bounded condition.
        let branch = Condition::all([unindexed(), young()]);
        assert_eq!(
            fields(Filter::new().any_of([ada(), branch])),
            Some(vec!["name".into(), "age".into()])
        );
        // Nested ORs flatten into one union.
        let inner = Condition::any([ada(), Condition::eq("name", "Bob")]);
        assert_eq!(
            fields(Filter::new().any_of([inner, young()])).map(|f| f.len()),
            Some(3)
        );
        // An OR of nothing matches nothing: no range at all.
        assert_eq!(fields(Filter::new().any_of([])), Some(vec![]));

        // One branch no index bounds, or a NOT: no union — the other
        // conditions decide, or a scan.
        assert_eq!(fields(Filter::new().any_of([ada(), unindexed()])), None);
        assert_eq!(fields(Filter::new().and(!ada())), None);
        let f = Filter::new().any_of([ada(), unindexed()]).gt("age", 65);
        assert_eq!(fields(f), Some(vec!["age".into()]));

        // Eq beats a union, a union beats a range, whatever the order.
        let f = Filter::new()
            .gt("age", 65)
            .any_of([ada(), young()])
            .eq("name", "Cy");
        assert_eq!(fields(f), Some(vec!["name".into()]));
        let f = Filter::new().gt("age", 65).any_of([ada(), young()]);
        assert_eq!(fields(f).map(|f| f.len()), Some(2));
        // An AND nested at the top is like more top-level conditions.
        let f = Filter::new().and(Condition::all([unindexed(), ada()]));
        assert_eq!(fields(f), Some(vec!["name".into()]));

        // Reading in sort order gives way to an OR found by value.
        let sorted = |f: Filter| f.sort_desc("age").limit(5).index_order(&indexes).is_some();
        assert!(sorted(Filter::new().gt("age", 65)));
        assert!(!sorted(
            Filter::new().any_of([ada(), Condition::eq("name", "Bob")])
        ));
        assert!(!sorted(Filter::new().eq("name", "Ada")));
        assert!(sorted(Filter::new().any_of([ada(), unindexed()])));
    }

    /// SPEC §124.1: one equality for arrays, objects and binary, by
    /// their contents, with single values inside as `eq` sees them.
    #[test]
    fn whole_values_are_equal_by_their_contents() {
        let v = |text: &str| text.parse::<Document>().unwrap();
        for (a, b) in [
            ("[1, 2]", "[1, 2]"),
            ("[1, 2]", "[1.0, 2]"),
            (r#"{"a": 1, "b": 2}"#, r#"{"b": 2, "a": 1}"#),
            (r#"{"a": {"x": [null]}}"#, r#"{"a": {"x": [null]}}"#),
            (r#"{"a": null}"#, r#"{"a": null}"#),
            ("[]", "[]"),
            ("{}", "{}"),
            (r#"{"$binary": "AAEC"}"#, r#"{"$binary": "AAEC"}"#),
            (
                r#"[{"$date": "2026-01-01T00:00:00Z"}]"#,
                r#"[{"$date": "2026-01-01T00:00:00Z"}]"#,
            ),
        ] {
            assert!(equal_values(&v(a), &v(b)), "{a} = {b}");
            assert!(equal_values(&v(b), &v(a)), "{b} = {a}");
            assert_eq!(canonical(&v(a)), canonical(&v(b)), "{a} and {b} one group");
            for op in [Op::Eq, Op::Lte, Op::Gte, Op::Lt, Op::Gt, Op::Ne] {
                let holds = value_matches(&op, &v(a), &v(b));
                assert_eq!(holds, op == Op::Eq, "{a} {op:?} {b}");
            }
        }
        for (a, b) in [
            ("[1, 2]", "[2, 1]"),
            ("[1, 2]", "[1, 2, 3]"),
            ("[1]", r#"["1"]"#),
            (r#"{"a": 1}"#, r#"{"a": 1, "b": 2}"#),
            ("{}", r#"{"a": null}"#),
            (r#"{"a": 1}"#, r#"{"b": 1}"#),
            ("[]", "{}"),
            ("[1]", "1"),
            (r#"{"$binary": "AAEC"}"#, r#"{"$binary": "AQ=="}"#),
            (r#"{"$binary": "AAEC"}"#, "[0, 1, 2]"),
            (r#"[{"$float": "NaN"}]"#, r#"[{"$float": "NaN"}]"#),
            ("[9007199254740993]", "[9007199254740992.0]"),
        ] {
            assert!(!equal_values(&v(a), &v(b)), "{a} != {b}");
            assert!(value_matches(&Op::Ne, &v(a), &v(b)), "{a} != {b}");
            assert!(!value_matches(&Op::Eq, &v(a), &v(b)), "not {a} = {b}");
        }
        // Not equal by `==`, which tells `1` from `1.0`.
        assert_ne!(v("[1]"), v("[1.0]"));
        // A missing field and a null one aren't arrays: `!=` holds.
        assert!(value_matches(&Op::Ne, &Document::Null, &v("[1]")));
        assert!(!value_matches(&Op::Eq, &Document::Null, &v("[]")));
    }
}
