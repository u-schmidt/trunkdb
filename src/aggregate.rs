//! Aggregates (SPEC §107): the documents a filter finds, put in groups
//! by the values of some fields, and per group a count, a sum, a
//! minimum, a maximum or an average — SQL's `GROUP BY`, MongoDB's
//! `$group`. `Aggregate` says what; `Groups` keeps the groups as
//! matches come in, from their documents or from their index entries
//! (§107.5), and turns them into rows.

use crate::document::{DocId, Document, ID_FIELD, encode_document};
use crate::map::Map;
use crate::path::Path;
use crate::query::{Condition, Filter, SortOrder, is_unordered, sort_order};
use crate::storage::RecordLocation;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// What `Collection::aggregate` computes from the documents its filter
/// finds (SPEC §107):
///
/// ```
/// use trunkdb::query::{Aggregate, Condition};
///
/// let per_status = Aggregate::new()
///     .group_by("status")
///     .count("n")
///     .avg("tries", "average_tries")
///     .max("created", "newest")
///     .having(Condition::gt("n", 10))
///     .sort_desc("n")
///     .limit(5);
/// # let _ = per_status;
/// ```
///
/// Each row holds its group's values at their paths, then each column
/// in the order added, and no `_id`. Without `group_by`, every match is
/// one group, and there is one row even for no matches.
///
/// - **Groups:** a missing field groups with `null` (SPEC §32); values a
///   filter finds equal are one group (`5` and `5.0`); values no
///   comparison orders (arrays, objects, binary, NaN) one group per
///   equal value. A row shows its group's first document's values, by
///   id. An array groups as a value, not by its elements.
/// - **`count`:** the documents in the group.
/// - **`sum`:** of the values that are numbers, the others passed over;
///   an `Int` when every value summed is one and the total fits, a
///   `Float` otherwise, `0` when there is none. Summed exactly and
///   rounded once, so the total doesn't depend on the order documents
///   are read in; a zero is never `-0.0`.
/// - **`avg`:** that total divided by how many numbers it holds, a
///   `Float`; `null` if there are none.
/// - **`min`, `max`:** in the order a sort uses (SPEC §34.1), `null` and
///   missing passed over, values no comparison orders only where there
///   is nothing else; of equal ones, the first document's by id. `null`
///   if there is no value.
///
/// Rows come in the order of their group values, ascending, unless
/// `sort_asc` and the like say otherwise. `having` picks rows before the
/// sort, `skip` and `limit` after it.
#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    pub(crate) group_by: Vec<String>,
    pub(crate) columns: Vec<Column>,
    /// `having`, the row sort, skip and limit: what a filter does to
    /// documents, done to the rows.
    pub(crate) rows: Filter,
}

