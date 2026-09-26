use crate::catalog::IndexMeta;
use crate::document::Document;
use crate::index::{KeyRange, key};

/// `#[non_exhaustive]`: a `match` on it outside this crate needs a `_`
/// arm, so a new operator isn't a breaking change (SPEC §45.4).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
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
/// `#[non_exhaustive]`, like `Op`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Condition {
    /// `field op value`.
    Compare {
        field: String,
        op: Op,
        value: Document,
    },
    /// The field is there — even if it holds null, unlike `is_not_null`
    /// (SPEC §45.1). On a path with `[*]`: some element has it.
    Exists { field: String },
    /// The field is an array whose length `op`-compares true against
    /// `size` (SPEC §45.2) — `Op::Eq` and 0 for an empty one. `Ne` is
    /// `Eq` negated, so it also holds where there's no array at all.
    Size { field: String, op: Op, size: usize },
    /// Some element of the array at `field` meets `condition` on its own
    /// (SPEC §46): `items` has an item with `sku == "A1"` *and* `qty >
    /// 2`, where `items[*].sku == "A1" AND items[*].qty > 2` may be met
    /// by two different items. Paths in `condition` start at the
    /// element; `""` is the element itself.
    ElemMatch {
        field: String,
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
    pub fn compare(field: impl Into<String>, op: Op, value: impl Into<Document>) -> Self {
        Condition::Compare {
            field: field.into(),
            op,
            value: value.into(),
        }
    }

    pub fn eq(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Eq, value)
    }

    pub fn ne(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Ne, value)
    }

    pub fn lt(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Lt, value)
    }

    pub fn lte(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Lte, value)
    }

    pub fn gt(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Gt, value)
    }

    pub fn gte(field: impl Into<String>, value: impl Into<Document>) -> Self {
        Self::compare(field, Op::Gte, value)
    }

    pub fn contains(field: impl Into<String>, needle: impl Into<String>) -> Self {
        Self::compare(field, Op::Contains, needle.into())
    }

    pub fn is_null(field: impl Into<String>) -> Self {
        Self::compare(field, Op::Eq, Document::Null)
    }

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

    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Condition::Compare { field, op, value } => compare_matches(field, op, value, doc),
            Condition::Exists { field } => !present_at(doc, field).is_empty(),
            Condition::Size { field, op, size } => size_matches(field, op, *size, doc),
            Condition::ElemMatch { field, condition } => {
                values_at(doc, field).into_iter().any(|value| match value {
                    Document::Array(elements) => elements.iter().any(|e| condition.matches(e)),
                    _ => false,
                })
            }
            Condition::All(conditions) => conditions.iter().all(|c| c.matches(doc)),
            Condition::Any(conditions) => conditions.iter().any(|c| c.matches(doc)),
            Condition::Not(condition) => !condition.matches(doc),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
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
/// NOTs, SPEC §36), plus an optional sort and limit. Evaluated by
/// scanning, or over index ranges when the conditions allow it
/// (`index_ranges`, SPEC §28.4, §36.3), or by reading an index in sort
/// order (`index_order`, SPEC §34.2, §47).
///
/// Built only through its methods (SPEC §60), so how it holds its parts
/// isn't part of the API:
///
/// ```
/// use trunkdb::query::Filter;
///
/// let newest_queued = Filter::new().eq("status", "Queued").sort_desc("created").limit(20);
/// let all = Filter::new().limit(None); // `limit` takes an `Option` too
/// # let _ = (newest_queued, all);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub(crate) conditions: Vec<Condition>,
    /// Sort keys, most significant first (SPEC §47): by the first field,
    /// documents equal in it by the second, and so on; equal in all of
    /// them, by id. Empty: no sort.
    pub(crate) sort: Vec<Sort>,
    pub(crate) limit: Option<usize>,
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
    /// Matches every document: no conditions, no sort, no limit.
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

    pub fn lt(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Lt, value)
    }

    pub fn lte(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Lte, value)
    }

    pub fn gt(self, field: impl Into<String>, value: impl Into<Document>) -> Self {
        self.condition(field, Op::Gt, value)
    }

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

    /// At most `n` results — replacing any earlier limit.
    /// At most `n` documents: `limit(20)`, or `limit(None)` for no limit,
    /// so an `Option<usize>` can be passed through as it is.
    pub fn limit(mut self, n: impl Into<Option<usize>>) -> Self {
        self.limit = n.into();
        self
    }
}

impl Filter {
    pub fn matches(&self, doc: &Document) -> bool {
        self.conditions.iter().all(|c| c.matches(doc))
    }

