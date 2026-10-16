//! A field path taken apart once (SPEC §102.1): `status`,
//! `address.city`, `tags[*]`, `items[*].sku` (§31, §42). A filter parses
//! each of its paths when a read starts, not on every document it checks.

use crate::document::Document;
use indexmap::IndexMap;

/// Up to this many fields an object is searched for a name in order,
/// beyond it by hash (SPEC §102.3): comparing a handful of short names
/// beats SipHash, which `Document::Object`'s map uses.
const SCAN_UP_TO: usize = 8;

/// The field `name` of `map` (SPEC §102.3).
pub(crate) fn get<'d>(map: &'d IndexMap<String, Document>, name: &str) -> Option<&'d Document> {
    #[cfg(test)]
    LOOKUPS.with(|n| n.set(n.get() + 1));
    if map.len() <= SCAN_UP_TO {
        map.iter()
            .find_map(|(key, value)| (key == name).then_some(value))
    } else {
        map.get(name)
    }
}

/// The field `name` of `map`, to change; made a `Null` first if missing.
pub(crate) fn get_or_insert<'d>(
    map: &'d mut IndexMap<String, Document>,
    name: &str,
) -> &'d mut Document {
    let at = match map.len() <= SCAN_UP_TO {
        true => map.keys().position(|key| key == name),
        false => map.get_index_of(name),
    };
    let at = at.unwrap_or_else(|| map.insert_full(name.to_string(), Document::Null).0);
    map.get_index_mut(at).expect("just found or made").1
}

/// A path, parsed (SPEC §102.1). The common one, a single field name, is
/// held as it is, without an allocation.
#[derive(Debug, Clone)]
pub(crate) enum Path<'p> {
    /// One field of the document: no `.`, no `[*]`, not empty.
    Field(&'p str),
    /// Anything else, step by step. `multi` if a `[*]` is anywhere in
    /// it, as `query::is_multi` says.
    Steps { steps: Vec<Step<'p>>, multi: bool },
}

/// One step of a path, between dots.
#[derive(Debug, Clone)]
pub(crate) struct Step<'p> {
    /// As written: what `field_value` looks up, `[*]` and all.
    raw: &'p str,
    /// Without the `[*]`s at its end: what `values` looks up.
    name: &'p str,
    /// How many `[*]` end it: each goes on with every element of the
    /// array there.
    fan_outs: usize,
}

impl<'p> Path<'p> {
    pub fn new(path: &'p str) -> Self {
        #[cfg(test)]
        PATHS_PARSED.with(|n| n.set(n.get() + 1));
        if !path.is_empty() && !path.contains('.') && !path.contains("[*]") {
            return Path::Field(path);
        }
        let steps = path
            .split('.')
            .map(|raw| {
                let name = raw.trim_end_matches("[*]");
                Step {
                    raw,
                    name,
                    fan_outs: (raw.len() - name.len()) / 3,
                }
            })
            .collect();
        Path::Steps {
            steps,
            multi: path.contains("[*]"),
        }
    }

    /// Whether it reaches any number of values, not one (SPEC §42).
    pub fn is_multi(&self) -> bool {
        matches!(self, Path::Steps { multi: true, .. })
    }

    /// The value at the path in `doc` (SPEC §31), `[*]` taken as part of a
    /// name — `None` if a step is missing or isn't an object. The empty
    /// path is `doc` itself (§46.1).
    pub fn field_value<'d>(&self, doc: &'d Document) -> Option<&'d Document> {
        match self {
            Path::Field(name) => match doc {
                Document::Object(map) => get(map, name),
                _ => None,
            },
            Path::Steps { steps, .. } => match &steps[..] {
                [only] if only.raw.is_empty() => Some(doc),
                steps => steps.iter().try_fold(doc, |value, step| match value {
                    Document::Object(map) => get(map, step.raw),
                    _ => None,
                }),
            },
        }
    }

    /// The value a condition or sort sees: a missing field reads as
    /// `Null` (SPEC §32).
    pub fn value_or_null<'d>(&self, doc: &'d Document) -> &'d Document {
        self.field_value(doc).unwrap_or(&Document::Null)
    }

    /// Every value at the path (SPEC §42): `query::values_at`.
    pub fn values<'d>(&self, doc: &'d Document) -> Vec<&'d Document> {
        self.walk(doc, true)
    }

    /// The values actually stored at the path (SPEC §45.1):
    /// `query::present_at`.
    pub fn present<'d>(&self, doc: &'d Document) -> Vec<&'d Document> {
        self.walk(doc, false)
    }

    /// `values`, or with `missing_is_null` false `present`. A step
    /// leading with an empty name starts at `doc` itself; each `[*]`
    /// goes on with the elements of the arrays there, and nothing else.
    fn walk<'d>(&self, doc: &'d Document, missing_is_null: bool) -> Vec<&'d Document> {
        let missing = missing_is_null.then_some(&Document::Null);
        let steps = match self {
            Path::Field(name) => {
                let value = match doc {
                    Document::Object(map) => get(map, name).or(missing),
                    _ => missing,
                };
                return value.into_iter().collect();
            }
            Path::Steps { steps, .. } => steps,
        };
        let mut values = vec![doc];
        for (i, step) in steps.iter().enumerate() {
            if i > 0 || !step.name.is_empty() {
                values = values
                    .into_iter()
                    .filter_map(|value| match value {
                        Document::Object(map) => get(map, step.name).or(missing),
                        _ => missing,
                    })
                    .collect();
            }
            for _ in 0..step.fan_outs {
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

    /// The value at the path in `map`, made if missing, with objects on
    /// the way, for an index entry's document (SPEC §100.2, §102.4).
    /// Whatever is on the way and isn't an object is replaced by one: a
    /// path inside another named one is ruled out (`Covering::new`).
    pub fn slot<'d>(&self, map: &'d mut IndexMap<String, Document>) -> &'d mut Document {
        let steps = match self {
            Path::Field(name) => return get_or_insert(map, name),
            Path::Steps { steps, .. } => steps,
        };
        let (last, before) = steps.split_last().expect("split gives a step");
        let mut map = map;
        for step in before {
            let value = get_or_insert(map, step.raw);
            if !matches!(value, Document::Object(_)) {
                *value = Document::Object(IndexMap::new());
            }
            map = match value {
                Document::Object(inner) => inner,
                _ => unreachable!("just made an object"),
            };
        }
        get_or_insert(map, last.raw)
    }
}

#[cfg(test)]
thread_local! {
    /// Paths parsed, and fields looked up in an object (SPEC §102), for
    /// tests.
    pub(crate) static PATHS_PARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