/// One column of a row.
#[derive(Debug, Clone)]
pub(crate) struct Column {
    pub name: String,
    pub kind: Kind,
    /// The values' path; `None` for `count`.
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl Aggregate {
    /// Every match one group, no columns yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Groups by the value at `path` too: call it again for several
    /// fields. A dotted path is nested in the row as stored.
    pub fn group_by(mut self, path: impl Into<String>) -> Self {
        self.group_by.push(path.into());
        self
    }

    /// A column `name`: how many documents the group has.
    pub fn count(self, name: impl Into<String>) -> Self {
        self.column(Kind::Count, None, name)
    }

    /// A column `name`: the sum of the numbers at `path`.
    pub fn sum(self, path: impl Into<String>, name: impl Into<String>) -> Self {
        self.column(Kind::Sum, Some(path.into()), name)
    }

    /// A column `name`: the smallest value at `path`.
    pub fn min(self, path: impl Into<String>, name: impl Into<String>) -> Self {
        self.column(Kind::Min, Some(path.into()), name)
    }

    /// A column `name`: the largest value at `path`.
    pub fn max(self, path: impl Into<String>, name: impl Into<String>) -> Self {
        self.column(Kind::Max, Some(path.into()), name)
    }

    /// A column `name`: the average of the numbers at `path`.
    pub fn avg(self, path: impl Into<String>, name: impl Into<String>) -> Self {
        self.column(Kind::Avg, Some(path.into()), name)
    }

    fn column(mut self, kind: Kind, path: Option<String>, name: impl Into<String>) -> Self {
        self.columns.push(Column {
            name: name.into(),
            kind,
            path,
        });
        self
    }

    /// Keeps only the rows `condition` matches, by column name and group
    /// field — SQL's `HAVING`. Several are ANDed.
    pub fn having(mut self, condition: Condition) -> Self {
        self.rows = self.rows.and(condition);
        self
    }

    /// Rows by `column` (or a group field), smallest first, replacing
    /// any earlier order.
    pub fn sort_asc(mut self, column: impl Into<String>) -> Self {
        self.rows = self.rows.sort_asc(column);
        self
    }

    /// Rows by `column`, largest first, replacing any earlier order.
    pub fn sort_desc(mut self, column: impl Into<String>) -> Self {
        self.rows = self.rows.sort_desc(column);
        self
    }

    /// Then by `column`, smallest first, for rows equal so far.
    pub fn then_asc(mut self, column: impl Into<String>) -> Self {
        self.rows = self.rows.then_asc(column);
        self
    }

    /// Then by `column`, largest first.
    pub fn then_desc(mut self, column: impl Into<String>) -> Self {
        self.rows = self.rows.then_desc(column);
        self
    }

    /// Passes over the first `n` rows, after `having` and the sort.
    pub fn skip(mut self, n: usize) -> Self {
        self.rows = self.rows.skip(n);
        self
    }

    /// At most `n` rows.
    pub fn limit(mut self, n: impl Into<Option<usize>>) -> Self {
        self.rows = self.rows.limit(n);
        self
    }

    /// The rows for `docs`, as `Collection::aggregate` gives them for the
    /// documents its filter finds: the meaning in memory, as
    /// `Filter::apply` is for `find`. A document's `_id` orders it for
    /// "first by id"; documents without one come after, in the order
    /// given. Refused as `aggregate` refuses (`check`).
    pub fn apply(&self, docs: impl IntoIterator<Item = Document>) -> crate::Result<Vec<Document>> {
        self.check()?;
        let mut groups = Groups::new(self);
        for (seq, doc) in docs.into_iter().enumerate() {
            let order = match &doc {
                Document::Object(map) => match map.get(ID_FIELD) {
                    Some(Document::Id(id)) => Order::id(*id),
                    _ => Order::after(seq as u64),
                },
                _ => Order::after(seq as u64),
            };
            groups.add_document(&doc, order, None);
        }
        groups.finish(|_| unreachable!("every value is the document's"))
    }

    /// Whether it can run, before anything is read (SPEC §107.2): every
    /// column named, once, not by a dot or `[*]`, nor by a group field's
    /// first step; no path with `[*]`, nor empty, nor `_id` for a value;
    /// no group field inside another; `having` and the row sort naming
    /// only columns and group fields.
    pub(crate) fn check(&self) -> crate::Result<()> {
        let refuse = |why: String| -> crate::Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, why).into())
        };
        let firsts: Vec<&str> = self.group_by.iter().map(|p| first_step(p)).collect();
        let paths = self
            .group_by
            .iter()
            .chain(self.columns.iter().filter_map(|c| c.path.as_ref()));
        for path in paths {
            if path.is_empty() || path.contains("[*]") {
                return refuse(format!(
                    "an aggregate's path must name a field, without `[*]`: `{path}`"
                ));
            }
        }
        for (i, a) in self.group_by.iter().enumerate() {
            for (j, b) in self.group_by.iter().enumerate() {
                let inside =
                    b.len() > a.len() && b.starts_with(a.as_str()) && b.as_bytes()[a.len()] == b'.';
                if i != j && (a == b || inside) {
                    return refuse(format!("grouped by `{a}` and by `{b}`, one in the other"));
                }
            }
        }
        for (i, column) in self.columns.iter().enumerate() {
            let name = column.name.as_str();
            if name.is_empty() || name.contains(['.', '[']) {
                return refuse(format!("a column needs a plain name: `{name}`"));
            }
            if self.columns[..i].iter().any(|c| c.name == name) {
                return refuse(format!("two columns named `{name}`"));
            }
            if firsts.contains(&name) {
                return refuse(format!("a column named `{name}`, as a group field is"));
            }
        }
        let known =
            |field: &str| firsts.contains(&field) || self.columns.iter().any(|c| c.name == field);
        match self.rows.fields() {
            Some(fields) => {
                if let Some(field) = fields.iter().find(|field| !known(field)) {
                    return refuse(format!("`{field}` is no column and no group field"));
                }
            }
            None => return refuse("`having` and the row sort name columns".to_string()),
        }
        Ok(())
    }

    /// Every path it reads: the group fields', then the columns'.
    pub(crate) fn paths(&self) -> Vec<&str> {
        let columns = self.columns.iter().filter_map(|c| c.path.as_deref());
        self.group_by
            .iter()
            .map(String::as_str)
            .chain(columns)
            .collect()
    }
}

fn first_step(path: &str) -> &str {
    path.split(['.', '[']).next().unwrap_or_default()
}

/// Where a match comes in "first by id": by its id, or, without one,
/// after every id in the order given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Order(bool, DocId, u64);

impl Order {
    pub fn id(id: DocId) -> Self {
        Order(false, id, 0)
    }

    fn after(seq: u64) -> Self {
        Order(true, DocId::NIL, seq)
    }
}

