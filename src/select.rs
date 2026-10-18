//! The fields a read hands out (SPEC §106): `Filter::select`'s paths,
//! parsed once per read into a tree. A document is cut down to it after
//! the filter, sort, skip and limit have run: in memory (`project`), from
//! its encoding without building the rest (`decode`), or from its index
//! entry without reading it (`Answer`).
//!
//! What a selection hands out (§106.2): the document's `_id`, always,
//! first; then each selected path, in the order the selection first
//! names it, nested as stored. A path stops where its value does: it
//! doesn't walk into an array, a scalar or a missing field, and an
//! object with nothing selected under it is left out. A missing field is
//! left out; a stored `null` is handed out. A step ending in `[*]`
//! selects its array whole, and the empty path the whole document.

use crate::decode::{take_u8, take_u32};
use crate::document::{
    DocId, Document, ID_FIELD, TAG_OBJECT, decode_document, decode_value, encodes_object,
    skip_value, take_str, with_id,
};
use crate::index::key;
use indexmap::IndexMap;

/// What is selected at one place in a document.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// The value whole.
    Whole,
    /// Some of an object's fields, in the order first named.
    Fields(Vec<(String, Node)>),
}

/// `Filter::select`'s paths as a tree (SPEC §106.4).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Selection {
    root: Node,
}

impl Selection {
    /// The tree of `paths`. A path inside one selected whole adds
    /// nothing; `_id`, handed out anyway, adds nothing either.
    pub fn new<'p>(paths: impl IntoIterator<Item = &'p str>) -> Self {
        let mut root = Node::Fields(Vec::new());
        for path in paths {
            add(&mut root, path);
        }
        Selection { root }
    }

    /// Whether it selects the whole document (the empty path).
    pub fn is_whole(&self) -> bool {
        self.root == Node::Whole
    }

    /// `doc` cut down to the selection: what `Filter::apply` hands out.
    /// A document that isn't an object (only damage since §95) is left
    /// as it is, as a read decodes it whole.
    pub fn project(&self, doc: Document) -> Document {
        let Node::Fields(fields) = &self.root else {
            return doc;
        };
        let Document::Object(mut map) = doc else {
            return doc;
        };
        let mut out = IndexMap::with_capacity(fields.len() + 1);
        if let Some(id) = map.swap_remove(ID_FIELD) {
            out.insert(ID_FIELD.to_string(), id);
        }
        take(&mut map, fields, &mut out);
        Document::Object(out)
    }

    /// The selection of a stored document, `id` as its `_id`, from its
    /// encoding (`document::encode_stored`): the fields not selected
    /// skipped without being built, with every check a whole decode makes
    /// (SPEC §99.2), so the selection is damaged exactly where the whole
    /// document is. A stored `_id` (files before §59) is passed over, as
    /// `with_id` replaces it.
    pub fn decode(&self, encoded: &[u8], id: DocId) -> std::io::Result<Document> {
        let Node::Fields(fields) = &self.root else {
            return Ok(with_id(decode_document(encoded)?.0, id));
        };
        if !encodes_object(encoded) {
            return Ok(with_id(decode_document(encoded)?.0, id));
        }
        let mut out = IndexMap::with_capacity(fields.len() + 1);
        out.insert(ID_FIELD.to_string(), Document::Id(id));
        // `_id` alone still walks the document: damage reads as damage.
        let mut bytes = &encoded[1..];
        decode_object(
            &mut bytes,
            fields,
            Document::MAX_NESTING - 1,
            true,
            &mut out,
        )?;
        Ok(Document::Object(out))
    }

    /// The paths of the values selected whole, in the order handed out:
    /// what an index must hold to answer it (`Answer`).
    fn leaves(&self) -> Option<Vec<String>> {
        fn walk(fields: &[(String, Node)], prefix: &str, out: &mut Vec<String>) {
            for (name, node) in fields {
                let path = match prefix {
                    "" => name.clone(),
                    _ => format!("{prefix}.{name}"),
                };
                match node {
                    Node::Whole => out.push(path),
                    Node::Fields(inner) => walk(inner, &path, out),
                }
            }
        }
        let Node::Fields(fields) = &self.root else {
            return None;
        };
        let mut out = Vec::new();
        walk(fields, "", &mut out);
        Some(out)
    }
}

