use crate::decode::{corrupt, take, take_array, take_u8, take_u32};
use indexmap::IndexMap;

/// The schema-less value every document is made of — this DB's equivalent
/// of LiteDB's BsonValue / MongoDB's BSON. Real from day one: nearly every
/// other module is expressed in terms of this type.
///
/// Any `T: Serialize + DeserializeOwned` converts to and from `Document`
/// via the serde bridge (`serde_bridge.rs`, SPEC §13), which is how
/// `Collection<T>` works on top of `Collection<Document>`.
#[derive(Debug, Clone, PartialEq)]
pub enum Document {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Binary(Vec<u8>),
    Array(Vec<Document>),
    Object(IndexMap<String, Document>),
    Id(DocId),
}

/// How deeply a document may nest (SPEC §56), counted as MongoDB counts
/// (its limit is 100): the document is the first level, and every object
/// or array inside adds one. Far deeper than real data goes, even with
/// serde's wrapper around each enum variant, and it keeps a damaged
/// document from recursing the decoder off the end of the stack.
pub const MAX_NESTING: usize = 64;

impl Document {
    /// Whether `self`, as the whole document, nests deeper than
    /// `MAX_NESTING`: what a write refuses. An object whose only key,
    /// besides `_id`, is an export tag (`$id`, `$object`, ...) counts
    /// twice, since an export writes it inside `{"$object": ...}` (SPEC
    /// §30.1): every export then stays well inside what an import reads.
    /// Stops at the limit, so even a document too deep for the stack
    /// can't overflow it here.
    pub fn nests_too_deep(&self) -> bool {
        fn within(doc: &Document, levels: usize) -> bool {
            match doc {
                Document::Array(items) => {
                    levels >= 1 && items.iter().all(|item| within(item, levels - 1))
                }
                Document::Object(map) => {
                    let mut others = map.keys().filter(|key| *key != "_id");
                    let tag_like = matches!(
                        (others.next(), others.next()),
                        (Some(key), None) if crate::json::is_tag(key)
                    );
                    let cost = if tag_like { 2 } else { 1 };
                    levels >= cost && map.values().all(|value| within(value, levels - cost))
                }
                _ => true,
            }
        }
        !within(self, MAX_NESTING)
    }
}

/// A document's primary key: 16 bytes, generated as a UUIDv7 by default
/// (see `id.rs`) so ids created in sequence sort close together — good
/// insert locality once a real B-tree index replaces `LinearIndex`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub [u8; 16]);

impl std::fmt::Display for DocId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", uuid::Uuid::from_bytes(self.0))
    }
}

// Plain Rust values as `Document`s, so a filter can say
// `.eq("age", 36)` instead of `Document::Int(36)` (SPEC §35.2). Only
// lossless ones: no `u64`/`usize`, which can exceed `i64`, and no `Vec`,
// which could mean `Array` or `Binary`.
macro_rules! document_from {
    ($($from:ty => $variant:ident as $as:ty),* $(,)?) => {$(
        impl From<$from> for Document {
            fn from(value: $from) -> Self {
                Document::$variant(<$as>::from(value))
            }
        }
    )*};
}

document_from! {
    bool => Bool as bool,
    i8 => Int as i64,
    i16 => Int as i64,
    i32 => Int as i64,
    i64 => Int as i64,
    u8 => Int as i64,
    u16 => Int as i64,
    u32 => Int as i64,
    f32 => Float as f64,
    f64 => Float as f64,
    String => String as String,
    &str => String as String,
    DocId => Id as DocId,
}

/// `None` is `Null`, so an `Option` field's value can go into a filter
/// as it is.
impl<T: Into<Document>> From<Option<T>> for Document {
    fn from(value: Option<T>) -> Self {
        value.map_or(Document::Null, Into::into)
    }
}

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_INT: u8 = 2;
const TAG_FLOAT: u8 = 3;
const TAG_STRING: u8 = 4;
const TAG_BINARY: u8 = 5;
const TAG_ARRAY: u8 = 6;
const TAG_OBJECT: u8 = 7;
const TAG_ID: u8 = 8;