/// A sum kept exact (SPEC §107.4): every number added into one wide
/// fixed-point integer, in units of the smallest float (2^-1074), wide
/// enough for any float and 2^60 more, and rounded once at the end —
/// the same total in any order, and no overflow on the way. Integers
/// are kept as an `i128` too, for an `Int` total.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExactSum {
    ints: i128,
    wide: Wide,
    /// Infinities and NaNs, added as IEEE adds them.
    special: Option<f64>,
    floats: bool,
    /// How many numbers.
    n: u64,
}

impl ExactSum {
    /// Adds `value` if it is a number; whether it was.
    pub fn add(&mut self, value: &Document) -> bool {
        match value {
            Document::Int(i) => {
                self.ints += i128::from(*i);
                self.wide.add(i.unsigned_abs(), INT_SHIFT, *i < 0);
            }
            Document::Float(f) if !f.is_finite() => {
                self.floats = true;
                self.special = Some(self.special.map_or(*f, |s| s + f));
            }
            Document::Float(f) => {
                self.floats = true;
                let bits = f.to_bits();
                let exponent = ((bits >> 52) & 0x7FF) as u32;
                let fraction = bits & ((1 << 52) - 1);
                let negative = bits >> 63 == 1;
                match exponent {
                    0 => self.wide.add(fraction, 0, negative),
                    e => self.wide.add(fraction | (1 << 52), e - 1, negative),
                }
            }
            _ => return false,
        }
        self.n += 1;
        true
    }

    /// The total as a float, rounded once (to nearest, ties to even);
    /// never `-0.0`.
    pub fn total(&self) -> f64 {
        match self.special {
            Some(special) => special,
            None => self.wide.to_f64(),
        }
    }

    /// `sum`'s value (SPEC §107.2).
    pub fn sum(&self) -> Document {
        match (self.floats, i64::try_from(self.ints)) {
            (false, Ok(int)) => Document::Int(int),
            _ => Document::Float(self.total()),
        }
    }

    /// `avg`'s value.
    pub fn avg(&self) -> Document {
        match self.n {
            0 => Document::Null,
            n => Document::Float(self.total() / n as f64),
        }
    }
}

/// Where an integer's units start: 1 is 2^1074 units of 2^-1074.
const INT_SHIFT: u32 = 1074;

/// Limbs of the fixed-point integer: 2^-1074 to past `f64::MAX`
/// (2^1024) is 2,098 bits, and 64 more for carries and the sign.
const LIMBS: usize = 34;

/// A two's-complement integer of `LIMBS` 64-bit limbs, lowest first.
#[derive(Debug, Clone)]
struct Wide([u64; LIMBS]);

impl Default for Wide {
    fn default() -> Self {
        Wide([0; LIMBS])
    }
}

impl Wide {
    /// Adds (or with `negative` subtracts) `value << shift`.
    fn add(&mut self, value: u64, shift: u32, negative: bool) {
        let (limb, bit) = ((shift / 64) as usize, shift % 64);
        let wide = u128::from(value) << bit;
        let parts = [wide as u64, (wide >> 64) as u64];
        let mut carry = false;
        for i in limb..LIMBS {
            let part = parts.get(i - limb).copied().unwrap_or(0);
            if part == 0 && !carry && i >= limb + 2 {
                break;
            }
            let (sum, over) = match negative {
                false => {
                    let (a, o1) = self.0[i].overflowing_add(part);
                    let (b, o2) = a.overflowing_add(u64::from(carry));
                    (b, o1 || o2)
                }
                true => {
                    let (a, o1) = self.0[i].overflowing_sub(part);
                    let (b, o2) = a.overflowing_sub(u64::from(carry));
                    (b, o1 || o2)
                }
            };
            self.0[i] = sum;
            carry = over;
        }
    }

    /// Rounded to the nearest float, ties to even; zero is `0.0`.
    fn to_f64(&self) -> f64 {
        let negative = self.0[LIMBS - 1] >> 63 == 1;
        let mut magnitude = self.0;
        if negative {
            // Two's complement: invert, add one.
            let mut carry = true;
            for limb in &mut magnitude {
                let (v, over) = (!*limb).overflowing_add(u64::from(carry));
                *limb = v;
                carry = over;
            }
        }
        let Some(top) = (0..LIMBS).rev().find(|&i| magnitude[i] != 0) else {
            return 0.0;
        };
        let high = top as u32 * 64 + (63 - magnitude[top].leading_zeros());
        let bit = |at: u32| (magnitude[(at / 64) as usize] >> (at % 64)) & 1;
        let value = if high <= 52 {
            // Below 2^53 units: a float's bits as they are (subnormal, or
            // the smallest exponent).
            f64::from_bits(magnitude[0])
        } else {
            let shift = high - 52;
            let mut mantissa = (0..53).fold(0u64, |m, k| m | (bit(shift + k) << k));
            let round = bit(shift - 1) == 1;
            let sticky = (0..shift - 1).any(|at| bit(at) == 1);
            let mut exponent = shift + 1;
            if round && (sticky || mantissa & 1 == 1) {
                mantissa += 1;
                if mantissa == 1 << 53 {
                    mantissa >>= 1;
                    exponent += 1;
                }
            }
            match exponent >= 0x7FF {
                true => f64::INFINITY,
                false => f64::from_bits((u64::from(exponent) << 52) | (mantissa & ((1 << 52) - 1))),
            }
        };
        if negative { -value } else { value }
    }
}