/// Adds `path` to the tree at `node`.
fn add(node: &mut Node, path: &str) {
    if path.is_empty() || path.starts_with("[*]") {
        *node = Node::Whole; // the document itself (§46.1)
        return;
    }
    let mut steps = path.split('.').peekable();
    let mut at = node;
    let mut top = true;
    while let Some(step) = steps.next() {
        let Node::Fields(fields) = at else {
            return; // inside a value selected whole already
        };
        // A step ending in `[*]` reaches into an array: the array is
        // selected whole (SPEC §106.2).
        let name = step.trim_end_matches("[*]");
        let fans_out = name.len() < step.len();
        if top && name == ID_FIELD {
            return; // handed out anyway, from the cell
        }
        top = false;
        let last = fans_out || steps.peek().is_none();
        let i = match fields.iter().position(|(field, _)| field == name) {
            Some(i) => i,
            None => {
                fields.push((name.to_string(), Node::Fields(Vec::new())));
                fields.len() - 1
            }
        };
        if last {
            fields[i].1 = Node::Whole;
            return;
        }
        at = &mut fields[i].1;
    }
}

/// `fields` of `map`, moved into `out` in the selection's order.
fn take(
    map: &mut IndexMap<String, Document>,
    fields: &[(String, Node)],
    out: &mut IndexMap<String, Document>,
) {
    for (name, node) in fields {
        let Some(value) = map.swap_remove(name) else {
            continue;
        };
        match (node, value) {
            (Node::Whole, value) => {
                out.insert(name.clone(), value);
            }
            (Node::Fields(inner), Document::Object(mut map)) => {
                let mut selected = IndexMap::with_capacity(inner.len());
                take(&mut map, inner, &mut selected);
                if !selected.is_empty() {
                    out.insert(name.clone(), Document::Object(selected));
                }
            }
            _ => {} // a path that stops before its end
        }
    }
}

/// An encoded object's `fields` into `out`, in the selection's order,
/// `bytes` at its length (after the tag). `levels` as for
/// `decode_value`: how many more objects or arrays may open inside it.
/// `top`: the document itself, whose stored `_id` is passed over.
fn decode_object(
    bytes: &mut &[u8],
    fields: &[(String, Node)],
    levels: usize,
    top: bool,
    out: &mut IndexMap<String, Document>,
) -> std::io::Result<()> {
    let mut slots: Vec<Option<Document>> = vec![None; fields.len()];
    let count = take_u32(bytes, "an object's length")?;
    for _ in 0..count {
        let key = take_str(bytes, "a key")?;
        let at = match top && key == ID_FIELD {
            true => None,
            false => fields.iter().position(|(name, _)| name == key),
        };
        let Some(at) = at else {
            skip_value(bytes, levels)?;
            continue;
        };
        slots[at] = match &fields[at].1 {
            Node::Whole => Some(decode_value(bytes, levels)?),
            Node::Fields(inner) => decode_inner(bytes, inner, levels)?,
        };
    }
    for ((name, _), slot) in fields.iter().zip(slots) {
        if let Some(value) = slot {
            out.insert(name.clone(), value);
        }
    }
    Ok(())
}

/// One value selected in part: an object's `fields`, `None` if it isn't
/// an object or has none of them — skipped, with its checks, either way.
fn decode_inner(
    bytes: &mut &[u8],
    fields: &[(String, Node)],
    levels: usize,
) -> std::io::Result<Option<Document>> {
    // An object too deep to open is skipped: `skip_value` reports it.
    if bytes.first() != Some(&TAG_OBJECT) || levels == 0 {
        skip_value(bytes, levels)?;
        return Ok(None);
    }
    take_u8(bytes, "a document's type tag")?;
    let mut inner = IndexMap::with_capacity(fields.len());
    decode_object(bytes, fields, levels - 1, false, &mut inner)?;
    Ok((!inner.is_empty()).then_some(Document::Object(inner)))
}