/// Encodes a `Document` into bytes: one type tag, then a payload whose
/// shape depends on the tag — fixed-width for scalars, a `u32` length
/// prefix + raw bytes for `String`/`Binary`, a `u32` count prefix followed
/// by that many encoded elements for `Array`, and a count prefix followed
/// by (key length, key bytes, encoded value) tuples for `Object`. All
/// lengths and counts are `u32` (SPEC §26.1): a document is no longer
/// capped at one page, so `u16`'s 64 KB would be a real limit — and one
/// that `as u16` used to enforce by silently truncating. `u32` casts here
/// can't truncate for the same reason one level up: every inner length
/// is at most the whole encoding's length, which `data.rs` rejects past
/// `u32::MAX`. `Array`/`Object` recurse into this same function for
/// their elements/values, which is also how a nested `_id` field
/// (`Document::Id`) round-trips — there's nothing document-shape-specific
/// about it, it's just another tagged value.
pub fn encode_document(doc: &Document) -> Vec<u8> {
    let mut buffer = Vec::new();
    write_document(doc, &mut buffer);
    buffer
}

fn write_document(doc: &Document, buffer: &mut Vec<u8>) {
    match doc {
        Document::Null => buffer.push(TAG_NULL),
        Document::Bool(value) => {
            buffer.push(TAG_BOOL);
            buffer.push(*value as u8);
        }
        Document::Int(value) => {
            buffer.push(TAG_INT);
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        Document::Float(value) => {
            buffer.push(TAG_FLOAT);
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        Document::String(value) => {
            buffer.push(TAG_STRING);
            write_len_prefixed(value.as_bytes(), buffer);
        }
        Document::Binary(value) => {
            buffer.push(TAG_BINARY);
            write_len_prefixed(value, buffer);
        }
        Document::Array(items) => {
            buffer.push(TAG_ARRAY);
            buffer.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                write_document(item, buffer);
            }
        }
        Document::Object(entries) => {
            buffer.push(TAG_OBJECT);
            buffer.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            for (key, value) in entries {
                write_len_prefixed(key.as_bytes(), buffer);
                write_document(value, buffer);
            }
        }
        Document::Id(id) => {
            buffer.push(TAG_ID);
            buffer.extend_from_slice(&id.0);
        }
    }
}

fn write_len_prefixed(bytes: &[u8], buffer: &mut Vec<u8>) {
    buffer.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buffer.extend_from_slice(bytes);
}

/// Decodes one `Document` from the front of `bytes`, returning it along
/// with whatever bytes follow it. Returning the remainder rather than just
/// the value is what makes the recursion work: `Array`/`Object` don't know
/// ahead of time how many bytes one of their elements occupies, so each
/// nested call has to report back where it stopped, so the next
/// element/entry can be decoded starting there.
///
/// Fails, with `InvalidData`, on an unknown type tag, invalid UTF-8, or
/// bytes that run out before the document does: cells come from the file,
/// and a length or count in them may be wrong (SPEC §55).
pub fn decode_document(bytes: &[u8]) -> std::io::Result<(Document, &[u8])> {
    let mut rest = bytes;
    let doc = decode_value(&mut rest, MAX_NESTING)?;
    Ok((doc, rest))
}

/// `levels`: how many more objects or arrays may open, this one included
/// (SPEC §56). A document nested deeper was never written by a trunkdb
/// with the limit, and decoding it could overflow the stack.
fn decode_value(bytes: &mut &[u8], levels: usize) -> std::io::Result<Document> {
    Ok(match take_u8(bytes, "a document's type tag")? {
        TAG_NULL => Document::Null,
        TAG_BOOL => Document::Bool(take_u8(bytes, "a bool")? != 0),
        TAG_INT => Document::Int(i64::from_le_bytes(take_array(bytes, "an int")?)),
        TAG_FLOAT => Document::Float(f64::from_le_bytes(take_array(bytes, "a float")?)),
        TAG_STRING => Document::String(decode_string(bytes, "a string")?),
        TAG_BINARY => Document::Binary(take_len_prefixed(bytes, "binary")?.to_vec()),
        TAG_ARRAY | TAG_OBJECT if levels == 0 => {
            return Err(corrupt(format_args!(
                "a document nests deeper than {MAX_NESTING} levels"
            )));
        }
        TAG_ARRAY => {
            let count = take_u32(bytes, "an array's length")? as usize;
            // Each item is at least a byte: a count past what's left is
            // damage, and must not size the allocation.
            let mut items = Vec::with_capacity(count.min(bytes.len()));
            for _ in 0..count {
                items.push(decode_value(bytes, levels - 1)?);
            }
            Document::Array(items)
        }
        TAG_OBJECT => {
            let count = take_u32(bytes, "an object's length")? as usize;
            let mut entries = IndexMap::with_capacity(count.min(bytes.len()));
            for _ in 0..count {
                let key = decode_string(bytes, "a key")?;
                entries.insert(key, decode_value(bytes, levels - 1)?);
            }
            Document::Object(entries)
        }
        TAG_ID => Document::Id(DocId(take_array(bytes, "an id")?)),
        other => return Err(corrupt(format_args!("unknown document type tag {other}"))),
    })
}

/// `[u32 length][bytes]`.
fn take_len_prefixed<'a>(bytes: &mut &'a [u8], what: &str) -> std::io::Result<&'a [u8]> {
    let len = take_u32(bytes, what)? as usize;
    take(bytes, len, what)
}

