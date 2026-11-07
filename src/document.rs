use crate::datetime::{Date, DateTime, Time};
use crate::decode::{corrupt, put_varint, take, take_array, take_u8, take_u32, take_varint};
use crate::map::Map;
use crate::names::FieldNames;
use std::time::SystemTime;

/// The schema-less value every document is made of — this DB's equivalent
/// of LiteDB's BsonValue / MongoDB's BSON. Real from day one: nearly every
/// other module is expressed in terms of this type.
///
/// Any `T: Serialize + DeserializeOwned` converts to and from `Document`
/// via the serde bridge (`serde_bridge.rs`, SPEC §13), which is how
/// `Collection<T>` works on top of `Collection<Document>`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Document {
    /// JSON's `null`, and what a `None` stores. `is null` finds it and a
    /// missing field alike (SPEC §32).
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A whole number. It compares with a `Float` by exact value: `5`
    /// equals `5.0` (SPEC §34.1).
    Int(i64),
    /// A floating-point number. NaN equals nothing, itself included, and
    /// has no order.
    Float(f64),
    /// Text.
    String(String),
    /// Bytes: `{"$binary": "<base64>"}` in JSON text (SPEC §30.1).
    Binary(Vec<u8>),
    /// A list of values, in order.
    Array(Vec<Document>),
    /// An object: names to values, in order (SPEC §116).
    Object(Map),
    /// A document id: a document's own `_id`, or a reference to another
    /// document. `{"$id": "<uuid>"}` in JSON text; ids sort after strings,
    /// by when they were made (SPEC §62).
    Id(DocId),
    /// A point in time, in UTC, to the nanosecond (SPEC §69, §70): what a
    /// `SystemTime` or `DateTime` field stores. Filters compare, sort and
    /// index it in time order; it equals no string or number.
    DateTime(DateTime),
    /// A day in the calendar, without a time (SPEC §173): what a `Date`
    /// field stores. Compares with dates only, in calendar order.
    Date(Date),
    /// A time of day, without a date (SPEC §173): what a `Time` field
    /// stores. Compares with times only.
    Time(Time),
}

impl Document {
    /// How deeply a document may nest (SPEC §56), counted as MongoDB
    /// counts (its limit is 100): the document is the first level, and
    /// every object or array inside adds one. Far deeper than real data
    /// goes, even with serde's wrapper around each enum variant, and it
    /// keeps a damaged document from recursing the decoder off the end of
    /// the stack. A write nested deeper is refused.
    pub const MAX_NESTING: usize = 64;

    /// Whether `self`, as the whole document, nests deeper than
    /// `Document::MAX_NESTING`: what a write refuses. An object whose only key,
    /// besides `_id`, is an export tag (`$id`, `$object`, ...) counts
    /// twice, since an export writes it inside `{"$object": ...}` (SPEC
    /// §30.1): every export then stays well inside what an import reads.
    /// Stops at the limit, so even a document too deep for the stack
    /// can't overflow it here.
    pub(crate) fn nests_too_deep(&self) -> bool {
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
        !within(self, Document::MAX_NESTING)
    }
}

/// Reading a value without a `match` (SPEC §136.3), as
/// `serde_json::Value` is read: each `as_` method gives `None` for a value
/// of another type.
///
/// ```
/// use trunkdb::Document;
///
/// let doc: Document = r#"{"pos": {"lat": 52.5, "lon": 13}, "tags": ["a"]}"#.parse()?;
/// assert_eq!(doc.get("pos.lat").and_then(Document::as_f64), Some(52.5));
/// // A whole number reads as a float too, as filters compare them.
/// assert_eq!(doc.get("pos.lon").and_then(Document::as_f64), Some(13.0));
/// assert_eq!(doc.get("pos.lon").and_then(Document::as_i64), Some(13));
/// assert_eq!(doc.get("pos.alt"), None);
/// assert_eq!(doc.get("tags").and_then(Document::as_array).map(<[_]>::len), Some(1));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
impl Document {
    /// The value at `path` (SPEC §31): field names, with a `.` between
    /// the steps into objects, `""` for the document itself. `None` where
    /// a step is missing or isn't an object. A path with `[*]` reaches any
    /// number of values, not one, and gives `None`: read the array with
    /// `as_array`.
    pub fn get(&self, path: &str) -> Option<&Document> {
        if path.contains("[*]") {
            return None;
        }
        crate::path::Path::new(path).field_value(self)
    }

    /// Whether it is `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Document::Null)
    }

    /// A `Bool`'s value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Document::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// An `Int`'s value. A `Float` gives `None`, even a whole one.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Document::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// A `Float`'s value, or an `Int`'s as a float, as filters compare
    /// the two (SPEC §34.1). Beyond 2^53 an `Int` is rounded to the
    /// nearest float.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Document::Float(value) => Some(*value),
            Document::Int(value) => Some(*value as f64),
            _ => None,
        }
    }

    /// A `String`'s text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Document::String(text) => Some(text),
            _ => None,
        }
    }

    /// A `Binary`'s bytes.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Document::Binary(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// A `DateTime`'s point in time.
    pub fn as_datetime(&self) -> Option<DateTime> {
        match self {
            Document::DateTime(at) => Some(*at),
            _ => None,
        }
    }

    /// A `Date`'s day.
    pub fn as_date(&self) -> Option<Date> {
        match self {
            Document::Date(day) => Some(*day),
            _ => None,
        }
    }

    /// A `Time`'s time of day.
    pub fn as_time(&self) -> Option<Time> {
        match self {
            Document::Time(time) => Some(*time),
            _ => None,
        }
    }

    /// An `Id`'s id.
    pub fn as_id(&self) -> Option<DocId> {
        match self {
            Document::Id(id) => Some(*id),
            _ => None,
        }
    }

    /// An `Array`'s elements.
    pub fn as_array(&self) -> Option<&[Document]> {
        match self {
            Document::Array(elements) => Some(elements),
            _ => None,
        }
    }

    /// An `Object`'s fields.
    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Document::Object(map) => Some(map),
            _ => None,
        }
    }
}

