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
            results.sort_by(|a, b| {
                let (a, b) = (doc_of(a), doc_of(b));
                let ord = match (field_value(a, &sort.field), field_value(b, &sort.field)) {
                    (Some(a), Some(b)) => compare(a, b).unwrap_or(std::cmp::Ordering::Equal),
                    // A document missing the sort field, or a value that
                    // isn't comparable to the other side's, doesn't error
                    // out — it just doesn't move relative to what it's
                    // being compared against.
                    _ => std::cmp::Ordering::Equal,
                };
                match sort.order {
                    SortOrder::Asc => ord,
                    SortOrder::Desc => ord.reverse(),
                }
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

fn condition_matches(cond: &Condition, doc: &Document) -> bool {
    let Some(field_value) = field_value(doc, &cond.field) else {
        return false;
    };
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

fn compare(a: &Document, b: &Document) -> Option<std::cmp::Ordering> {
    use Document::*;
    match (a, b) {
        (Int(x), Int(y)) => x.partial_cmp(y),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => (*x as f64).partial_cmp(y),
        (Float(x), Int(y)) => x.partial_cmp(&(*y as f64)),
        (String(x), String(y)) => x.partial_cmp(y),
        (Bool(x), Bool(y)) => x.partial_cmp(y),
        _ => None,
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
        assert!(
            !Filter {
                conditions: vec![Condition {
                    field: "address.city".into(),
                    op: Op::Ne,
                    value: berlin(),
                }],
                ..Default::default()
            }
            .matches(&doc(&[("address", Document::Null)])),
            "a missing path matches nothing, not even `Ne` — like a missing field"
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

    #[test]
    fn sort_treats_missing_field_as_equal_rather_than_erroring() {
        let docs = vec![
            doc(&[("age", Document::Int(5))]),
            doc(&[("name", Document::String("no age field".into()))]),
        ];

        let filter = Filter {
            sort: Some(Sort {
                field: "age".into(),
                order: SortOrder::Asc,
            }),
            ..Default::default()
        };

        // Just needs to not panic and to return both documents — exact
        // relative order for the missing-field case isn't the contract.
        assert_eq!(filter.apply(docs).len(), 2);
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
                cond("name", Op::Eq, Document::Null),
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
