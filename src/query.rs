use crate::catalog::IndexMeta;
use crate::document::Document;
use crate::index::{KeyRange, key};

#[derive(Debug, Clone)]
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

/// What a document must satisfy (SPEC §36): a comparison of one field, or
/// several conditions combined — nested as deep as needed.
#[derive(Debug, Clone)]
pub enum Condition {
    /// `field op value`.
    Compare {
        field: String,
        op: Op,
        value: Document,
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

#[derive(Debug, Clone)]
pub struct Sort {
    pub field: String,
    pub order: SortOrder,
}

/// A query: conditions that must all hold (each may nest ORs, ANDs and
/// NOTs, SPEC §36), plus an optional sort-by-field and limit. Evaluated
/// by scanning, or over index ranges when the conditions allow it
/// (`index_ranges`, SPEC §28.4, §36.3), or by reading the sort field's
/// index in order (`index_order`, SPEC §34.2).
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub conditions: Vec<Condition>,
    pub sort: Option<Sort>,
    pub limit: Option<usize>,
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
/// # assert_eq!(newest_complete.conditions.len(), 2);
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

    /// Sorts by `field`, smallest first — replacing any earlier sort.
    pub fn sort_asc(self, field: impl Into<String>) -> Self {
        self.sort_by(field, SortOrder::Asc)
    }

    /// Sorts by `field`, largest first — replacing any earlier sort.
    pub fn sort_desc(self, field: impl Into<String>) -> Self {
        self.sort_by(field, SortOrder::Desc)
    }

    pub fn sort_by(mut self, field: impl Into<String>, order: SortOrder) -> Self {
        self.sort = Some(Sort {
            field: field.into(),
            order,
        });
        self
    }

    /// At most `n` results — replacing any earlier limit.
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
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

        if let Some(sort) = &self.sort {
            // Stable: equal values keep the order they came in.
            results.sort_by(|a, b| {
                let a = value_or_null(doc_of(a), &sort.field);
                let b = value_or_null(doc_of(b), &sort.field);
                sort_order(a, b, sort.order)
            });
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
/// `Union` (an OR) beats `ByRange`.
struct Bounds<'a> {
    ranges: Vec<(&'a IndexMeta, KeyRange)>,
    kind: BoundKind,
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
/// nested group is bounded on its own; the best of all that wins, the
/// first among equals. `Ne` and `Contains` never use an index, nor does a
/// `Not`.
fn bounds_for_all<'a>(conditions: &[Condition], indexes: &'a [IndexMeta]) -> Option<Bounds<'a>> {
    let mut best: Option<Bounds<'a>> = None;
    let mut consider = |candidate: Bounds<'a>| {
        if best.as_ref().is_none_or(|b| candidate.kind < b.kind) {
            best = Some(candidate);
        }
    };
    for (i, condition) in conditions.iter().enumerate() {
        match condition {
            Condition::Compare { field, .. } => {
                let Some(index) = indexes.iter().find(|index| index.field == *field) else {
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
                    } if f == field => Some((op, key::range_for(op, value)?)),
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
    best
}

/// Bounds for one condition: an `All` like the filter's own list, an
/// `Any` only if every branch can be bounded (the union of theirs), a
/// `Not` never.
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
            })
        }
        Condition::Not(_) => None,
    }
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
    /// with a `sort` and a `limit`, on a field with an index, unless the
    /// conditions find documents by value some other way: an `Eq` on
    /// another indexed field, or an OR of indexed branches (a few
    /// documents found by value beat walking in order). With it, the sort
    /// field's own range comparisons narrowed to one range, or `None` for
    /// the whole index.
    pub(crate) fn index_order<'a>(
        &self,
        indexes: &'a [IndexMeta],
    ) -> Option<(&'a IndexMeta, Option<KeyRange>)> {
        let sort = self.sort.as_ref()?;
        self.limit?;
        // A multikey index holds a document once per element, in element
        // order: no order of documents (SPEC §42.3).
        let index = indexes
            .iter()
            .find(|i| i.field == sort.field && !is_multi(&i.field))?;
        let on_sort_field =
            |c: &&Condition| matches!(c, Condition::Compare { field, .. } if *field == sort.field);
        let others: Vec<Condition> = self
            .conditions
            .iter()
            .filter(|c| !on_sort_field(c))
            .cloned()
            .collect();
        if bounds_for_all(&others, indexes).is_some_and(|b| b.kind != BoundKind::ByRange) {
            return None;
        }
        let range = self
            .conditions
            .iter()
            .filter_map(|c| match c {
                Condition::Compare { field, op, value } if *field == sort.field => {
                    key::range_for(op, value)
                }
                _ => None,
            })
            .reduce(KeyRange::intersect);
        Some((index, range))
    }
}

/// The value at `path` in `doc` (SPEC §31): a field name, or several
/// joined by dots — `address.city` is the `city` field of the object in
/// the `address` field. `None` if a step is missing or isn't an object;
/// arrays aren't walked into. A dot always separates, so a key that
/// itself contains a dot can't be reached by a path.
pub(crate) fn field_value<'a>(doc: &'a Document, path: &str) -> Option<&'a Document> {
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
/// - a path without `[*]` gives exactly `value_or_null`.
pub(crate) fn values_at<'a>(doc: &'a Document, path: &str) -> Vec<&'a Document> {
    let mut values = vec![doc];
    for step in path.split('.') {
        let name = step.trim_end_matches("[*]");
        let fan_outs = (step.len() - name.len()) / 3;
        values = values
            .into_iter()
            .map(|value| match value {
                Document::Object(map) => map.get(name).unwrap_or(&Document::Null),
                _ => &Document::Null,
            })
            .collect();
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
        let values = values_at(doc, field);
        return match op {
            Op::Ne => !values.iter().any(|v| value_matches(&Op::Eq, v, value)),
            op => values.iter().any(|v| value_matches(op, v, value)),
        };
    }
    value_matches(op, value_or_null(doc, field), value)
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
        assert_eq!(ranges[0].0.field, "tags[*]");
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
            sort: Some(Sort {
                field: "y".into(),
                order: SortOrder::Desc,
            }),
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
            sort: Some(Sort {
                field: "meta.rank".into(),
                order: SortOrder::Desc,
            }),
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
            sort: Some(Sort {
                field: "age".into(),
                order: SortOrder::Asc,
            }),
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
            sort: Some(Sort {
                field: "age".into(),
                order: SortOrder::Desc,
            }),
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
            sort: Some(Sort {
                field: "tst".into(),
                order: SortOrder::Desc,
            }),
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
                    sort: Some(Sort {
                        field: "v".into(),
                        order,
                    }),
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
            sort: Some(Sort {
                field: "age".into(),
                order: SortOrder::Asc,
            }),
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
            field: field.into(),
            root: 1,
            unique: false,
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
            Some(ranges[0].0.field.clone())
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
            Some(ranges.iter().map(|(i, _)| i.field.clone()).collect())
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
