//! Aggregates (SPEC §107): the documents a filter finds, put in groups
//! by the values of some fields, and per group a count, a sum, a
//! minimum, a maximum or an average — SQL's `GROUP BY`, MongoDB's
//! `$group`. `Aggregate` says what; `Groups` keeps the groups as
//! matches come in, from their documents or from their index entries
//! (§107.5), and turns them into rows.

use crate::document::{DocId, Document, ID_FIELD, encode_document};
use crate::map::Map;
use crate::path::Path;
use crate::query::{
    Condition, Filter, Matcher, Panicked, SortOrder, canonical, is_unordered, sort_order,
};
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
///     .count().named("n")
///     .avg("tries").named("average_tries")
///     .max("created").named("newest")
///     // Rows whose column "n" is over 10.
///     .having(Condition::gt("n", 10))
///     .sort_desc("n")
///     .limit(5);
/// # let _ = per_status;
/// ```
///
/// Each row holds its group's values at their paths, then each column
/// in the order added, and no `_id`. A column is added by what it
/// computes, then `named` (an [`UnnamedColumn`]): the name is the key
/// its value gets in the row, as SQL's `AS` names a column. Without
/// `group_by`, every match is one group, and there is one row even for
/// no matches.
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
    /// Which of the group's documents the column takes in (SPEC §126);
    /// `None`: every one.
    pub condition: Option<Condition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

/// A column of an [`Aggregate`] that has no name yet (SPEC §134):
/// `count()`, `sum(path)`, `min(path)`, `max(path)` or `avg(path)`,
/// then `filter` if only some of the group's documents count, and
/// `named` last, which adds it and hands the `Aggregate` back. SQL
/// writes it in the same order: `count(*) FILTER (WHERE …) AS n`.
///
/// ```
/// use trunkdb::query::{Aggregate, Condition};
///
/// let per_team = Aggregate::new()
///     .group_by("team")
///     .count().named("tasks")
///     .count().filter(Condition::eq("status", "open")).named("open_tasks")
///     .avg("hours").named("mean_hours");
/// # let _ = per_team;
/// ```
///
/// A column without a name doesn't compile, as it isn't an
/// `Aggregate` yet:
///
/// ```compile_fail,E0599
/// use trunkdb::query::Aggregate;
///
/// let per_team = Aggregate::new().group_by("team").count().sum("hours");
/// ```
///
/// Nor does a condition with no column before it:
///
/// ```compile_fail,E0599
/// use trunkdb::query::{Aggregate, Condition};
///
/// let open = Aggregate::new().filter(Condition::eq("status", "open"));
/// ```
#[must_use = "a column is added to the aggregate only once it is `named`"]
#[derive(Debug, Clone)]
pub struct UnnamedColumn {
    aggregate: Aggregate,
    kind: Kind,
    path: Option<String>,
    condition: Option<Condition>,
}

impl UnnamedColumn {
    /// Lets this column take in only the documents that meet
    /// `condition` (SPEC §126). It filters this one column, not the
    /// aggregate: the other columns, and the groups, still see every
    /// document the filter finds. SQL's `count(*) FILTER (WHERE ...)`.
    /// Called again, the conditions are ANDed.
    pub fn filter(mut self, condition: Condition) -> Self {
        self.condition = Some(match self.condition.take() {
            None => condition,
            Some(earlier) => Condition::all([earlier, condition]),
        });
        self
    }

    /// Adds the column to the aggregate, under `name`, and hands the
    /// aggregate back. `name` is the key the value gets in each row, as
    /// SQL's `AS` names a column; `having` and the row sort refer to it.
    pub fn named(self, name: impl Into<String>) -> Aggregate {
        let mut aggregate = self.aggregate;
        aggregate.columns.push(Column {
            name: name.into(),
            kind: self.kind,
            path: self.path,
            condition: self.condition,
        });
        aggregate
    }
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

    /// A column counting the group's documents, to be `named`:
    /// `.count().named("n")` gives `{"n": 340}` for a group of 340.
    pub fn count(self) -> UnnamedColumn {
        self.column(Kind::Count, None)
    }

    /// A column summing the numbers at `path`, to be `named`:
    /// `.sum("tries").named("total")` gives `{"total": 1234}`, as SQL's
    /// `sum(tries) AS total`.
    pub fn sum(self, path: impl Into<String>) -> UnnamedColumn {
        self.column(Kind::Sum, Some(path.into()))
    }

    /// A column of the smallest value at `path`, to be `named`.
    pub fn min(self, path: impl Into<String>) -> UnnamedColumn {
        self.column(Kind::Min, Some(path.into()))
    }