    /// Applies conditions, then sort, then limit — in that order,
    /// matching SQL's `WHERE` -> `ORDER BY` -> `LIMIT` evaluation order
    /// (sorting an already-filtered set is both correct and cheaper than
    /// sorting everything first).
    pub fn apply(&self, docs: impl IntoIterator<Item = Document>) -> Vec<Document> {
        self.apply_to(docs, |doc| doc)
    }

    /// `apply` for items that carry a document rather than being one —
    /// e.g. `(DocId, Document)` pairs, so `find_with_ids` keeps each id
    /// with its document through filtering, sorting and limiting.
    pub fn apply_to<T>(
        &self,
        items: impl IntoIterator<Item = T>,
        doc_of: impl Fn(&T) -> &Document,
    ) -> Vec<T> {
        let mut results: Vec<T> = items
            .into_iter()
            .filter(|item| self.matches(doc_of(item)))
            .collect();

        if !self.sort.is_empty() {
            // Stable: equal in every key, they keep the order they came in.
            results.sort_by(|a, b| compare_by(&self.sort, doc_of(a), doc_of(b)));
        }

        if let Some(limit) = self.limit {
            results.truncate(limit);
        }

        results
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryPlan {
    /// Every document of the collection is read and checked.
    Scan,
    /// Only the documents in a range of the index on `field` are read,
    /// then checked against the whole filter.
    Index { field: String },
    /// The index on the sort field `field` is read in sort order, and
    /// documents are checked one by one until `limit` of them match; the
    /// rest are never read (SPEC §34.2).
    IndexOrder { field: String },
    /// Several index ranges are read — one per branch of an OR, on the
    /// indexes on `fields` — each document once, then checked against
    /// the whole filter (SPEC §36.3).
    IndexUnion { fields: Vec<String> },
}

/// Index ranges whose union holds every document a condition can match
/// (SPEC §36.3), and how good a choice that is: `ByValue` (one `Eq`) beats
/// `Union` (an OR) beats `ByRange`; among equal kinds, bounds on more
/// fields beat fewer — a compound index narrowed by two conditions
/// beats one index narrowed by one (SPEC §43.3).
struct Bounds<'a> {
    ranges: Vec<(&'a IndexMeta, KeyRange)>,
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
                let on_field = conditions.iter().filter_map(|c| match c {
                    Condition::Compare {
                        field: f,
                        op,
                        value,
                    } if f == field && (!index.sparse || rules_out_null(op, value)) => {
                        Some((op, key::range_for(op, value)?))
                    }
                    _ => None,
                });
                let (by_value, mut ranges): (Vec<bool>, Vec<KeyRange>) = on_field
                    .map(|(op, range)| (matches!(op, Op::Eq), range))
                    .unzip();
                if is_multi(field) && !ranges.is_empty() {
                    // Different elements may meet different comparisons —
                    // `tags[*] > 5 AND tags[*] < 3` holds for `[1, 10]` —
                    // so their ranges can't be intersected (SPEC §42.3).
                    // One of them bounds the documents: an `Eq`'s if any.
                    let one = by_value.iter().position(|&b| b).unwrap_or(0);
                    ranges = vec![ranges.swap_remove(one)];
                }
                if let Some(range) = ranges.into_iter().reduce(KeyRange::intersect) {
                    let kind = match by_value.contains(&true) {
                        true => BoundKind::ByValue,
                        false => BoundKind::ByRange,
                    };
                    consider(Bounds {
                        ranges: vec![(index, range)],
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
    let (prefix, equal) = equal_prefix(index, conditions, index.fields.len());
    let range = index
        .fields
        .get(equal)
        .and_then(|next| field_range(index, conditions, next));
    let fields = equal + range.is_some() as usize;
    let range = match range {
        Some(range) => range.under(&prefix),
        None if equal > 0 => KeyRange::prefixed(&prefix),
        None => return None,
    };
    let kind = match equal > 0 {
        true => BoundKind::ByValue,
        false => BoundKind::ByRange,
    };
    Some(Bounds {
        ranges: vec![(index, range)],
        kind,
        fields,
    })
}

/// How many of `index`'s first fields, up to `max`, an `Eq` among
/// `conditions` fixes — and those values encoded one after another, as
/// its keys begin.
fn equal_prefix(index: &IndexMeta, conditions: &[Condition], max: usize) -> (Vec<u8>, usize) {
    let mut prefix = Vec::new();
    for (i, field) in index.fields.iter().take(max).enumerate() {
        let equal = conditions.iter().find_map(|c| match c {
            Condition::Compare {
                field: f,
                op: Op::Eq,
                value,
            } if f == field => key::range_for_in(&Op::Eq, value, index.fields.len()),
            _ => None,
        });
        match equal {
            Some(range) => prefix.extend(range.start),
            None => return (prefix, i),
        }
    }
    (prefix, max)
}

/// The comparisons on `field` as one range of `index`'s values for it —
/// relative to where that value starts in a key.
fn field_range(index: &IndexMeta, conditions: &[Condition], field: &str) -> Option<KeyRange> {
    conditions
        .iter()
        .filter_map(|c| match c {
            Condition::Compare {
                field: f,
                op,
                value,
            } if f == field => key::range_for_in(op, value, index.fields.len()),
            _ => None,
        })
        .reduce(KeyRange::intersect)
}

impl Filter {
    /// The index ranges `find` reads, among `indexes` — `None` for a full
    /// scan. Rule-based, not cost-based (SPEC §28.4, §36.3): an `Eq` on an
    /// indexed field, else an OR whose every branch an index can bound,
    /// else a range on an indexed field. The ranges may hold documents
    /// the filter rejects, never the other way around; the same document
    /// may be in several.
    pub(crate) fn index_ranges<'a>(
        &self,
        indexes: &'a [IndexMeta],
    ) -> Option<Vec<(&'a IndexMeta, KeyRange)>> {
        bounds_for_all(&self.conditions, indexes).map(|b| b.ranges)
    }

    /// The index `find` reads in sort order (SPEC §34.2) — for a filter
    /// with a `sort` and a `limit` — and how: see `OrderedRead`. An index
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
    pub(crate) fn index_order<'a>(&self, indexes: &'a [IndexMeta]) -> Option<OrderedRead<'a>> {
        let sort = self.sort.first()?;
        self.limit?;
        let mut best: Option<(&IndexMeta, Vec<u8>, usize, usize)> = None;
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
            let (prefix, equal) = equal_prefix(index, &self.conditions, at);
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
                |(_, _, fixed, most): &(_, _, usize, usize)| (at, served) > (*fixed, *most);
            if best.as_ref().is_none_or(better) {
                best = Some((index, prefix, at, served));
            }
        }
        let (index, prefix, fixed, served) = best?;
        if fixed == 0 {
            let on_sort_field = |c: &&Condition| matches!(c, Condition::Compare { field, .. } if *field == sort.field);
            let others: Vec<Condition> = self
                .conditions
                .iter()
                .filter(|c| !on_sort_field(c))
                .cloned()
                .collect();
            if bounds_for_all(&others, indexes).is_some_and(|b| b.kind != BoundKind::ByRange) {
                return None;
            }
        }
        let range = field_range(index, &self.conditions, &sort.field);
        let range = match fixed {
            0 => range,
            _ => Some(range.map_or_else(|| KeyRange::prefixed(&prefix), |r| r.under(&prefix))),
        };
        Some(OrderedRead {
            index,
            range,
            fixed,
            served,
        })
    }
}

/// How `find` reads an index in sort order (`Filter::index_order`).
#[derive(Debug)]
pub(crate) struct OrderedRead<'a> {
    pub index: &'a IndexMeta,
    /// The part of it to read — `None` for all of it.
    pub range: Option<KeyRange>,
    /// How many of its first fields an `Eq` fixes: the first sort
    /// field is the one after them.
    pub fixed: usize,
    /// How many sort keys, from the first, its fields give the order of
    /// (SPEC §47) — at least one.
    pub served: usize,
}