/// A document as tagged JSON text (SPEC §71): compact, or indented with
/// `{:#}`. Plain JSON, except for what JSON has no type for: an id is
/// `{"$id": "<uuid>"}`, a date-time `{"$date": "<RFC 3339>"}`, binary
/// `{"$binary": "<base64>"}` (§30.1). `str::parse` reads it back, equal
/// to what was written.
///
/// ```
/// use trunkdb::Document;
///
/// let doc: Document = r#"{"name": "Ann", "seen": {"$date": "2026-09-27T14:05:00Z"}}"#.parse()?;
/// assert_eq!(doc.to_string(), r#"{"name":"Ann","seen":{"$date":"2026-09-27T14:05:00Z"}}"#);
/// # Ok::<(), trunkdb::ParseDocumentError>(())
/// ```
impl std::fmt::Display for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&crate::json::to_json(self), f)
    }
}

/// A document from JSON text (SPEC §71): any JSON, with the tags that
/// `Display` writes read back as ids, date-times and binary. Integers
/// outside `i64` and malformed tags are errors.
impl std::str::FromStr for Document {
    type Err = crate::json::ParseDocumentError;

    fn from_str(s: &str) -> Result<Document, Self::Err> {
        crate::json::parse(s)
    }
}

/// A document's primary key: 16 bytes, generated as a UUIDv7 by default
/// (see `id.rs`) so ids created in sequence sort close together, which
/// keeps inserts into the primary B-tree local.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub [u8; 16]);

impl std::fmt::Display for DocId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", uuid::Uuid::from_bytes(self.0))
    }
}

impl DocId {
    /// The all-zero id, which no generator makes: on an insert, `_id`
    /// holding it means "make one", like `None` (SPEC §59).
    pub const NIL: DocId = DocId([0; 16]);
}

/// An id from its string form (SPEC §63), so `id.to_string()` and
/// `text.parse::<DocId>()` go back and forth: a UUID, as `Display`
/// writes it, or in the other forms UUIDs are written in (no hyphens,
/// braces, `urn:uuid:`).
impl std::str::FromStr for DocId {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<DocId, ParseIdError> {
        uuid::Uuid::parse_str(s)
            .map(|uuid| DocId(uuid.into_bytes()))
            .map_err(|reason| ParseIdError {
                input: s.to_string(),
                reason,
            })
    }
}

/// A string that isn't a document id: what `str::parse::<DocId>` returns
/// (SPEC §63). Its message says which string and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIdError {
    input: String,
    reason: uuid::Error,
}

impl std::fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} is not a document id: {}", self.input, self.reason)
    }
}

impl std::error::Error for ParseIdError {}

/// The name `DocId`'s serde impls give their newtype (SPEC §59), so the
/// serde bridge can tell an id from a string and store it as
/// `Document::Id`: the technique `serde_bytes` uses for byte strings. Any
/// other format sees a newtype around the id's string form, and writes
/// the string.
pub(crate) const DOC_ID_NEWTYPE: &str = "$trunkdb::DocId";

/// Lets a struct in a `Collection<T>` carry its own id (SPEC §59):
/// `#[serde(rename = "_id")] id: Option<DocId>`, filled in on every read,
/// and on insert used if it's `Some`, generated if it's `None`.
impl serde::Serialize for DocId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_newtype_struct(DOC_ID_NEWTYPE, &self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for DocId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;

        impl<'de> serde::de::Visitor<'de> for IdVisitor {
            type Value = DocId;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a document id, as a UUID string")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<DocId, E> {
                s.parse::<DocId>().map_err(E::custom)
            }

            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<DocId, D::Error> {
                deserializer.deserialize_str(self)
            }
        }

        // The serde bridge hands a stored `Document::Id` over as its string
        // (SPEC §13.5); a format like JSON, the newtype around it.
        deserializer.deserialize_newtype_struct(DOC_ID_NEWTYPE, IdVisitor)
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
    SystemTime => DateTime as DateTime,
    DateTime => DateTime as DateTime,
    Date => Date as Date,
    Time => Time as Time,
}

/// `None` is `Null`, so an `Option` field's value can go into a filter
/// as it is.
impl<T: Into<Document>> From<Option<T>> for Document {
    fn from(value: Option<T>) -> Self {
        value.map_or(Document::Null, Into::into)
    }
}

pub(crate) const TAG_NULL: u8 = 0;
pub(crate) const TAG_BOOL: u8 = 1;
pub(crate) const TAG_INT: u8 = 2;
pub(crate) const TAG_FLOAT: u8 = 3;
pub(crate) const TAG_STRING: u8 = 4;
pub(crate) const TAG_BINARY: u8 = 5;
pub(crate) const TAG_ARRAY: u8 = 6;
pub(crate) const TAG_OBJECT: u8 = 7;
pub(crate) const TAG_ID: u8 = 8;
/// A `DateTime` (SPEC §69, file format 11): seconds since 1970 as an
/// `i64`, then the nanoseconds as a `u32`, both little-endian.
pub(crate) const TAG_DATETIME: u8 = 9;

