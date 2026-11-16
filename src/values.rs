//! How two values compare (SPEC §32, §34.1, §124): equality as a
//! filter's `eq` sees it, the one total order sorts and indexes share,
//! the form values that are equal take for grouping, and an operator
//! held against one value. Filters (`query`), indexes, updates and live
//! queries all decide by these.

use crate::document::Document;
use crate::map::Map;
use crate::query::{Op, SortOrder};

/// `op` against `value` for any of `values` — `Ne` for none being equal.
pub(crate) fn any_matches<'a>(
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
pub(crate) fn value_matches(op: &Op, field_value: &Document, value: &Document) -> bool {
    if let Op::StartsWith = op {
        return match (field_value, value) {
            (Document::String(text), Document::String(prefix)) => text.starts_with(prefix.as_str()),
            (Document::Binary(bytes), Document::Binary(prefix)) => bytes.starts_with(prefix),
            _ => false,
        };
    }
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

/// An array or an object: a value `=` and `!=` compare by its contents
/// and nothing orders (SPEC §124). Binary data was one too, until it
/// was ordered by its bytes (SPEC §166).
pub(crate) fn is_whole(value: &Document) -> bool {
    matches!(value, Document::Array(_) | Document::Object(_))
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
/// answer. Not for `Op::Contains` or `Op::StartsWith`.
pub(crate) fn holds(op: &Op, ord: Option<std::cmp::Ordering>) -> bool {
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
pub(crate) fn fold_case(s: &str) -> String {
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
        // In calendar order, and in the day's; a date never with a
        // date-time, which is a moment (SPEC §173).
        (Date(x), Date(y)) => Some(x.cmp(y)),
        (Time(x), Time(y)) => Some(x.cmp(y)),
        // Byte by byte, as SQLite compares BLOBs (SPEC §166).
        (Binary(x), Binary(y)) => Some(x.cmp(y)),
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
        Document::Binary(_) => 6,
        Document::Date(_) => 7,
        Document::Time(_) => 8,
        _ => UNORDERED,
    }
}

const UNORDERED: u8 = 9;

/// Whether `sort_order` puts `value` among the values nothing orders —
/// arrays, objects, NaN. No index holds them.
pub(crate) fn is_unordered(value: &Document) -> bool {
    sort_rank(value) == UNORDERED
}

/// The order a sort puts two field values in (SPEC §34.1) — a total
/// order, so any sort is well-defined: null (and missing) before bools
/// before numbers before strings before ids (SPEC §62) before date-times
/// (SPEC §69) before binary (SPEC §166), each by value, reversed for
/// `Desc`;
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
    use crate::DocId;

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