/// A group's values, ordered as rows come (SPEC §107.2): by the order a
/// sort uses, and values no comparison orders by their encoding, so that
/// equal ones are one group and different ones two.
#[derive(Debug, Clone)]
struct GroupKey(Vec<Document>);

impl Ord for GroupKey {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.0.iter().zip(&other.0))
            .map(|(a, b)| match (is_unordered(a), is_unordered(b)) {
                (true, true) => encode_document(a).cmp(&encode_document(b)),
                _ => sort_order(a, b, SortOrder::Asc),
            })
            .find(|ord| ord.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for GroupKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for GroupKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GroupKey {}

/// A value as a match gave it: from its document, or from its index
/// entry, where `stored` says whether it is the value as stored (a
/// number in a key is only its value, `5` and `5.0` alike, SPEC
/// §107.5), and `at` where its document is, to read it if needed.
#[derive(Debug, Clone)]
struct Seen {
    value: Document,
    order: Order,
    stored: bool,
    at: Option<RecordLocation>,
}

#[derive(Debug, Clone)]
enum State {
    Count(u64),
    Sum(ExactSum),
    Avg(ExactSum),
    Min(Option<Seen>),
    Max(Option<Seen>),
}

#[derive(Debug)]
struct Group {
    /// The group's values from its first match by id, and whether they
    /// are as stored.
    first: Vec<Seen>,
    columns: Vec<State>,
}

/// The groups of an aggregate as matches come in (SPEC §107.3, §107.5).
pub(crate) struct Groups<'a> {
    aggregate: &'a Aggregate,
    group_paths: Vec<Path<'a>>,
    column_paths: Vec<Option<Path<'a>>>,
    /// Each group's place in `list`, in the order of its values.
    groups: BTreeMap<GroupKey, usize>,
    list: Vec<Group>,
    /// The group the last match went to.
    last: Option<usize>,
}

impl<'a> Groups<'a> {
    pub fn new(aggregate: &'a Aggregate) -> Self {
        Groups {
            aggregate,
            group_paths: aggregate.group_by.iter().map(|p| Path::new(p)).collect(),
            column_paths: (aggregate.columns.iter())
                .map(|c| c.path.as_deref().map(Path::new))
                .collect(),
            groups: BTreeMap::new(),
            list: Vec::new(),
            last: None,
        }
    }