// Since file format 14, in stored documents only (`encode_stored`):
// lengths and counts as varints, and an object's keys as numbers into
// the file's dictionary of field names (`names.rs`). The tags above stay
// what `encode_document` writes, and what a document stored before 14
// holds, which reads go on decoding.

/// An object: `[varint count]`, then each entry's key and value. A key
/// is a varint: a name's number in the dictionary times two, or a name
/// not in it, its length times two plus one, then its UTF-8 bytes.
pub(crate) const TAG_FIELDS: u8 = 10;
/// An int, zigzag-encoded as a varint: `-1` is `1`, `1` is `2`.
pub(crate) const TAG_VARINT: u8 = 11;
/// A string: `[varint length][UTF-8 bytes]`.
pub(crate) const TAG_TEXT: u8 = 12;
/// Binary: `[varint length][bytes]`.
pub(crate) const TAG_BYTES: u8 = 13;
/// An array: `[varint count]`, then the items.
pub(crate) const TAG_LIST: u8 = 14;

// Since file format 15 (SPEC §173), in both encodings.

/// A `Date`: its days since 1970-01-01, zigzag-encoded as a varint.
pub(crate) const TAG_DATE: u8 = 15;
/// A `Time`: its nanoseconds since midnight as a varint.
pub(crate) const TAG_TIME: u8 = 16;

/// Whether `tag` is an object's, either encoding.
pub(crate) fn is_object_tag(tag: u8) -> bool {
    tag == TAG_OBJECT || tag == TAG_FIELDS
}

/// Whether `encoded`, a document as `encode_document` or `encode_stored`
/// wrote it, is an object: its first byte, without decoding the rest
/// (SPEC §95).
pub(crate) fn encodes_object(encoded: &[u8]) -> bool {
    encoded.first().is_some_and(|tag| is_object_tag(*tag))
}

/// After an array's or an object's tag: how many items or entries.
pub(crate) fn take_count(bytes: &mut &[u8], tag: u8, what: &str) -> std::io::Result<u64> {
    match tag {
        TAG_ARRAY | TAG_OBJECT => take_u32(bytes, what).map(u64::from),
        _ => take_varint(bytes, what),
    }
}

/// One of an object's keys, after its tag: a name of `names`, or one
/// stored as it is.
pub(crate) fn take_key<'k, 'b: 'k, 'n: 'k>(
    bytes: &mut &'b [u8],
    tag: u8,
    names: &'n FieldNames,
) -> std::io::Result<&'k str> {
    if tag == TAG_OBJECT {
        return take_str(bytes, "a key");
    }
    let key = take_varint(bytes, "a key")?;
    if key & 1 == 0 {
        return names.name(key >> 1).ok_or_else(|| {
            corrupt(format_args!(
                "a key is field name {}, of {} there are",
                key >> 1,
                names.len()
            ))
        });
    }
    let raw = take(
        bytes,
        usize::try_from(key >> 1).unwrap_or(usize::MAX),
        "a key",
    )?;
    std::str::from_utf8(raw).map_err(|_| corrupt("bad UTF-8 in a key"))
}

/// An int, after its tag.
pub(crate) fn take_int(bytes: &mut &[u8], tag: u8) -> std::io::Result<i64> {
    if tag == TAG_INT {
        return Ok(i64::from_le_bytes(take_array(bytes, "an int")?));
    }
    let zigzag = take_varint(bytes, "an int")?;
    Ok((zigzag >> 1) as i64 ^ -((zigzag & 1) as i64))
}

/// A string's or binary's bytes, after its tag.
pub(crate) fn take_run<'a>(bytes: &mut &'a [u8], tag: u8, what: &str) -> std::io::Result<&'a [u8]> {
    match tag {
        TAG_STRING | TAG_BINARY => take_len_prefixed(bytes, what),
        _ => {
            let len = take_varint(bytes, what)?;
            take(bytes, usize::try_from(len).unwrap_or(usize::MAX), what)
        }
    }
}

/// A string, after its tag.
pub(crate) fn take_text<'a>(bytes: &mut &'a [u8], tag: u8) -> std::io::Result<&'a str> {
    let raw = take_run(bytes, tag, "a string")?;
    std::str::from_utf8(raw).map_err(|_| corrupt("bad UTF-8 in a string"))
}

/// A document as a data cell stores it (SPEC §59): the varint encoding
/// above, its keys numbered by `names` where it has them, and an
/// object's own `_id` left out. The cell holds the id already, and reads
/// put it back (`with_id`), so there's one id, not a copy that could say
/// something else.
pub(crate) fn encode_stored(doc: &Document, names: &FieldNames) -> Vec<u8> {
    let mut buffer = Vec::new();
    match doc {
        Document::Object(entries) => {
            let fields = entries.iter().filter(|(key, _)| *key != ID_FIELD);
            write_fields(fields.clone().count(), fields, names, &mut buffer);
        }
        other => write_stored(other, names, &mut buffer),
    }
    buffer
}