/// The value at `path` in `doc` (SPEC §31): a field name, or several
/// joined by dots — `address.city` is the `city` field of the object in
/// the `address` field. `None` if a step is missing or isn't an object;
/// arrays aren't walked into. A dot always separates, so a key that
/// itself contains a dot can't be reached by a path. The empty path is
/// `doc` itself — an element, in `Condition::elem_match` (SPEC §46.1).
pub(crate) fn field_value<'a>(doc: &'a Document, path: &str) -> Option<&'a Document> {
    if path.is_empty() {
        return Some(doc);
    }
    path.split('.').try_fold(doc, |value, name| match value {
        Document::Object(map) => map.get(name),
        _ => None,
    })
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
    walk(doc, path, true)
}

/// The values actually stored at `path` in `doc` (SPEC §45.1): like
/// `values_at`, but a missing field — or a step into something that isn't
/// an object — gives nothing instead of a null.
fn present_at<'a>(doc: &'a Document, path: &str) -> Vec<&'a Document> {
    walk(doc, path, false)
}

/// `values_at`, or with `missing_is_null` false `present_at`.
fn walk<'a>(doc: &'a Document, path: &str, missing_is_null: bool) -> Vec<&'a Document> {
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

/// A comparison on a path with `[*]` holds if it holds for any value
/// there (SPEC §42.1) — except `Ne`, which stays what it is everywhere:
/// `Eq` negated. So `tags[*] != "x"` means no tag is `"x"`.
fn compare_matches(field: &str, op: &Op, value: &Document, doc: &Document) -> bool {
    if is_multi(field) {
        return any_matches(op, value, values_at(doc, field));
    }
    value_matches(op, value_or_null(doc, field), value)
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

/// `Condition::Size` (SPEC §45.2): the lengths of the arrays at `field`
/// — none if it isn't one; on a path with `[*]`, one per element that
/// is one — compared as `compare_matches` compares values.
fn size_matches(field: &str, op: &Op, size: usize, doc: &Document) -> bool {
    let lengths: Vec<Document> = values_at(doc, field)
        .into_iter()
        .filter_map(|value| match value {
            Document::Array(elements) => Some(Document::Int(elements.len() as i64)),
            _ => None,
        })
        .collect();
    any_matches(op, &Document::Int(size as i64), &lengths)
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
    let ord = compare(field_value, value);
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
fn compare(a: &Document, b: &Document) -> Option<std::cmp::Ordering> {
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
        _ => UNORDERED,
    }
}

const UNORDERED: u8 = 4;

/// Whether `sort_order` puts `value` among the values nothing orders —
/// arrays, objects, binary, ids, NaN. No index holds them.
pub(crate) fn is_unordered(value: &Document) -> bool {
    sort_rank(value) == UNORDERED
}

/// The order a sort puts two field values in (SPEC §34.1) — a total
/// order, so any sort is well-defined: null (and missing) before bools
/// before numbers before strings, each by value, reversed for `Desc`;
/// values nothing orders come after all of them in both directions, as
/// equals. It is the order of an index's keys, so reading an index gives
/// what sorting in memory gives.
/// `a` against `b` by the sort `keys` — by the first, where that's
/// equal by the second, and so on (SPEC §47).
pub(crate) fn compare_by(keys: &[Sort], a: &Document, b: &Document) -> std::cmp::Ordering {
    keys.iter()
        .map(|key| {
            let (a, b) = (value_or_null(a, &key.field), value_or_null(b, &key.field));
            sort_order(a, b, key.order)
        })
        .find(|ord| ord.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

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
    use indexmap::IndexMap;

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
        let mut map = IndexMap::new();
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
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].0.name());
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
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].0.name());
        let f = Filter::new().elem_match(
            "items",
            Condition::eq("sku", "A1") & Condition::gt("qty", 2),
        );
        assert_eq!(plan(f.clone()).as_deref(), Some("items[*].sku"));
        let range = &f.index_ranges(&indexes).unwrap()[0].1;
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
        assert_eq!(ranges[0].0.name(), "tags[*]");
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
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].0.name());
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
        assert!(ranges[0].1.contains(&k(2)) && !ranges[0].1.contains(&k(4)));
    }

    /// Read in sort order when the sort field follows fields an `Eq`
    /// fixes — the more fixed, the better; not when one before it is
    /// free.
    #[test]
    fn a_compound_index_gives_the_order_after_its_fixed_fields() {
        let indexes = [index("created"), compound(&["status", "created"])];
        let order = |f: Filter| f.limit(20).index_order(&indexes).map(|o| o.index.name());
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

    /// A sparse index has no entry for a null (SPEC §44): it bounds only
    /// by comparisons null fails, and is read in order only where the
    /// filter keeps nulls out.
    #[test]
    fn a_sparse_index_is_only_used_where_nulls_are_ruled_out() {
        let indexes = [sparse(&["nick"]), sparse(&["tags[*]"])];
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].0.name());
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
        assert!(ranges[0].1.contains(&five), "{:?}", ranges[0].1);

        let order = |f: Filter| f.limit(5).index_order(&indexes).map(|o| o.index.name());
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
        let plan = |f: Filter| f.index_ranges(&indexes).map(|r| r[0].0.name());
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
        let order = |f: Filter| f.limit(5).index_order(&indexes).map(|o| o.index.name());
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
                .map(|o| (o.index.name(), o.fixed, o.served))
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
            (o.index.name(), o.fixed, o.served),
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
        assert!(ranges[0].1.contains(&key), "{:?}", ranges[0].1);

        // An `Eq` among them is the one that bounds.
        let filter = Filter::new().gt("tags[*]", 5).eq("tags[*]", 1);
        let ranges = filter.index_ranges(&indexes).unwrap();
        let key = key::secondary(&Document::Int(1), crate::DocId([0; 16])).unwrap();
        assert!(ranges[0].1.contains(&key));
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
            .limit(5); // replaces the first
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
            limit: Some(5),
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
            limit: Some(2),
        };

        assert_eq!(
            filter.apply(docs),
            vec![
                doc(&[("age", Document::Int(20))]),
                doc(&[("age", Document::Int(30))]),
            ]
        );
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
            Some(ranges[0].0.name())
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
        let (_, range) = f.index_ranges(&indexes).unwrap().remove(0);
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
            Some(ranges.iter().map(|(i, _)| i.name()).collect())
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
}