    /// How many groups so far.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn aggregate(&self) -> &'a Aggregate {
        self.aggregate
    }

    /// A match, from its document (or the fields of it the aggregate
    /// reads): every value as stored.
    pub fn add_document(&mut self, doc: &Document, order: Order, at: Option<RecordLocation>) {
        let group: Vec<Document> = (self.group_paths.iter())
            .map(|path| path.value_or_null(doc).clone())
            .collect();
        let values: Vec<Option<Document>> = (self.column_paths.iter())
            .map(|path| path.as_ref().and_then(|p| p.field_value(doc)).cloned())
            .collect();
        self.add(group, &values, true, order, at);
    }

    /// A match from its index entry (SPEC §107.5): `group` and `values`
    /// as `add_document` takes them, each with whether it is as stored.
    /// `values` holds `None` for `count` and for a missing value.
    pub fn add_entry(
        &mut self,
        group: Vec<(Document, bool)>,
        values: Vec<Option<(Document, bool)>>,
        order: Order,
        at: RecordLocation,
    ) {
        let first: Vec<Seen> = (group.into_iter())
            .map(|(value, stored)| Seen {
                value,
                order,
                stored,
                at: Some(at),
            })
            .collect();
        let i = self.group_index(first);
        self.add_values(i, values, order, at);
    }

    /// A match from its index entry in the group the last match went to
    /// — its key's bytes up to the group's values the same (SPEC
    /// §107.5): `values` as `add_entry` takes them.
    pub fn add_entry_to_last(
        &mut self,
        values: Vec<Option<(Document, bool)>>,
        order: Order,
        at: RecordLocation,
    ) {
        let i = self.last.expect("a group added before");
        let group = &mut self.list[i];
        if group.first.first().is_some_and(|f| order < f.order) {
            // The value held is a later match's: equal, and the same but
            // for a number (`5` for `5.0`), which the row reads from this
            // one's document.
            for seen in &mut group.first {
                seen.order = order;
                seen.at = Some(at);
                if matches!(seen.value, Document::Int(_) | Document::Float(_)) {
                    seen.stored = false;
                }
            }
        }
        self.add_values(i, values, order, at);
    }

    fn add_values(
        &mut self,
        i: usize,
        values: Vec<Option<(Document, bool)>>,
        order: Order,
        at: RecordLocation,
    ) {
        let columns = &self.aggregate.columns;
        for ((state, value), column) in (self.list[i].columns.iter_mut()).zip(values).zip(columns) {
            let seen = value.map(|(value, stored)| Seen {
                value,
                order,
                stored,
                at: Some(at),
            });
            update(state, column.kind, seen);
        }
    }

    /// `n` matches of one group whose every column is a `count` (SPEC
    /// §107.5: a leaf counted whole), its values as stored.
    pub fn add_counted(
        &mut self,
        group: Vec<Document>,
        n: u64,
        order: Order,
        at: Option<RecordLocation>,
    ) {
        let first = (group.into_iter())
            .map(|value| Seen {
                value,
                order,
                stored: true,
                at,
            })
            .collect();
        let state = self.group_state(first);
        for state in &mut state.columns {
            if let State::Count(count) = state {
                *count += n;
            }
        }
    }

    fn add(
        &mut self,
        group: Vec<Document>,
        values: &[Option<Document>],
        stored: bool,
        order: Order,
        at: Option<RecordLocation>,
    ) {
        let first = (group.into_iter())
            .map(|value| Seen {
                value,
                order,
                stored,
                at,
            })
            .collect();
        let columns = &self.aggregate.columns;
        let state = self.group_state(first);
        for ((state, value), column) in (state.columns.iter_mut()).zip(values).zip(columns) {
            let seen = value.as_ref().map(|value| Seen {
                value: value.clone(),
                order,
                stored,
                at,
            });
            update(state, column.kind, seen);
        }
    }

    /// The group of the values in `first`, made if new; its first match
    /// by id kept.
    fn group_state(&mut self, first: Vec<Seen>) -> &mut Group {
        let i = self.group_index(first);
        &mut self.list[i]
    }

    fn group_index(&mut self, first: Vec<Seen>) -> usize {
        let key = GroupKey(first.iter().map(|seen| seen.value.clone()).collect());
        let columns = &self.aggregate.columns;
        let list = &mut self.list;
        let i = *self.groups.entry(key).or_insert_with(|| {
            list.push(Group {
                first: Vec::new(),
                columns: columns.iter().map(|c| initial(c.kind)).collect(),
            });
            list.len() - 1
        });
        self.last = Some(i);
        let group = &mut self.list[i];
        let earlier = group
            .first
            .first()
            .is_some_and(|f| f.order <= first[0].order);
        if group.first.is_empty() || (!first.is_empty() && !earlier) {
            group.first = first;
        }
        i
    }

    /// The rows (SPEC §107.2). A value that isn't as stored — a group's
    /// number, an extreme that is one — is read from its document
    /// through `read`, which gives the fields the aggregate reads.
    /// Then `having`, the sort, skip and limit.
    pub fn finish(
        self,
        mut read: impl FnMut(RecordLocation) -> std::io::Result<Document>,
    ) -> crate::Result<Vec<Document>> {
        let mut list: Vec<Option<Group>> = self.list.into_iter().map(Some).collect();
        let mut groups: Vec<Group> = (self.groups.into_values())
            .map(|i| list[i].take().expect("one key a group"))
            .collect();
        if groups.is_empty() && self.aggregate.group_by.is_empty() {
            groups.push(Group {
                first: Vec::new(),
                columns: self
                    .aggregate
                    .columns
                    .iter()
                    .map(|c| initial(c.kind))
                    .collect(),
            });
        }
        let mut as_stored = |seen: Seen, path: &Path| -> crate::Result<Document> {
            match (seen.stored, seen.at) {
                (true, _) | (false, None) => Ok(seen.value),
                (false, Some(at)) => Ok(path.value_or_null(&read(at)?).clone()),
            }
        };
        let mut rows = Vec::with_capacity(groups.len());
        for group in groups {
            let mut row = Document::Object(Map::new());
            for ((seen, path), name) in (group.first.into_iter())
                .zip(&self.group_paths)
                .zip(&self.aggregate.group_by)
            {
                let value = as_stored(seen, path)?;
                put(&mut row, name, value);
            }
            for ((state, column), path) in (group.columns.into_iter())
                .zip(&self.aggregate.columns)
                .zip(&self.column_paths)
            {
                let value = match state {
                    State::Count(n) => Document::Int(n as i64),
                    State::Sum(sum) => sum.sum(),
                    State::Avg(sum) => sum.avg(),
                    State::Min(seen) | State::Max(seen) => match seen {
                        Some(seen) => as_stored(seen, path.as_ref().expect("a path"))?,
                        None => Document::Null,
                    },
                };
                put(&mut row, &column.name, value);
            }
            rows.push(row);
        }
        Ok(self.aggregate.rows.apply(rows))
    }
}

