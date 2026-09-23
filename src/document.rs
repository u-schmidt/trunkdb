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
/// Only fails on an unrecognized type tag (real corruption) or invalid
/// UTF-8 in a string/key — a truncated buffer panics via out-of-bounds
/// slicing instead of a graceful error, the same trust level the rest of
/// this crate's cell decoders use: nothing reads a cell's bytes except
/// code that wrote them.
pub fn decode_document(bytes: &[u8]) -> std::io::Result<(Document, &[u8])> {
    let (&tag, rest) = bytes.split_first().expect("cell bytes must be non-empty");
    match tag {
        TAG_NULL => Ok((Document::Null, rest)),
        TAG_BOOL => {
            let (&value, rest) = rest.split_first().unwrap();
            Ok((Document::Bool(value != 0), rest))
        }
        TAG_INT => {
            let (value, rest) = rest.split_at(8);
            Ok((
                Document::Int(i64::from_le_bytes(value.try_into().unwrap())),
                rest,
            ))
        }
        TAG_FLOAT => {
            let (value, rest) = rest.split_at(8);
            Ok((
                Document::Float(f64::from_le_bytes(value.try_into().unwrap())),
                rest,
            ))
        }
        TAG_STRING => {
            let (bytes, rest) = read_len_prefixed(rest);
            let value = String::from_utf8(bytes.to_vec()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bad utf8 in document string",
                )
            })?;
            Ok((Document::String(value), rest))
        }
        TAG_BINARY => {
            let (bytes, rest) = read_len_prefixed(rest);
            Ok((Document::Binary(bytes.to_vec()), rest))
        }
        TAG_ARRAY => {
            let (count_bytes, mut rest) = rest.split_at(4);
            let count = u32::from_le_bytes(count_bytes.try_into().unwrap());
            let mut items = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let (item, remaining) = decode_document(rest)?;
                items.push(item);
                rest = remaining;
            }
            Ok((Document::Array(items), rest))
        }
        TAG_OBJECT => {
            let (count_bytes, mut rest) = rest.split_at(4);
            let count = u32::from_le_bytes(count_bytes.try_into().unwrap());
            let mut entries = IndexMap::with_capacity(count as usize);
            for _ in 0..count {
                let (key_bytes, r) = read_len_prefixed(rest);
                let key = String::from_utf8(key_bytes.to_vec()).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "bad utf8 in document key")
                })?;
                let (value, r) = decode_document(r)?;
                entries.insert(key, value);
                rest = r;
            }
            Ok((Document::Object(entries), rest))
        }
        TAG_ID => {
            let (id_bytes, rest) = rest.split_at(16);
            Ok((Document::Id(DocId(id_bytes.try_into().unwrap())), rest))
        }
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown document type tag {other} — file may be corrupt"),
        )),
    }
}

fn read_len_prefixed(bytes: &[u8]) -> (&[u8], &[u8]) {
    let (len_bytes, rest) = bytes.split_at(4);
    let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
    rest.split_at(len)
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
}