fn write_fields<'d>(
    count: usize,
    fields: impl Iterator<Item = (&'d String, &'d Document)>,
    names: &FieldNames,
    buffer: &mut Vec<u8>,
) {
    buffer.push(TAG_FIELDS);
    put_varint(count as u64, buffer);
    for (key, value) in fields {
        match names.number(key) {
            Some(number) => put_varint(u64::from(number) << 1, buffer),
            None => {
                put_varint(((key.len() as u64) << 1) | 1, buffer);
                buffer.extend_from_slice(key.as_bytes());
            }
        }
        write_stored(value, names, buffer);
    }
}

fn write_stored(doc: &Document, names: &FieldNames, buffer: &mut Vec<u8>) {
    match doc {
        Document::Int(value) => {
            buffer.push(TAG_VARINT);
            put_varint(((value << 1) ^ (value >> 63)) as u64, buffer);
        }
        Document::String(value) => {
            buffer.push(TAG_TEXT);
            put_varint(value.len() as u64, buffer);
            buffer.extend_from_slice(value.as_bytes());
        }
        Document::Binary(value) => {
            buffer.push(TAG_BYTES);
            put_varint(value.len() as u64, buffer);
            buffer.extend_from_slice(value);
        }
        Document::Array(items) => {
            buffer.push(TAG_LIST);
            put_varint(items.len() as u64, buffer);
            for item in items {
                write_stored(item, names, buffer);
            }
        }
        Document::Object(entries) => write_fields(entries.len(), entries.iter(), names, buffer),
        other => write_document(other, buffer),
    }
}

/// The field a document's id appears in: `_id`, as in MongoDB.
/// `Filter::id` spares callers spelling it (SPEC §63).
pub(crate) const ID_FIELD: &str = "_id";

/// `doc` with `id` in its `_id` field, first, replacing whatever was
/// there (SPEC §18, §59). Other documents have no field for it.
pub(crate) fn with_id(doc: Document, id: DocId) -> Document {
    match doc {
        Document::Object(mut map) => {
            map.0
                .shift_insert(0, ID_FIELD.to_string(), Document::Id(id));
            Document::Object(map)
        }
        other => other,
    }
}

/// Encodes a `Document` into bytes: one type tag, then a payload whose
/// shape depends on the tag — fixed-width for scalars, a `u32` length
/// prefix + raw bytes for `String`/`Binary`, a `u32` count prefix followed
/// by that many encoded elements for `Array`, and a count prefix followed
/// by (key length, key bytes, encoded value) tuples for `Object`. All
/// lengths and counts are `u32` (SPEC §26.1). The canonical form, which
/// needs no dictionary: what comparisons, keys of values and the files
/// before format 14 use. A data cell stores `encode_stored`'s.
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
        Document::DateTime(time) => {
            buffer.push(TAG_DATETIME);
            buffer.extend_from_slice(&time.unix_seconds().to_le_bytes());
            buffer.extend_from_slice(&time.subsec_nanos().to_le_bytes());
        }
        Document::Date(day) => {
            let days = day.days_since_epoch();
            buffer.push(TAG_DATE);
            put_varint(((days << 1) ^ (days >> 63)) as u64, buffer);
        }
        Document::Time(time) => {
            buffer.push(TAG_TIME);
            put_varint(time.nanos_since_midnight(), buffer);
        }
    }
}

fn write_len_prefixed(bytes: &[u8], buffer: &mut Vec<u8>) {
    buffer.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buffer.extend_from_slice(bytes);
}

/// Decodes one `Document` from the front of `bytes`, as `encode_document`
/// wrote it, returning it along with whatever bytes follow it.
///
/// Fails, with `InvalidData`, on an unknown type tag, invalid UTF-8, or
/// bytes that run out before the document does: cells come from the file,
/// and a length or count in them may be wrong (SPEC §55).
#[cfg(test)]
pub fn decode_document(bytes: &[u8]) -> std::io::Result<(Document, &[u8])> {
    let mut rest = bytes;
    let doc = decode_value(&mut rest, Document::MAX_NESTING, &FieldNames::default())?;
    Ok((doc, rest))
}

/// A stored document, as `encode_stored` wrote it with `names`, or an
/// older release with `encode_document`: the whole cell is the document.
pub(crate) fn decode_stored(encoded: &[u8], names: &FieldNames) -> std::io::Result<Document> {
    let mut rest = encoded;
    decode_value(&mut rest, Document::MAX_NESTING, names)
}