fn initial(kind: Kind) -> State {
    match kind {
        Kind::Count => State::Count(0),
        Kind::Sum => State::Sum(ExactSum::default()),
        Kind::Avg => State::Avg(ExactSum::default()),
        Kind::Min => State::Min(None),
        Kind::Max => State::Max(None),
    }
}

/// One match's value into a column.
fn update(state: &mut State, kind: Kind, seen: Option<Seen>) {
    match state {
        State::Count(n) => *n += 1,
        State::Sum(sum) | State::Avg(sum) => {
            if let Some(seen) = seen {
                sum.add(&seen.value);
            }
        }
        State::Min(best) | State::Max(best) => {
            let Some(seen) = seen.filter(|seen| seen.value != Document::Null) else {
                return;
            };
            let replace = match best {
                None => true,
                Some(best) => extreme_order(&seen.value, &best.value, kind)
                    .then(seen.order.cmp(&best.order))
                    .is_lt(),
            };
            if replace {
                *best = Some(seen);
            }
        }
    }
}

/// Whether `a` is a better extreme than `b` (`Less`), for `min` or
/// `max`: ordered values by the sort's order, reversed for `max`, and
/// before every value nothing orders, which are all equal.
fn extreme_order(a: &Document, b: &Document, kind: Kind) -> Ordering {
    match (is_unordered(a), is_unordered(b), kind) {
        (false, false, Kind::Max) => sort_order(b, a, SortOrder::Asc),
        (false, false, _) => sort_order(a, b, SortOrder::Asc),
        (ua, ub, _) => ua.cmp(&ub),
    }
}