/// A selection answered from index entries (SPEC §106.5): where every
/// selected path is one of the index's fields, an entry whose selected
/// values are each handed out as stored gives the result without its
/// document. Owned, so a cursor can keep it.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    selection: Selection,
    /// The index's fields: how many values a key has.
    fields: usize,
    /// For each leaf of the selection, in order, the index field that
    /// holds it.
    from: Vec<usize>,
}

impl Answer {
    /// For entries of an index on `fields` (none: the primary index),
    /// whose entries decide the read's filter — `None` unless the index
    /// holds every selected path, exactly as written (`o.p`, not `o` or
    /// `o.p.q`, §100.1). Never a multikey index: its entries decide
    /// nothing (`Covering`), and a selected path has no `[*]`.
    pub fn new(selection: &Selection, fields: &[String]) -> Option<Self> {
        let from = (selection.leaves()?.iter())
            .map(|leaf| fields.iter().position(|field| field == leaf))
            .collect::<Option<Vec<usize>>>()?;
        Some(Answer {
            selection: selection.clone(),
            fields: fields.len(),
            from,
        })
    }

    /// The result `key` tells of — `None` where a selected value can't
    /// be handed out from the key, and the document must be read. Only a
    /// bool, a string that fits the key, an id and a date-time can: a
    /// number is keyed as a float, `5` and `5.0` alike, and `-0.0` as
    /// `0.0`; a null is a missing field too (§32); a cut string is only
    /// its start; arrays, objects and binary aren't held at all.
    pub fn entry(&self, key: &[u8]) -> Option<Document> {
        // The selected values, in the leaves' order; nothing for `_id`
        // alone.
        let mut values: Vec<Option<Document>> = Vec::with_capacity(self.from.len());
        if !self.from.is_empty() {
            values.resize(self.from.len(), None);
            let mut parts = key::each_part(key::value_part(key));
            for i in 0..self.fields {
                let part = parts.next()?;
                for (slot, _) in values
                    .iter_mut()
                    .zip(&self.from)
                    .filter(|(_, at)| **at == i)
                {
                    let mut value = Document::Null;
                    key::decode_part_into(part, self.fields, &mut value)?;
                    if !matches!(
                        value,
                        Document::Bool(_)
                            | Document::String(_)
                            | Document::Id(_)
                            | Document::DateTime(_)
                    ) {
                        return None;
                    }
                    *slot = Some(value);
                }
            }
            if parts.next().is_some() {
                return None; // a damaged key with more values than fields
            }
        }
        let Node::Fields(fields) = &self.selection.root else {
            unreachable!("an answer selects fields");
        };
        let mut out = IndexMap::with_capacity(fields.len() + 1);
        out.insert(ID_FIELD.to_string(), Document::Id(key::doc_id(key)));
        let mut leaves = values.iter_mut();
        build(fields, &mut leaves, &mut out);
        Some(Document::Object(out))
    }
}