/// `levels`: how many more objects or arrays may open, this one included
/// (SPEC §56). A document nested deeper was never written by a trunkdb
/// with the limit, and decoding it could overflow the stack. `names`:
/// the dictionary its keys are numbers into, if it was stored.
pub(crate) fn decode_value(
    bytes: &mut &[u8],
    levels: usize,
    names: &FieldNames,
) -> std::io::Result<Document> {
    Ok(match take_u8(bytes, "a document's type tag")? {
        TAG_NULL => Document::Null,
        TAG_BOOL => Document::Bool(take_u8(bytes, "a bool")? != 0),
        tag @ (TAG_INT | TAG_VARINT) => Document::Int(take_int(bytes, tag)?),
        TAG_FLOAT => Document::Float(f64::from_le_bytes(take_array(bytes, "a float")?)),
        tag @ (TAG_STRING | TAG_TEXT) => Document::String(take_text(bytes, tag)?.to_string()),
        tag @ (TAG_BINARY | TAG_BYTES) => {
            Document::Binary(take_run(bytes, tag, "binary")?.to_vec())
        }
        TAG_ARRAY | TAG_OBJECT | TAG_LIST | TAG_FIELDS if levels == 0 => {
            return Err(too_deep());
        }
        tag @ (TAG_ARRAY | TAG_LIST) => {
            let count = take_count(bytes, tag, "an array's length")?;
            // Each item is at least a byte: a count past what's left is
            // damage, and must not size the allocation.
            let mut items = Vec::with_capacity(bytes.len().min(count as usize));
            for _ in 0..count {
                items.push(decode_value(bytes, levels - 1, names)?);
            }
            Document::Array(items)
        }
        tag @ (TAG_OBJECT | TAG_FIELDS) => {
            let count = take_count(bytes, tag, "an object's length")?;
            // At the top, room for the `_id` a read adds (SPEC §59):
            // without it, adding one reallocates the map on every read.
            let top = usize::from(levels == Document::MAX_NESTING);
            let mut entries = Map::with_capacity(bytes.len().min(count as usize) + top);
            for _ in 0..count {
                let key = take_key(bytes, tag, names)?.to_string();
                entries.insert(key, decode_value(bytes, levels - 1, names)?);
            }
            Document::Object(entries)
        }
        TAG_ID => Document::Id(DocId(take_array(bytes, "an id")?)),
        TAG_DATETIME => Document::DateTime(take_datetime(bytes)?),
        TAG_DATE => Document::Date(take_date(bytes)?),
        TAG_TIME => Document::Time(take_time(bytes)?),
        other => return Err(corrupt(format_args!("unknown document type tag {other}"))),
    })
}

fn too_deep() -> std::io::Error {
    corrupt(format_args!(
        "a document nests deeper than {} levels",
        Document::MAX_NESTING
    ))
}

/// `[u32 length][bytes]`.
pub(crate) fn take_len_prefixed<'a>(bytes: &mut &'a [u8], what: &str) -> std::io::Result<&'a [u8]> {
    let len = take_u32(bytes, what)? as usize;
    take(bytes, len, what)
}

/// `[u32 length][UTF-8 bytes]`, borrowed.
pub(crate) fn take_str<'a>(bytes: &mut &'a [u8], what: &str) -> std::io::Result<&'a str> {
    let raw = take_len_prefixed(bytes, what)?;
    std::str::from_utf8(raw).map_err(|_| corrupt(format_args!("bad UTF-8 in {what}")))
}

/// A date-time's encoding, after its tag, checked as `decode_value`
/// checks it.
pub(crate) fn take_datetime(bytes: &mut &[u8]) -> std::io::Result<DateTime> {
    let secs = i64::from_le_bytes(take_array(bytes, "a date-time")?);
    let nanos = u32::from_le_bytes(take_array(bytes, "a date-time")?);
    DateTime::from_unix(secs, nanos)
        .ok_or_else(|| corrupt(format_args!("a date-time with {nanos} ns past its second")))
}

/// A date's encoding, after its tag: a day a `DateTime` falls on.
fn take_date(bytes: &mut &[u8]) -> std::io::Result<Date> {
    let zigzag = take_varint(bytes, "a date")?;
    let days = (zigzag >> 1) as i64 ^ -((zigzag & 1) as i64);
    Date::from_days_since_epoch(days)
        .ok_or_else(|| corrupt(format_args!("a date {days} days from 1970, past the last")))
}

/// A time of day's encoding, after its tag: below a day.
fn take_time(bytes: &mut &[u8]) -> std::io::Result<Time> {
    let nanos = take_varint(bytes, "a time of day")?;
    Time::from_nanos_since_midnight(nanos).ok_or_else(|| {
        corrupt(format_args!(
            "a time of day {nanos} ns after midnight, a day or more"
        ))
    })
}

/// Moves past one encoded value without building it (SPEC §99), checking
/// what `decode_value` checks — tags, lengths, UTF-8, key numbers,
/// date-times, the nesting limit — so a value is damaged for a read that
/// skips it exactly when it is for one that decodes it.
pub(crate) fn skip_value(
    bytes: &mut &[u8],
    levels: usize,
    names: &FieldNames,
) -> std::io::Result<()> {
    match take_u8(bytes, "a document's type tag")? {
        TAG_NULL => {}
        TAG_BOOL => {
            take_u8(bytes, "a bool")?;
        }
        tag @ (TAG_INT | TAG_VARINT) => {
            take_int(bytes, tag)?;
        }
        TAG_FLOAT => {
            take_array::<8>(bytes, "a float")?;
        }
        tag @ (TAG_STRING | TAG_TEXT) => {
            take_text(bytes, tag)?;
        }
        tag @ (TAG_BINARY | TAG_BYTES) => {
            take_run(bytes, tag, "binary")?;
        }
        TAG_ARRAY | TAG_OBJECT | TAG_LIST | TAG_FIELDS if levels == 0 => {
            return Err(too_deep());
        }
        tag @ (TAG_ARRAY | TAG_LIST) => {
            let count = take_count(bytes, tag, "an array's length")?;
            for _ in 0..count {
                skip_value(bytes, levels - 1, names)?;
            }
        }
        tag @ (TAG_OBJECT | TAG_FIELDS) => {
            let count = take_count(bytes, tag, "an object's length")?;
            for _ in 0..count {
                take_key(bytes, tag, names)?;
                skip_value(bytes, levels - 1, names)?;
            }
        }
        TAG_ID => {
            take_array::<16>(bytes, "an id")?;
        }
        TAG_DATETIME => {
            take_datetime(bytes)?;
        }
        TAG_DATE => {
            take_date(bytes)?;
        }
        TAG_TIME => {
            take_time(bytes)?;
        }
        other => return Err(corrupt(format_args!("unknown document type tag {other}"))),
    }
    Ok(())
}