/// `value` into `row` at `path`, objects made on the way.
fn put(row: &mut Document, path: &str, value: Document) {
    let Document::Object(map) = row else {
        unreachable!("a row is an object");
    };
    match path.split_once('.') {
        None => {
            map.insert(path.to_string(), value);
        }
        Some((first, rest)) => {
            let inner = map
                .0
                .entry(first.to_string())
                .or_insert_with(|| Document::Object(Map::new()));
            put(inner, rest, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs(lines: &[&str]) -> Vec<Document> {
        (lines.iter().enumerate())
            .map(|(i, json)| {
                let mut doc: Document = json.parse().unwrap();
                if let Document::Object(map) = &mut doc {
                    map.0
                        .shift_insert(0, ID_FIELD.into(), Document::Id(DocId([i as u8 + 1; 16])));
                }
                doc
            })
            .collect()
    }

    fn rows(aggregate: &Aggregate, docs: Vec<Document>) -> Vec<String> {
        (aggregate.apply(docs).unwrap().iter())
            .map(Document::to_string)
            .collect()
    }

    /// SPEC §107.2: which documents are one group, and what a row shows.
    #[test]
    fn groups_are_values_a_filter_finds_equal() {
        let all = docs(&[
            r#"{"g":5,"x":1}"#,
            r#"{"g":5.0,"x":2}"#,
            r#"{"x":3}"#,
            r#"{"g":null,"x":4}"#,
            r#"{"g":"a"}"#,
            r#"{"g":"A"}"#,
            r#"{"g":[1,2]}"#,
            r#"{"g":[2,1]}"#,
            r#"{"g":[1,2]}"#,
        ]);
        let by_g = Aggregate::new().group_by("g").count("n");
        // Null (and missing) first, numbers, strings, unordered last.
        assert_eq!(
            rows(&by_g, all.clone()),
            [
                r#"{"g":null,"n":2}"#,
                r#"{"g":5,"n":2}"#,
                r#"{"g":"A","n":1}"#,
                r#"{"g":"a","n":1}"#,
                r#"{"g":[1,2],"n":2}"#,
                r#"{"g":[2,1],"n":1}"#,
            ]
        );
        // The first document by id gives the value, whatever the order.
        let mut reversed = all.clone();
        reversed.reverse();
        assert_eq!(rows(&by_g, reversed)[1], r#"{"g":5,"n":2}"#);
        // Nested paths, nested rows; several fields.
        let nested = docs(&[
            r#"{"o":{"p":1,"q":"x"}}"#,
            r#"{"o":{"p":1,"q":"y"}}"#,
            r#"{"o":{"p":1,"q":"x"}}"#,
        ]);
        let by_o = Aggregate::new().group_by("o.q").group_by("o.p").count("n");
        assert_eq!(
            rows(&by_o, nested),
            [
                r#"{"o":{"q":"x","p":1},"n":2}"#,
                r#"{"o":{"q":"y","p":1},"n":1}"#
            ]
        );
        // NaN is a group of its own bits.
        let nan = vec![
            Document::Object(
                [("g".to_string(), Document::Float(f64::NAN))]
                    .into_iter()
                    .collect(),
            ),
            Document::Object(
                [("g".to_string(), Document::Float(f64::NAN))]
                    .into_iter()
                    .collect(),
            ),
        ];
        assert_eq!(rows(&by_g, nan).len(), 1);
    }

    /// SPEC §107.2: each column's rules.
    #[test]
    fn columns_count_sum_average_and_find_extremes() {
        let all = docs(&[
            r#"{"v":1}"#,
            r#"{"v":2}"#,
            r#"{"v":"text"}"#,
            r#"{"v":null}"#,
            r#"{}"#,
            r#"{"v":[9]}"#,
        ]);
        let columns = Aggregate::new()
            .count("n")
            .sum("v", "sum")
            .avg("v", "avg")
            .min("v", "min")
            .max("v", "max");
        assert_eq!(
            rows(&columns, all),
            [r#"{"n":6,"sum":3,"avg":1.5,"min":1,"max":"text"}"#]
        );
        // No match: one row, as SQL's; nothing to average or compare.
        assert_eq!(
            rows(&columns, Vec::new()),
            [r#"{"n":0,"sum":0,"avg":null,"min":null,"max":null}"#]
        );
        // Values nothing orders count only where nothing else is there.
        let unordered = docs(&[r#"{"v":[2]}"#, r#"{"v":[1]}"#]);
        let extremes = Aggregate::new().min("v", "min").max("v", "max");
        assert_eq!(rows(&extremes, unordered), [r#"{"min":[2],"max":[2]}"#]);
        // Equal extremes: the first document's by id.
        let equal = docs(&[r#"{"v":5.0}"#, r#"{"v":5}"#]);
        assert_eq!(rows(&extremes, equal), [r#"{"min":5.0,"max":5.0}"#]);
        // Sums: `Int` while every value is one and the total fits.
        let sum = |lines: &[&str]| rows(&Aggregate::new().sum("v", "s"), docs(lines))[0].clone();
        assert_eq!(sum(&[r#"{"v":2}"#, r#"{"v":3}"#]), r#"{"s":5}"#);
        assert_eq!(sum(&[r#"{"v":2}"#, r#"{"v":0.5}"#]), r#"{"s":2.5}"#);
        assert_eq!(sum(&[r#"{"v":2.5}"#, r#"{"v":2.5}"#]), r#"{"s":5.0}"#);
        let max = i64::MAX;
        assert_eq!(
            sum(&[&format!(r#"{{"v":{max}}}"#), r#"{"v":1}"#]),
            format!(r#"{{"s":{}}}"#, Document::Float(max as f64 + 1.0))
        );
        assert_eq!(sum(&[r#"{"v":-0.0}"#]), r#"{"s":0.0}"#);
        // Past `f64::MAX` on the way, and back: exact in every order.
        let big = [r#"{"v":1e308}"#, r#"{"v":1e308}"#, r#"{"v":-1e308}"#];
        for order in [[0, 1, 2], [0, 2, 1], [2, 0, 1]] {
            let lines = order.map(|i| big[i]);
            assert_eq!(
                sum(&lines),
                r#"{"s":1e308}"#.replace("1e308", &Document::Float(1e308).to_string())
            );
        }
        let inf = Document::Float(f64::INFINITY);
        assert_eq!(sum(&big[..2]), format!(r#"{{"s":{inf}}}"#));
    }

    /// SPEC §107.4: a float sum is exact until rounded once, so every
    /// order gives one total.
    #[test]
    fn sums_are_exact_in_any_order() {
        let mut rng = crate::testing::XorShift(0x0107_5005);
        for _ in 0..300 {
            let n = 1 + rng.below(40);
            let values: Vec<f64> = (0..n)
                .map(|_| {
                    let mantissa = (rng.next() % (1 << 53)) as f64;
                    let exponent = rng.below(80) as i32 - 40;
                    let sign = if rng.below(2) == 0 { 1.0 } else { -1.0 };
                    sign * mantissa * 2f64.powi(exponent)
                })
                .collect();
            let total = |order: &[f64]| {
                let mut sum = ExactSum::default();
                for v in order {
                    sum.add(&Document::Float(*v));
                }
                sum.total()
            };
            let first = total(&values);
            let mut shuffled = values.clone();
            for i in (1..shuffled.len()).rev() {
                shuffled.swap(i, rng.below(i + 1));
            }
            assert_eq!(total(&shuffled).to_bits(), first.to_bits(), "{values:?}");
            shuffled.reverse();
            assert_eq!(total(&shuffled).to_bits(), first.to_bits(), "{values:?}");
        }
        // Known exact totals, as `math.fsum` gives them.
        let total = |values: &[f64]| {
            let mut sum = ExactSum::default();
            values
                .iter()
                .for_each(|v| _ = sum.add(&Document::Float(*v)));
            sum.total()
        };
        assert_eq!(total(&[0.1; 10]), 1.0);
        assert_eq!(total(&[0.1, 0.2, 0.3]), 0.6);
        assert_eq!(total(&[1e100, 1.0, -1e100, 1e-100]), 1.0);
        assert_eq!(total(&[5e-324, 5e-324]), 1e-323);
        assert_eq!(total(&[-5e-324]), -5e-324);
        assert_eq!(total(&[f64::MAX, f64::MAX, -f64::MAX]), f64::MAX);
        assert_eq!(total(&[-0.0, -0.0]).to_bits(), 0.0f64.to_bits());
        assert_eq!(total(&[1.0, 2f64.powi(-53)]), 1.0); // a tie, to even
        assert_eq!(
            total(&[1.0, 2f64.powi(-53), 2f64.powi(-80)]),
            1.0 + f64::EPSILON
        );
        // Integral values: the exact integer total, then rounded.
        let mut sum = ExactSum::default();
        for v in [1e16, 1.0, -1e16, 1.0] {
            sum.add(&Document::Float(v));
        }
        assert_eq!(sum.total(), 2.0);
        let mut sum = ExactSum::default();
        for v in [i64::MAX, i64::MAX, -1] {
            sum.add(&Document::Int(v));
        }
        assert_eq!(
            sum.sum(),
            Document::Float((2 * i64::MAX as i128 - 1) as f64)
        );
        // Infinities and NaN as IEEE adds them.
        let mut sum = ExactSum::default();
        for v in [f64::INFINITY, 1.0, f64::NEG_INFINITY] {
            sum.add(&Document::Float(v));
        }
        assert!(sum.total().is_nan());
    }

    /// SPEC §107.2: `having`, the row sort, skip and limit.
    #[test]
    fn rows_are_picked_sorted_and_cut() {
        let all = docs(&[
            r#"{"t":"a","v":1}"#,
            r#"{"t":"b","v":5}"#,
            r#"{"t":"b","v":1}"#,
            r#"{"t":"c","v":2}"#,
            r#"{"t":"c","v":2}"#,
            r#"{"t":"c","v":2}"#,
        ]);
        let per_t = || Aggregate::new().group_by("t").count("n").sum("v", "total");
        assert_eq!(
            rows(&per_t().having(Condition::gte("n", 2)), all.clone()),
            [
                r#"{"t":"b","n":2,"total":6}"#,
                r#"{"t":"c","n":3,"total":6}"#
            ]
        );
        assert_eq!(
            rows(
                &per_t().sort_desc("total").then_asc("n").limit(2),
                all.clone()
            ),
            [
                r#"{"t":"b","n":2,"total":6}"#,
                r#"{"t":"c","n":3,"total":6}"#
            ]
        );
        assert_eq!(
            rows(&per_t().sort_desc("n").skip(1), all),
            [
                r#"{"t":"b","n":2,"total":6}"#,
                r#"{"t":"a","n":1,"total":1}"#
            ]
        );
    }

    /// SPEC §107.2: what is refused before anything is read.
    #[test]
    fn a_wrong_aggregate_is_refused() {
        let refused = |aggregate: Aggregate| {
            let err = aggregate.apply(Vec::new()).unwrap_err();
            match &err {
                crate::Error::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
                other => panic!("{other:?}"),
            }
            err.to_string()
        };
        assert!(refused(Aggregate::new().count("n").count("n")).contains("two columns"));
        assert!(refused(Aggregate::new().group_by("t").count("t")).contains("as a group field"));
        assert!(refused(Aggregate::new().group_by("t.x").count("t")).contains("as a group field"));
        assert!(refused(Aggregate::new().count("")).contains("plain name"));
        assert!(refused(Aggregate::new().count("a.b")).contains("plain name"));
        assert!(refused(Aggregate::new().group_by("tags[*]")).contains("[*]"));
        assert!(refused(Aggregate::new().sum("", "s")).contains("name a field"));
        assert!(
            refused(Aggregate::new().group_by("a").group_by("a.b")).contains("one in the other")
        );
        assert!(refused(Aggregate::new().group_by("a").group_by("a")).contains("one in the other"));
        assert!(refused(Aggregate::new().count("n").sort_asc("m")).contains("`m` is no column"));
        assert!(
            refused(Aggregate::new().count("n").having(Condition::eq("v", 1)))
                .contains("`v` is no")
        );
        Aggregate::new()
            .group_by("a.b")
            .count("n")
            .sort_desc("a.b")
            .apply(Vec::new())
            .unwrap();
    }
}
