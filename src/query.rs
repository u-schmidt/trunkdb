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

#[derive(Debug, Clone)]
pub struct Condition {
    pub field: String,
    pub op: Op,
    pub value: Document,
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

/// v0's entire query language: a flat AND of comparisons plus an
/// optional sort-by-field and limit — still no OR, no nesting. Evaluated
/// by scanning, or over one secondary index's range when a condition
/// allows it (`index_range`, SPEC §28.4).
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub conditions: Vec<Condition>,
    pub sort: Option<Sort>,
    pub limit: Option<usize>,
}

impl Filter {
    pub fn matches(&self, doc: &Document) -> bool {
        self.conditions.iter().all(|c| condition_matches(c, doc))
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
}

impl Filter {
    /// Picks the index `find` reads from, and the range of it, among
    /// `indexes` — `None` for a full scan. Rule-based, not cost-based:
    /// the first indexed field with an `Eq` condition, else the first
    /// with any range condition (`Lt`/`Lte`/`Gt`/`Gte`); all of that
    /// field's range conditions are intersected, so `a >= 10 AND a <= 20`
    /// reads just that stretch. `Ne` and `Contains` never use an index.
    /// The range may include documents the filter rejects, never the
    /// other way around.
    pub(crate) fn index_range<'a>(
        &self,
        indexes: &'a [IndexMeta],
    ) -> Option<(&'a IndexMeta, KeyRange)> {
        let usable = |c: &&Condition| {
            key::range_for(&c.op, &c.value).is_some() && indexes.iter().any(|i| i.field == c.field)
        };
        let chosen = self
            .conditions
            .iter()
            .filter(usable)
            .find(|c| matches!(c.op, Op::Eq))
            .or_else(|| self.conditions.iter().find(usable))?;
        let index = indexes.iter().find(|i| i.field == chosen.field)?;
        let range = self
            .conditions
            .iter()
            .filter(|c| c.field == chosen.field)
            .filter_map(|c| key::range_for(&c.op, &c.value))
            .reduce(KeyRange::intersect)?;
        Some((index, range))
    }
}

impl Filter {
    /// The index `find` reads in sort order (SPEC §34.2) — for a filter
    /// with a `sort` and a `limit`, on a field with an index, and no `Eq`
    /// condition another index could answer (a few documents found by
    /// value beat walking in order). With it, that field's own range
    /// conditions narrowed to one range, or `None` for the whole index.
    pub(crate) fn index_order<'a>(
        &self,
        indexes: &'a [IndexMeta],
    ) -> Option<(&'a IndexMeta, Option<KeyRange>)> {
        let sort = self.sort.as_ref()?;
        self.limit?;
        let index = indexes.iter().find(|i| i.field == sort.field)?;
        let eq_elsewhere = self.conditions.iter().any(|c| {
            matches!(c.op, Op::Eq)
                && c.field != sort.field
                && key::range_for(&c.op, &c.value).is_some()
                && indexes.iter().any(|i| i.field == c.field)
        });
        if eq_elsewhere {
            return None;
        }
        let range = self
            .conditions
            .iter()
            .filter(|c| c.field == sort.field)
            .filter_map(|c| key::range_for(&c.op, &c.value))
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

fn condition_matches(cond: &Condition, doc: &Document) -> bool {
    let field_value = value_or_null(doc, &cond.field);
    if let Op::Contains = cond.op {
        return match (field_value, &cond.value) {
            (Document::String(haystack), Document::String(needle)) => {
                fold_case(haystack).contains(&fold_case(needle))
            }
            _ => false,
        };
    }
    let ord = compare(field_value, &cond.value);
    match (&cond.op, ord) {
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

    #[test]
    fn and_of_comparisons() {
        let filter = Filter {
            conditions: vec![
                Condition {
                    field: "age".into(),
                    op: Op::Gte,
                    value: Document::Int(18),
                },
                Condition {
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
            conditions: vec![Condition {
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
            conditions: vec![Condition {
                field: "n".into(),
                op: Op::Contains,
                value: Document::Int(1),
            }],
            ..Default::default()
        };
        assert!(!non_string_needle.matches(&doc(&[("n", Document::String("1".into()))])));
    }

    fn city_is(path: &str, city: &str) -> Filter {
        Filter {
            conditions: vec![Condition {
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
            conditions: vec![Condition {
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
        let mut on_path = when(Op::Eq, Document::Null);
        on_path.conditions[0].field = "address.city".into();
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
            conditions: vec![Condition {
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
        Condition {
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
        let field_of = |f: &Filter| f.index_range(&indexes).map(|(i, _)| i.field.clone());

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
        let (_, range) = f.index_range(&indexes).unwrap();
        let k = |n| key::secondary(&Document::Int(n), crate::DocId([0; 16])).unwrap();
        assert!(range.contains(&k(15)) && !range.contains(&k(9)) && !range.contains(&k(21)));
    }
}