/// A stored document's top-level fields among `fields` only, as an
/// object, with `id` as `_id` if `fields` has it (SPEC §99.2); every
/// other field is skipped without being built. A path's value depends
/// only on its top-level field, so a filter or sort that names no other
/// field sees the same values here as in the whole document. A stored
/// document that isn't an object (only damage since §95) is decoded
/// whole.
pub(crate) fn decode_fields(
    encoded: &[u8],
    id: DocId,
    fields: &[&str],
    names: &FieldNames,
) -> std::io::Result<Document> {
    if !encodes_object(encoded) {
        return Ok(with_id(decode_stored(encoded, names)?, id));
    }
    let mut entries = Map::new();
    if fields.contains(&ID_FIELD) {
        entries.insert(ID_FIELD.to_string(), Document::Id(id));
    }
    if fields.iter().all(|name| *name == ID_FIELD) {
        // No stored field named: nothing to read (SPEC §99.2).
        return Ok(Document::Object(entries));
    }
    let tag = encoded[0];
    let mut bytes = &encoded[1..];
    let levels = Document::MAX_NESTING - 1;
    let count = take_count(&mut bytes, tag, "an object's length")?;
    for _ in 0..count {
        let key = take_key(&mut bytes, tag, names)?;
        if key != ID_FIELD && fields.contains(&key) {
            entries.insert(key.to_string(), decode_value(&mut bytes, levels, names)?);
        } else {
            skip_value(&mut bytes, levels, names)?;
        }
    }
    Ok(Document::Object(entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::sample as some_names;

    /// SPEC §136.3: each reader gives its type's value, and `None` for
    /// every other type.
    #[test]
    fn each_reader_reads_its_own_type_only() {
        let at = DateTime::from_unix(1_790_000_000, 5).unwrap();
        let id = DocId([7; 16]);
        let values = [
            Document::Null,
            Document::Bool(true),
            Document::Int(-3),
            Document::Float(2.5),
            Document::String("s".into()),
            Document::Binary(vec![1, 2]),
            Document::Array(vec![Document::Int(1)]),
            Document::Object(Map::from_iter([("k".to_string(), Document::Null)])),
            Document::Id(id),
            Document::DateTime(at),
        ];
        for value in &values {
            let kind = |of: &Document| std::mem::discriminant(of) == std::mem::discriminant(value);
            assert_eq!(value.is_null(), kind(&Document::Null));
            assert_eq!(value.as_bool().is_some(), kind(&values[1]), "{value}");
            assert_eq!(value.as_i64().is_some(), kind(&values[2]), "{value}");
            // An `Int` reads as a float too (§34.1).
            assert_eq!(
                value.as_f64().is_some(),
                kind(&values[2]) || kind(&values[3]),
                "{value}"
            );
            assert_eq!(value.as_str().is_some(), kind(&values[4]), "{value}");
            assert_eq!(value.as_bytes().is_some(), kind(&values[5]), "{value}");
            assert_eq!(value.as_array().is_some(), kind(&values[6]), "{value}");
            assert_eq!(value.as_object().is_some(), kind(&values[7]), "{value}");
            assert_eq!(value.as_id().is_some(), kind(&values[8]), "{value}");
            assert_eq!(value.as_datetime().is_some(), kind(&values[9]), "{value}");
        }
        assert_eq!(values[1].as_bool(), Some(true));
        assert_eq!(values[2].as_i64(), Some(-3));
        assert_eq!(values[2].as_f64(), Some(-3.0));
        assert_eq!(values[3].as_f64(), Some(2.5));
        assert_eq!(Document::Float(4.0).as_i64(), None, "a whole float");
        assert_eq!(values[4].as_str(), Some("s"));
        assert_eq!(values[5].as_bytes(), Some(&[1u8, 2][..]));
        assert_eq!(values[6].as_array(), Some(&[Document::Int(1)][..]));
        assert_eq!(values[8].as_id(), Some(id));
        assert_eq!(values[9].as_datetime(), Some(at));
    }

    /// SPEC §136.3: `get` follows a path into objects, as a condition
    /// reads it; `""` is the document; `[*]` reaches no single value.
    #[test]
    fn get_follows_a_path() {
        let doc: Document = r#"{"a": {"b": {"c": 1}}, "d": [{"e": 2}], "f": null}"#
            .parse()
            .unwrap();
        assert_eq!(doc.get("a.b.c"), Some(&Document::Int(1)));
        assert_eq!(
            doc.get("a.b").and_then(Document::as_object).map(Map::len),
            Some(1)
        );
        assert_eq!(doc.get(""), Some(&doc));
        assert_eq!(doc.get("f"), Some(&Document::Null), "there, and null");
        assert_eq!(doc.get("g"), None, "not there");
        assert_eq!(doc.get("a.x.c"), None);
        assert_eq!(doc.get("a.b.c.d"), None, "through a number");
        assert_eq!(doc.get("d.e"), None, "through an array");
        assert_eq!(doc.get("d[*].e"), None);
        assert_eq!(doc.get("d[*]"), None);
        assert_eq!(Document::Int(1).get("a"), None);
        // Not even a field of that name, as a filter wouldn't read it.
        let named: Document = r#"{"d[*]": 1}"#.parse().unwrap();
        assert_eq!(named.get("d[*]"), None);
    }

    fn roundtrip(doc: Document) {
        let bytes = encode_document(&doc);
        let (decoded, rest) = decode_document(&bytes).unwrap();
        assert_eq!(decoded, doc);
        assert!(rest.is_empty(), "decode must consume every encoded byte");
        // A stored document's own `_id` is left out: the cell holds it.
        let without_id = match &doc {
            Document::Object(map) => Document::Object(Map::from_iter(
                map.iter()
                    .filter(|(key, _)| *key != ID_FIELD)
                    .map(|(k, v)| (k.clone(), v.clone())),
            )),
            other => other.clone(),
        };
        for names in [FieldNames::default(), some_names()] {
            let stored = encode_stored(&doc, &names);
            assert_eq!(decode_stored(&stored, &names).unwrap(), without_id);
            let mut rest = &stored[..];
            skip_value(&mut rest, Document::MAX_NESTING, &names).unwrap();
            assert!(rest.is_empty(), "skip must pass every stored byte");
        }
    }

    /// File format 14: a stored document's keys are numbers where the
    /// dictionary has them, its ints, lengths and counts varints; every
    /// int, at both ends of its range, and keys past a byte's numbers,
    /// read back.
    #[test]
    fn a_stored_document_reads_back_and_is_smaller() {
        let mut names = FieldNames::default();
        for n in 0..300 {
            names.add(&format!("f{n}"));
        }
        let ints = [0, 1, -1, 63, -64, 64, -65, i64::MAX, i64::MIN, 1 << 40];
        let doc = Document::Object(Map::from_iter(
            ints.iter()
                .enumerate()
                .map(|(n, v)| (format!("f{}", n * 33), Document::Int(*v)))
                .chain([
                    ("not named".to_string(), Document::String("x".repeat(200))),
                    ("f299".to_string(), Document::Binary(vec![7; 130])),
                    ("f1".to_string(), Document::Array(vec![Document::Null; 128])),
                ]),
        ));
        let stored = encode_stored(&doc, &names);
        assert_eq!(decode_stored(&stored, &names).unwrap(), doc);
        assert!(stored.len() < encode_document(&doc).len());
        // The example in the plan: 106 bytes before, 50 after.
        let example: Document = r#"{"server": "web-1", "cpu": 42, "load": 0.7,
            "tags": ["prod", "eu"], "at": {"$date": "2026-11-06T22:15:36Z"}}"#
            .parse()
            .unwrap();
        let mut learned = FieldNames::default();
        for name in learned.unknown(&example) {
            learned.add(&name);
        }
        assert_eq!(encode_stored(&example, &FieldNames::default()).len(), 69);
        assert_eq!(encode_stored(&example, &learned).len(), 50);
        assert_eq!(encode_document(&example).len(), 106);
    }

    /// A key numbered past the dictionary is damage, not a panic.
    #[test]
    fn a_key_past_the_dictionary_is_damage() {
        let doc: Document = r#"{"n": 1}"#.parse().unwrap();
        let stored = encode_stored(&doc, &some_names());
        let err = decode_stored(&stored, &FieldNames::default()).unwrap_err();
        assert!(err.to_string().contains("field name 1, of 0"), "{err}");
    }

    /// SPEC §99.2: decoding some fields is damaged exactly when decoding
    /// the whole document is, with the same message — in the fields it
    /// takes and in those it skips.
    #[test]
    fn some_fields_are_damaged_when_the_whole_is() {
        let doc: Document =
            r#"{"a":-1,"s":"héllo","n":[1,{"x":"y"}],"b":true,"t":"txt","bin":null}"#
                .parse()
                .unwrap();
        // Some keys numbered, some inline; and the encoding before 14.
        let names = some_names();
        let id = DocId([3; 16]);
        let mut cases = Vec::new();
        for encoded in [encode_stored(&doc, &names), encode_document(&doc)] {
            cases.extend((0..encoded.len()).map(|n| encoded[..n].to_vec()));
            for at in 0..encoded.len() {
                for byte in [0xFF, 0x80, 0x0A, 0x00, 0x02, 0x01] {
                    let mut altered = encoded.clone();
                    altered[at] = byte;
                    cases.push(altered);
                }
            }
        }
        for bytes in cases {
            let whole = decode_stored(&bytes, &names)
                .map(|_| ())
                .map_err(|e| e.to_string());
            for fields in [&["a"][..], &["t"], &["n", "_id"], &["missing"]] {
                let some = decode_fields(&bytes, id, fields, &names)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                assert_eq!(some, whole, "{fields:?} of {bytes:?}");
            }
        }
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

    /// SPEC §63: an id and its string go back and forth; the other ways
    /// UUIDs are written parse too; anything else is an error that says
    /// which string.
    #[test]
    fn ids_parse_from_their_strings() {
        let id = crate::id::IdGenerator::generate(&crate::id::UuidV7Generator);
        assert_eq!(id.to_string().parse::<DocId>(), Ok(id));
        assert_eq!(DocId::NIL.to_string().parse::<DocId>(), Ok(DocId::NIL));

        let id = DocId([
            0x01, 0x92, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f, 0x90, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5,
            0xf6, 0x07,
        ]);
        let text = "01923a4b-5c6d-7e8f-90a1-b2c3d4e5f607";
        assert_eq!(id.to_string(), text);
        for form in [
            text,
            "01923a4b5c6d7e8f90a1b2c3d4e5f607",
            "{01923a4b-5c6d-7e8f-90a1-b2c3d4e5f607}",
            "urn:uuid:01923a4b-5c6d-7e8f-90a1-b2c3d4e5f607",
            "01923A4B-5C6D-7E8F-90A1-B2C3D4E5F607",
        ] {
            assert_eq!(form.parse::<DocId>(), Ok(id), "{form}");
        }

        for bad in [
            "",
            "abc",
            "01923a4b-5c6d-7e8f-90a1-b2c3d4e5f60",
            "01923a4b-5c6d-7e8f-90a1-b2c3d4e5f6077",
        ] {
            let error = bad.parse::<DocId>().unwrap_err();
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("{bad:?} is not a document id: ")),
                "{message}"
            );
        }
    }

    /// SPEC §69: a date-time's encoding keeps it to the nanosecond,
    /// before 1970 too; nanoseconds of a second or more are damage.
    #[test]
    fn datetimes_roundtrip_and_bad_nanoseconds_are_damage() {
        let at = |secs, nanos| crate::DateTime::from_unix(secs, nanos).unwrap();
        for time in [
            at(0, 0),
            at(1_790_000_000, 123_456_789),
            at(-1, 999_999_999),
        ] {
            roundtrip(Document::DateTime(time));
        }
        let mut bytes = encode_document(&Document::DateTime(at(5, 0)));
        bytes[9..13].copy_from_slice(&1_000_000_000u32.to_le_bytes());
        let error = decode_document(&bytes).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("date-time"), "{error}");
        assert!(decode_document(&bytes[..12]).is_err(), "cut short");
    }

    /// SPEC §173: dates and times of day keep their value, as varints,
    /// in both encodings and through a skip; one past its range is
    /// damage, and so is one cut short.
    #[test]
    fn dates_and_times_roundtrip_and_out_of_range_is_damage() {
        const NANOS_PER_DAY: u64 = 86_400 * 1_000_000_000;
        let first = crate::DateTime::EARLIEST.date();
        let last = crate::DateTime::from_unix(i64::MAX, 0).unwrap().date();
        for date in [
            Date::UNIX_EPOCH,
            "2026-11-07".parse().unwrap(),
            "1969-12-31".parse().unwrap(),
            first,
            last,
        ] {
            roundtrip(Document::Date(date));
        }
        for nanos in [0, 1, 52_200_000_000_000, NANOS_PER_DAY - 1] {
            roundtrip(Document::Time(
                Time::from_nanos_since_midnight(nanos).unwrap(),
            ));
        }
        // 1970-01-02 is two bytes: the tag and a zigzag of 1.
        let today = Document::Date(Date::from_days_since_epoch(1).unwrap());
        assert_eq!(encode_document(&today), [TAG_DATE, 2]);
        assert_eq!(
            encode_document(&Document::Date(Date::from_days_since_epoch(-1).unwrap())),
            [TAG_DATE, 1]
        );

        let damaged = |tag: u8, value: u64, what: &str| {
            let mut bytes = vec![tag];
            put_varint(value, &mut bytes);
            let error = decode_document(&bytes).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(what), "{error}");
            let mut rest = &bytes[..];
            let names = FieldNames::default();
            assert!(skip_value(&mut rest, 1, &names).is_err(), "skipped: {what}");
        };
        let past = |days: i64| ((days << 1) ^ (days >> 63)) as u64;
        damaged(TAG_DATE, past(last.days_since_epoch() + 1), "a date");
        damaged(TAG_DATE, past(first.days_since_epoch() - 1), "a date");
        damaged(TAG_TIME, NANOS_PER_DAY, "a time of day");
        damaged(TAG_TIME, u64::MAX, "a time of day");
        assert!(decode_document(&[TAG_DATE]).is_err(), "cut short");
        assert!(decode_document(&[TAG_TIME, 0x80]).is_err(), "cut short");
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
        let mut entries = Map::new();
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
        let mut entries = Map::new();
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
        let deepest = nested(Document::MAX_NESTING, Document::Int(1));
        assert!(!deepest.nests_too_deep());
        let bytes = encode_document(&deepest);
        assert_eq!(decode_document(&bytes).unwrap().0, deepest);

        let too_deep = nested(Document::MAX_NESTING + 1, Document::Int(1));
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
        let half = (0..Document::MAX_NESTING / 2).fold(Document::Null, |inner, _| tagged(inner));
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
        let inner = (0..Document::MAX_NESTING / 2 - 1)
            .fold(Document::Array(vec![]), |inner, _| tagged(inner));
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
        // And a varint count or length past what's there, to the top of
        // a u64.
        for tag in [TAG_LIST, TAG_FIELDS, TAG_TEXT, TAG_BYTES] {
            let mut bytes = vec![tag];
            crate::decode::put_varint(u64::MAX, &mut bytes);
            bytes.push(TAG_NULL);
            assert!(decode_stored(&bytes, &some_names()).is_err(), "{tag}");
            let mut rest = &bytes[..];
            assert!(skip_value(&mut rest, 3, &some_names()).is_err(), "{tag}");
        }
    }
}