fn decode_string(bytes: &mut &[u8], what: &str) -> std::io::Result<String> {
    let raw = take_len_prefixed(bytes, what)?;
    String::from_utf8(raw.to_vec()).map_err(|_| corrupt(format_args!("bad UTF-8 in {what}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(doc: Document) {
        let bytes = encode_document(&doc);
        let (decoded, rest) = decode_document(&bytes).unwrap();
        assert_eq!(decoded, doc);
        assert!(rest.is_empty(), "decode must consume every encoded byte");
    }

    #[test]
    fn scalars_roundtrip() {
        roundtrip(Document::Null);
        roundtrip(Document::Bool(true));
        roundtrip(Document::Bool(false));
        roundtrip(Document::Int(-42));
        roundtrip(Document::Float(3.5));
        roundtrip(Document::String("hello".to_string()));
        roundtrip(Document::Binary(vec![1, 2, 3, 4]));
        roundtrip(Document::Id(DocId([7; 16])));
    }

    #[test]
    fn array_roundtrips() {
        roundtrip(Document::Array(vec![
            Document::Int(1),
            Document::String("two".to_string()),
            Document::Bool(false),
        ]));
    }

    #[test]
    fn plain_values_convert_into_documents() {
        let id = DocId([7; 16]);
        let cases: Vec<(Document, Document)> = vec![
            (true.into(), Document::Bool(true)),
            ((-8i8).into(), Document::Int(-8)),
            (300i16.into(), Document::Int(300)),
            (5.into(), Document::Int(5)), // an unsuffixed literal is i32
            (i64::MIN.into(), Document::Int(i64::MIN)),
            (255u8.into(), Document::Int(255)),
            (u16::MAX.into(), Document::Int(65535)),
            (u32::MAX.into(), Document::Int(4_294_967_295)),
            (1.5f32.into(), Document::Float(1.5)),
            (0.1.into(), Document::Float(0.1)),
            ("text".into(), Document::String("text".into())),
            (
                String::from("owned").into(),
                Document::String("owned".into()),
            ),
            (id.into(), Document::Id(id)),
            (Some("x").into(), Document::String("x".into())),
            (None::<i64>.into(), Document::Null),
            (Some(None::<bool>).into(), Document::Null),
        ];
        for (converted, expected) in cases {
            assert_eq!(converted, expected);
        }
    }

    #[test]
    fn nested_object_with_id_roundtrips() {
        let mut entries = IndexMap::new();
        entries.insert("_id".to_string(), Document::Id(DocId([1; 16])));
        entries.insert("name".to_string(), Document::String("Ada".to_string()));
        entries.insert(
            "tags".to_string(),
            Document::Array(vec![Document::String("admin".to_string())]),
        );
        roundtrip(Document::Object(entries));
    }

    #[test]
    fn lengths_past_u16_roundtrip() {
        // 70 000 bytes, elements and keys: each would have been truncated
        // by a u16 length or count.
        roundtrip(Document::String("x".repeat(70_000)));
        roundtrip(Document::Binary(vec![9; 70_000]));
        roundtrip(Document::Array(vec![Document::Null; 70_000]));
        let mut entries = IndexMap::new();
        entries.insert("k".repeat(70_000), Document::Int(1));
        roundtrip(Document::Object(entries));
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        let bytes = [200u8]; // not a valid tag
        assert!(decode_document(&bytes).is_err());
    }

    /// Found by fuzzing (SPEC §55): a document cut short anywhere is an
    /// `InvalidData` error, not a panic.
    #[test]
    fn every_truncated_document_is_an_error() {
        let doc = Document::Object(
            [
                ("n".to_string(), Document::Int(7)),
                ("f".to_string(), Document::Float(0.5)),
                ("b".to_string(), Document::Bool(true)),
                ("s".to_string(), Document::String("text".into())),
                ("x".to_string(), Document::Binary(vec![1, 2, 3])),
                ("i".to_string(), Document::Id(DocId([9; 16]))),
                ("a".to_string(), Document::Array(vec![Document::Null])),
            ]
            .into_iter()
            .collect(),
        );
        let bytes = encode_document(&doc);
        for len in 0..bytes.len() {
            let err = decode_document(&bytes[..len]).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{len}");
        }
        assert_eq!(decode_document(&bytes).unwrap().0, doc);
    }

    /// `levels` objects or arrays, each inside the one before, the
    /// innermost holding `leaf`.
    fn nested(levels: usize, leaf: Document) -> Document {
        (0..levels).fold(leaf, |inner, i| match i % 2 {
            0 => Document::Array(vec![inner]),
            _ => Document::Object([("x".to_string(), inner)].into_iter().collect()),
        })
    }

    /// SPEC §56: 64 levels are written and read; 65 are refused on
    /// write, and a damaged cell claiming them is an error on read.
    #[test]
    fn documents_nest_at_most_64_levels() {
        let deepest = nested(MAX_NESTING, Document::Int(1));
        assert!(!deepest.nests_too_deep());
        let bytes = encode_document(&deepest);
        assert_eq!(decode_document(&bytes).unwrap().0, deepest);

        let too_deep = nested(MAX_NESTING + 1, Document::Int(1));
        assert!(too_deep.nests_too_deep());
        let err = decode_document(&encode_document(&too_deep)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("deeper than 64"), "{err}");
    }

    /// An object whose only key besides `_id` is an export tag counts
    /// twice: an export wraps it in `{"$object": ...}`.
    #[test]
    fn tag_like_objects_count_twice() {
        let tagged =
            |inner: Document| Document::Object([("$id".to_string(), inner)].into_iter().collect());
        let half = (0..MAX_NESTING / 2).fold(Document::Null, |inner, _| tagged(inner));
        assert!(!half.nests_too_deep());
        assert!(tagged(half).nests_too_deep());
        let with_id = |inner: Document| {
            Document::Object(
                [
                    ("_id".to_string(), Document::Null),
                    ("$object".to_string(), inner),
                ]
                .into_iter()
                .collect(),
            )
        };
        // 31 tag-like levels around an array: 63. At the top, next to an
        // `_id`, one more tag-like object is two levels, so 65.
        let inner =
            (0..MAX_NESTING / 2 - 1).fold(Document::Array(vec![]), |inner, _| tagged(inner));
        assert!(!inner.nests_too_deep());
        assert!(
            with_id(inner).nests_too_deep(),
            "`_id` doesn't count as a field"
        );
    }

    /// The check stops at the limit: a document far too deep for the
    /// stack to walk whole is still just refused.
    #[test]
    fn a_document_too_deep_for_the_stack_is_refused_not_walked() {
        // Built and dropped on a thread with room for it; the check itself
        // needs only 65 levels of that.
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                let abyss = nested(100_000, Document::Null);
                assert!(abyss.nests_too_deep());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// A count or length near `u32::MAX` with nothing behind it is an
    /// error, and never sizes an allocation first.
    #[test]
    fn huge_counts_and_lengths_are_errors() {
        for tag in [TAG_ARRAY, TAG_OBJECT, TAG_STRING, TAG_BINARY] {
            let mut bytes = vec![tag];
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
            bytes.push(TAG_NULL);
            assert!(decode_document(&bytes).is_err(), "{tag}");
        }
    }
}