    /// A column of the largest value at `path`, to be `named`.
    pub fn max(self, path: impl Into<String>) -> UnnamedColumn {
        self.column(Kind::Max, Some(path.into()))
    }

    /// A column of the average of the numbers at `path`, to be `named`.
    pub fn avg(self, path: impl Into<String>) -> UnnamedColumn {
        self.column(Kind::Avg, Some(path.into()))
    }

    fn column(self, kind: Kind, path: Option<String>) -> UnnamedColumn {
        UnnamedColumn {
            aggregate: self,
            kind,
            path,
            condition: None,
        }
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
            // In memory, a `custom` condition's panic goes on (SPEC §136.2).
            (groups.add_document(&doc, order, None)).unwrap_or_else(|panicked| panicked.resume());
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

    /// Each column's condition as a filter of its own (SPEC §126), for
    /// what a filter's conditions get: the fields it reads, whether an
    /// index's entries decide it. `None` for a column without one.
    pub(crate) fn column_filters(&self) -> Vec<Option<Filter>> {
        (self.columns.iter())
            .map(|c| c.condition.clone().map(|c| Filter::new().and(c)))
            .collect()
    }

    /// Whether some column has a condition.
    pub(crate) fn has_conditions(&self) -> bool {
        self.columns.iter().any(|c| c.condition.is_some())
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

    /// `avg`'s value: the exact total divided by the count, rounded
    /// once, so a mean of finite numbers is finite however large their
    /// total (SPEC §131); never `-0.0`. Infinities and NaNs as IEEE
    /// divides them.
    pub fn avg(&self) -> Document {
        match (self.n, self.special) {
            (0, _) => Document::Null,
            (n, Some(special)) => Document::Float(special / n as f64),
            (n, None) => Document::Float(self.wide.div_to_f64(n)),
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
        let (negative, magnitude) = self.sign_and_magnitude();
        rounded(negative, &magnitude, None)
    }

    /// Divided by `n`, then rounded once to the nearest float, ties to
    /// even (SPEC §131); zero is `0.0`.
    fn div_to_f64(&self, n: u64) -> f64 {
        let (negative, mut quotient) = self.sign_and_magnitude();
        // Long division, a limb at a time from the top: the remainder
        // stays below `n`, so it and the next limb fit in a `u128`.
        let mut remainder = 0u128;
        for limb in quotient.iter_mut().rev() {
            let current = (remainder << 64) | u128::from(*limb);
            *limb = (current / u128::from(n)) as u64;
            remainder = current % u128::from(n);
        }
        let rest = (remainder != 0).then(|| (2 * remainder).cmp(&u128::from(n)));
        rounded(negative, &quotient, rest)
    }

    /// Whether it is below zero, and how far from zero.
    fn sign_and_magnitude(&self) -> (bool, [u64; LIMBS]) {
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
        (negative, magnitude)
    }
}

/// `magnitude` units of 2^-1074, and `rest` of a unit more where a
/// division left one: how it compares to half a unit. Rounded to the
/// nearest float, ties to even, signed; zero is `0.0`.
fn rounded(negative: bool, magnitude: &[u64; LIMBS], rest: Option<Ordering>) -> f64 {
    let top = (0..LIMBS).rev().find(|&i| magnitude[i] != 0);
    let high = top.map(|top| top as u32 * 64 + (63 - magnitude[top].leading_zeros()));
    let bit = |at: u32| (magnitude[(at / 64) as usize] >> (at % 64)) & 1;
    let value = match high {
        None | Some(0..=52) => {
            // Below 2^53 units: a float's bits as they are (subnormal, or
            // the smallest exponent), and a unit more for a rest past
            // half of one, or half of one beside an odd unit.
            let mut units = magnitude[0];
            if rest == Some(Ordering::Greater) || (rest == Some(Ordering::Equal) && units & 1 == 1)
            {
                units += 1;
            }
            f64::from_bits(units)
        }
        Some(high) => {
            let shift = high - 52;
            let mut mantissa = (0..53).fold(0u64, |m, k| m | (bit(shift + k) << k));
            let round = bit(shift - 1) == 1;
            let sticky = rest.is_some() || (0..shift - 1).any(|at| bit(at) == 1);
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
        }
    };
    match negative && value != 0.0 {
        true => -value,
        false => value,
    }
}

/// A group's values, ordered as rows come (SPEC §107.2): by the order a
/// sort uses, and values no comparison orders by the encoding of their
/// canonical form (§124.4), so that values a filter finds equal are one
/// group and different ones two.
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
    /// Each column's condition, compiled once (SPEC §126.3).
    conditions: Vec<Option<Matcher<'a>>>,
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
            conditions: (aggregate.columns.iter())
                .map(|c| c.condition.as_ref().map(Matcher::of))
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
    /// `Err` if a column's `custom` condition panicked (SPEC §136.2).
    pub fn add_document(
        &mut self,
        doc: &Document,
        order: Order,
        at: Option<RecordLocation>,
    ) -> Result<(), Panicked> {
        let group: Vec<Document> = (self.group_paths.iter())
            .map(|path| path.value_or_null(doc).clone())
            .collect();
        let values: Vec<Option<Document>> = (self.column_paths.iter())
            .map(|path| path.as_ref().and_then(|p| p.field_value(doc)).cloned())
            .collect();
        // Which columns take this document in: those without a
        // condition, and those whose condition it meets (SPEC §126.2).
        let mut takes: Vec<bool> = Vec::with_capacity(self.conditions.len());
        for condition in &self.conditions {
            takes.push(match condition {
                Some(matcher) => matcher.matches(doc)?,
                None => true,
            });
        }
        self.add(group, &values, &takes, true, order, at);
        Ok(())
    }

    /// A match from its index entry (SPEC §107.5): `group` and `values`
    /// as `add_document` takes them, each with whether it is as stored.
    /// `values` holds `None` for `count` and for a missing value;
    /// `takes`, per column, whether the entry meets its condition (SPEC
    /// §126.4).
    pub fn add_entry(
        &mut self,
        group: Vec<(Document, bool)>,
        values: Vec<Option<(Document, bool)>>,
        takes: &[bool],
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
        self.add_values(i, values, takes, order, at);
    }

    /// A match from its index entry in the group the last match went to
    /// — its key's bytes up to the group's values the same (SPEC
    /// §107.5): `values` and `takes` as `add_entry` takes them.
    pub fn add_entry_to_last(
        &mut self,
        values: Vec<Option<(Document, bool)>>,
        takes: &[bool],
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
        self.add_values(i, values, takes, order, at);
    }

    fn add_values(
        &mut self,
        i: usize,
        values: Vec<Option<(Document, bool)>>,
        takes: &[bool],
        order: Order,
        at: RecordLocation,
    ) {
        let columns = &self.aggregate.columns;
        for (((state, value), column), take) in (self.list[i].columns.iter_mut())
            .zip(values)
            .zip(columns)
            .zip(takes)
        {
            if !take {
                continue;
            }
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
    /// §107.5: a leaf counted whole), its values as stored, counted in
    /// the columns `takes` says they meet the condition of (§126.4).
    pub fn add_counted(
        &mut self,
        group: Vec<Document>,
        n: u64,
        takes: &[bool],
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
        for (state, take) in state.columns.iter_mut().zip(takes) {
            if let (State::Count(count), true) = (state, take) {
                *count += n;
            }
        }
    }

    fn add(
        &mut self,
        group: Vec<Document>,
        values: &[Option<Document>],
        takes: &[bool],
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
        for (((state, value), column), take) in (state.columns.iter_mut())
            .zip(values)
            .zip(columns)
            .zip(takes)
        {
            if !take {
                continue;
            }
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
        // Arrays, objects and binary as a filter's `=` sees them: equal
        // ones one group (SPEC §124.4).
        let key = GroupKey(
            (first.iter())
                .map(|seen| match is_unordered(&seen.value) {
                    true => canonical(&seen.value),
                    false => seen.value.clone(),
                })
                .collect(),
        );
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

    /// SPEC §126.2: a column with a condition takes in only the group's
    /// documents that meet it; the groups, the row's values and the
    /// other columns see every one.
    #[test]
    fn a_column_takes_only_what_its_condition_meets() {
        let all = docs(&[
            r#"{"team": 5.0, "status": "done", "hours": 2}"#,
            r#"{"team": 5, "status": "open", "hours": 4}"#,
            r#"{"team": 5, "status": "open", "hours": 7.5}"#,
            r#"{"team": "b", "status": "done", "hours": 1, "tags": ["x"]}"#,
            r#"{"team": "b", "hours": null, "tags": ["y", "x"]}"#,
        ]);
        let open = || Condition::eq("status", "open");
        let long = || Condition::gte("hours", 4);
        let per_team = Aggregate::new()
            .group_by("team")
            .count()
            .named("tasks")
            .count()
            .filter(open())
            .named("open_tasks")
            .sum("hours")
            .filter(open())
            .named("open_hours")
            .avg("hours")
            .filter(long())
            .named("mean_long")
            .min("hours")
            .filter(long())
            .named("least_long")
            .max("hours")
            .filter(open())
            .named("most_open");
        assert_eq!(
            rows(&per_team, all.clone()),
            [
                // Team 5 shows its first document's 5.0, which is in no
                // column with a condition.
                r#"{"team":5.0,"tasks":3,"open_tasks":2,"open_hours":11.5,"mean_long":5.75,"least_long":4,"most_open":7.5}"#,
                // No document of team "b" is open or long: still a row.
                r#"{"team":"b","tasks":2,"open_tasks":0,"open_hours":0,"mean_long":null,"least_long":null,"most_open":null}"#,
            ]
        );

        // Two `filter`s on one column are ANDed; the column before keeps
        // its own. Either condition alone counts more: 2 open, 3 short.
        let both = Aggregate::new()
            .count()
            .filter(open())
            .named("open")
            .count()
            .filter(open())
            .filter(Condition::lt("hours", 5))
            .named("open_and_short");
        assert_eq!(
            rows(&both, all.clone()),
            [r#"{"open":2,"open_and_short":1}"#]
        );

        // Missing and null as a filter has them (SPEC §32), arrays through
        // `[*]`, a field no group or other column names.
        let fields = Aggregate::new()
            .count()
            .filter(Condition::is_null("status"))
            .named("no_status")
            .count()
            .filter(Condition::is_null("hours"))
            .named("null_hours")
            .count()
            .filter(Condition::eq("tags[*]", "x"))
            .named("tagged_x");
        assert_eq!(
            rows(&fields, all.clone()),
            [r#"{"no_status":1,"null_hours":1,"tagged_x":2}"#]
        );

        // Without `group_by` and no matches: still one row.
        assert_eq!(
            rows(&fields, Vec::new()),
            [r#"{"no_status":0,"null_hours":0,"tagged_x":0}"#]
        );
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
        let by_g = Aggregate::new().group_by("g").count().named("n");
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
        let by_o = Aggregate::new()
            .group_by("o.q")
            .group_by("o.p")
            .count()
            .named("n");
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
            .count()
            .named("n")
            .sum("v")
            .named("sum")
            .avg("v")
            .named("avg")
            .min("v")
            .named("min")
            .max("v")
            .named("max");
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
        let extremes = Aggregate::new().min("v").named("min").max("v").named("max");
        assert_eq!(rows(&extremes, unordered), [r#"{"min":[2],"max":[2]}"#]);
        // Equal extremes: the first document's by id.
        let equal = docs(&[r#"{"v":5.0}"#, r#"{"v":5}"#]);
        assert_eq!(rows(&extremes, equal), [r#"{"min":5.0,"max":5.0}"#]);
        // Sums: `Int` while every value is one and the total fits.
        let sum =
            |lines: &[&str]| rows(&Aggregate::new().sum("v").named("s"), docs(lines))[0].clone();
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

    /// SPEC §131: a mean is the exact total divided by the count, then
    /// rounded once. Finite numbers have a finite mean, however large
    /// their total, and the mean of a large total isn't rounded twice.
    #[test]
    fn means_are_the_exact_total_divided_and_rounded_once() {
        let mean = |values: &[f64]| {
            let mut sum = ExactSum::default();
            values
                .iter()
                .for_each(|v| _ = sum.add(&Document::Float(*v)));
            match sum.avg() {
                Document::Float(f) => f,
                other => panic!("{other:?}"),
            }
        };
        // Totals past f64::MAX: infinity, where the mean is finite.
        let max = f64::MAX;
        assert_eq!(mean(&[1e308, 1e308]), 1e308);
        assert_eq!(mean(&[max, max, max]), max);
        assert_eq!(mean(&[-max, -max]), -max);
        assert_eq!(mean(&[max, -max, max]), max / 3.0);
        assert_eq!(mean(&[1e308, 1e308, -1e308]), 1e308 / 3.0);
        // Below the smallest float: half of one is a tie, to even, and
        // a mean that rounds to zero is `0.0`.
        assert_eq!(mean(&[5e-324, 0.0]), 0.0);
        assert_eq!(mean(&[1.5e-323, 0.0]), 1e-323);
        assert_eq!(mean(&[1.5e-323, 0.0, 0.0]), 5e-324);
        assert_eq!(mean(&[-5e-324, 0.0]).to_bits(), 0.0f64.to_bits());
        // Just above the subnormals the quotient's last bit is the one
        // rounded on: a third of a unit past it, left by the division
        // alone, rounds up, where it would tie and round to even.
        let past_a_tie = f64::from_bits(0x0038_0000_0000_0001); // 3 * 2^53 + 4 units
        assert_eq!(
            mean(&[past_a_tie, 0.0, 0.0]).to_bits(),
            0x0020_0000_0000_0001
        );
        // The total rounded first, then divided, was a unit off: means
        // as Python's `Fraction` gives them.
        for (values, expected) in [
            (
                &[0x41f50f0218270ee8, 0x420da917d65a1c44, 0x42b76d4e0c4d1583][..],
                0x429f431ad48c844a,
            ),
            (
                &[
                    0x4167563a05110792,
                    0x412af4a9084f854a,
                    0x3effe62956aaf56a,
                    0xc12bdbb2dc3eb5dd,
                    0x43207d8ba6caf1a9,
                ],
                0x42fa62790d3249ff,
            ),
            (
                &[
                    0xbc395f27564dcbb2,
                    0x40d145bbac604bed,
                    0xbed85ebed2591610,
                    0x3e133b81a24db572,
                    0xc27c04b7777af4dc,
                    0xc234534ff0e2139a,
                    0xbc8c7cf28e8d5a1c,
                ],
                0xc250bc871cb21661,
            ),
        ] {
            let values: Vec<f64> = values.iter().map(|&b| f64::from_bits(b)).collect();
            assert_eq!(mean(&values).to_bits(), expected, "{values:?}");
        }
        // A count that is a power of two divides a normal total exactly:
        // the mean is the rounded total, halved as often.
        let mut rng = crate::testing::XorShift(0x0131_0a5e);
        for _ in 0..300 {
            let n = 1 << rng.below(6);
            let values: Vec<f64> = (0..n)
                .map(|_| {
                    let mantissa = (rng.next() % (1 << 53)) as f64;
                    let sign = if rng.below(2) == 0 { 1.0 } else { -1.0 };
                    sign * mantissa * 2f64.powi(rng.below(400) as i32 - 200)
                })
                .collect();
            let mut sum = ExactSum::default();
            values
                .iter()
                .for_each(|v| _ = sum.add(&Document::Float(*v)));
            let expected = sum.total() / n as f64;
            assert_eq!(mean(&values).to_bits(), expected.to_bits(), "{values:?}");
        }
        // Infinities and NaN as IEEE divides them.
        assert_eq!(mean(&[f64::INFINITY, 1.0]), f64::INFINITY);
        assert!(mean(&[f64::INFINITY, f64::NEG_INFINITY]).is_nan());
        // Through `Aggregate`, as the robustness review asked it.
        let aggregate = Aggregate::new().avg("n").named("mean");
        let docs = docs(&[r#"{"n": 1e308}"#, r#"{"n": 1e308}"#]);
        assert_eq!(rows(&aggregate, docs), [r#"{"mean":1e+308}"#]);
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
        let per_t = || {
            Aggregate::new()
                .group_by("t")
                .count()
                .named("n")
                .sum("v")
                .named("total")
        };
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
        assert!(
            refused(Aggregate::new().count().named("n").count().named("n")).contains("two columns")
        );
        assert!(
            refused(Aggregate::new().group_by("t").count().named("t")).contains("as a group field")
        );
        assert!(
            refused(Aggregate::new().group_by("t.x").count().named("t"))
                .contains("as a group field")
        );
        assert!(refused(Aggregate::new().count().named("")).contains("plain name"));
        assert!(refused(Aggregate::new().count().named("a.b")).contains("plain name"));
        assert!(refused(Aggregate::new().group_by("tags[*]")).contains("[*]"));
        assert!(refused(Aggregate::new().sum("").named("s")).contains("name a field"));
        assert!(
            refused(Aggregate::new().group_by("a").group_by("a.b")).contains("one in the other")
        );
        assert!(refused(Aggregate::new().group_by("a").group_by("a")).contains("one in the other"));
        assert!(
            refused(Aggregate::new().count().named("n").sort_asc("m")).contains("`m` is no column")
        );
        assert!(
            refused(
                Aggregate::new()
                    .count()
                    .named("n")
                    .having(Condition::eq("v", 1))
            )
            .contains("`v` is no")
        );
        Aggregate::new()
            .group_by("a.b")
            .count()
            .named("n")
            .sort_desc("a.b")
            .apply(Vec::new())
            .unwrap();
    }
}