/// The selection's `fields` from the key's values, one per leaf, in
/// order.
fn build(
    fields: &[(String, Node)],
    leaves: &mut std::slice::IterMut<Option<Document>>,
    out: &mut IndexMap<String, Document>,
) {
    for (name, node) in fields {
        let value = match node {
            Node::Whole => (leaves.next().and_then(Option::take)).expect("each leaf decoded"),
            Node::Fields(inner) => {
                let mut map = IndexMap::with_capacity(inner.len());
                build(inner, leaves, &mut map);
                Document::Object(map)
            }
        };
        out.insert(name.clone(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> Document {
        json.parse().unwrap()
    }

    fn projected(paths: &[&str], json: &str) -> String {
        Selection::new(paths.iter().copied())
            .project(doc(json))
            .to_string()
    }

    #[test]
    fn a_tree_of_paths() {
        let selection = Selection::new(["a.b", "c", "a.d", "a.b.x", "_id", "e[*].f", "c.y"]);
        assert_eq!(
            selection.leaves().unwrap(),
            ["a.b", "a.d", "c", "e"].map(String::from)
        );
        assert!(Selection::new(["a", ""]).is_whole());
        assert!(Selection::new(["[*].x"]).is_whole());
        assert_eq!(
            Selection::new(["_id"]).leaves().unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_projection_hands_out_what_it_names() {
        let stored = r#"{"_id":"x","a":{"b":1,"c":2},"n":null,"s":"t","arr":[{"b":1}],"z":3}"#;
        // `_id` first, the rest in the selection's order.
        assert_eq!(
            projected(&["z", "a.b", "s"], stored),
            r#"{"_id":"x","z":3,"a":{"b":1},"s":"t"}"#
        );
        // A stored null kept, a missing field left out.
        assert_eq!(projected(&["n", "gone"], stored), r#"{"_id":"x","n":null}"#);
        // A path stops at an array, a scalar, a missing object.
        assert_eq!(
            projected(&["arr.b", "s.len", "q.r", "a.zz"], stored),
            r#"{"_id":"x"}"#
        );
        // `[*]` selects the array whole.
        assert_eq!(
            projected(&["arr[*].b"], stored),
            r#"{"_id":"x","arr":[{"b":1}]}"#
        );
        // A path inside one selected whole adds nothing, in either order.
        assert_eq!(
            projected(&["a.b", "a"], stored),
            r#"{"_id":"x","a":{"b":1,"c":2}}"#
        );
        assert_eq!(projected(&[], stored), r#"{"_id":"x"}"#);
        assert_eq!(projected(&[""], stored), doc(stored).to_string());
    }

    /// SPEC §106.4: a selection is damaged exactly when the whole
    /// document is, with the same message, in what it takes and in what
    /// it skips, nested or not.
    #[test]
    fn a_selection_is_damaged_when_the_whole_is() {
        let doc =
            doc(r#"{"a":1,"s":"héllo","n":[1,{"x":"y"}],"o":{"p":true,"q":{"r":"t"}},"bin":null}"#);
        let encoded = crate::document::encode_stored(&doc);
        let id = DocId([3; 16]);
        let mut cases: Vec<Vec<u8>> = (0..encoded.len()).map(|n| encoded[..n].to_vec()).collect();
        for at in 0..encoded.len() {
            for byte in [0xFF, 0x80, 0x0A, 0x00] {
                let mut altered = encoded.clone();
                altered[at] = byte;
                cases.push(altered);
            }
        }
        let selections: [&[&str]; 6] =
            [&["a"], &["o.q.r"], &["n", "o.p"], &["missing"], &[], &["o"]];
        for bytes in cases {
            let whole = decode_document(&bytes)
                .map(|_| ())
                .map_err(|e| e.to_string());
            for paths in selections {
                let selected = Selection::new(paths.iter().copied())
                    .decode(&bytes, id)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                assert_eq!(selected, whole, "{paths:?} of {bytes:?}");
            }
        }
    }

    /// `decode` gives what `project` gives on the decoded document, for
    /// every selection of a few paths over a nested document.
    #[test]
    fn decoding_a_selection_is_projecting_the_whole() {
        let stored = doc(
            r#"{"a":{"b":1,"c":{"d":[1,2],"e":"x"}},"n":null,"s":"t","arr":[{"b":1}],
                "_id":"stored","z":{"y":{"x":{}}}}"#,
        );
        let encoded = crate::document::encode_stored(&stored);
        let id = DocId([7; 16]);
        let paths = [
            "a", "a.b", "a.c", "a.c.d", "a.c.e", "a.x", "n", "s", "s.t", "arr", "arr.b",
            "arr[*].b", "z.y", "z.y.x", "z.y.x.w", "_id", "gone", "",
        ];
        let whole = with_id(stored.clone(), id);
        for a in paths {
            for b in paths {
                for c in ["z", "a.c.e", "n"] {
                    let selection = Selection::new([a, b, c]);
                    let decoded = selection.decode(&encoded, id).unwrap();
                    let expected = selection.project(whole.clone());
                    assert_eq!(
                        decoded.to_string(),
                        expected.to_string(),
                        "select {a}, {b}, {c}"
                    );
                }
            }
        }
    }
}
