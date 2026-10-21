//! Typed reads straight from a stored document's bytes (SPEC §99.4): a
//! serde `Deserializer` over `document::encode_stored`'s encoding, with
//! the cell's id as `_id`, first, as `with_id` puts it. No `Document` is
//! built on the way, and a field `T` doesn't have is skipped
//! (`ignored_any`).
//!
//! It gives `T` exactly what the serde bridge gives it from the decoded
//! `Document` (`serde_bridge`, `Deserializer for Document`): the same
//! visitor calls for scalars, strings, binary, ids, arrays, objects and
//! `Option`, so the same values and the same errors. The rare shapes —
//! an enum, a date-time seen as a map, a document that isn't an object —
//! decode that value into a `Document` and hand it to the bridge itself,
//! so they can't drift from it.
//!
//! Damage reads as damage, as in the full decode: every byte of the
//! document is decoded or skipped with the same checks (`skip_value`),
//! the nesting limit included, and a document that is both damaged and
//! of the wrong type for `T` is reported as damaged.

use crate::decode::{take_array, take_u8, take_u32};
use crate::document::{
    self, DocId, Document, ID_FIELD, TAG_ARRAY, TAG_BINARY, TAG_BOOL, TAG_DATETIME, TAG_FLOAT,
    TAG_ID, TAG_INT, TAG_NULL, TAG_OBJECT, TAG_STRING,
};
use crate::serde_bridge::DocumentError;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, IntoDeserializer, MapAccess, SeqAccess,
    Visitor,
};
use std::fmt;

/// `T` from a stored document's encoding and its cell's id.
pub(crate) fn from_stored<T: DeserializeOwned>(
    encoded: &[u8],
    id: DocId,
) -> Result<T, StoredError> {
    let mut bytes = encoded;
    let value = Value {
        bytes: &mut bytes,
        levels: Document::MAX_NESTING,
        id: Some(id),
    };
    match T::deserialize(value) {
        // Damage first, as the full decode finds it before any value
        // reaches `T`.
        Err(StoredError::Message(message)) => {
            let mut whole = encoded;
            document::skip_value(&mut whole, Document::MAX_NESTING)
                .map_err(StoredError::Damaged)?;
            Err(StoredError::Message(message))
        }
        other => other,
    }
}

/// Why a typed read failed: damage, as the full decode reports it
/// (`Error::Io`, `InvalidData`), or a value that doesn't fit `T`, as the
/// bridge reports it (`Error::Document`).
#[derive(Debug)]
pub(crate) enum StoredError {
    Damaged(std::io::Error),
    Message(DocumentError),
}

impl fmt::Display for StoredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoredError::Damaged(e) => e.fmt(f),
            StoredError::Message(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StoredError {}

impl de::Error for StoredError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        StoredError::Message(DocumentError::custom(msg))
    }
}

impl From<StoredError> for crate::Error {
    fn from(error: StoredError) -> Self {
        match error {
            StoredError::Damaged(e) => crate::Error::Io(e),
            StoredError::Message(e) => crate::Error::Document(e),
        }
    }
}

fn damaged(e: std::io::Error) -> StoredError {
    StoredError::Damaged(e)
}

/// One encoded value at the front of `bytes`, which moves past it.
/// `levels` as in `document::decode_value`. `id`, only for the document
/// itself: its `_id`, which goes first.
struct Value<'a, 'b> {
    bytes: &'b mut &'a [u8],
    levels: usize,
    id: Option<DocId>,
}

impl Value<'_, '_> {
    fn tag(&self) -> Result<u8, StoredError> {
        let mut peek = *self.bytes;
        take_u8(&mut peek, "a document's type tag").map_err(damaged)
    }

    /// The value as a `Document`, as the full decode makes it — for the
    /// shapes handed to the bridge.
    fn decoded(self) -> Result<Document, StoredError> {
        let value = document::decode_value(self.bytes, self.levels).map_err(damaged)?;
        Ok(match self.id {
            Some(id) => document::with_id(value, id),
            None => value,
        })
    }

    /// The document itself, and not an object: only damage since §95,
    /// and handed to the bridge whole.
    fn not_an_object(&self) -> Result<bool, StoredError> {
        Ok(self.id.is_some() && self.tag()? != TAG_OBJECT)
    }

    fn too_deep(&self) -> Result<(), StoredError> {
        if self.levels == 0 {
            return Err(damaged(crate::decode::corrupt(format_args!(
                "a document nests deeper than {} levels",
                Document::MAX_NESTING
            ))));
        }
        Ok(())
    }
}

impl<'de> Deserializer<'de> for Value<'_, '_> {
    type Error = StoredError;

    /// As the bridge's `deserialize_any` for each kind of `Document`.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, StoredError> {
        let tag = self.tag()?;
        if tag == TAG_DATETIME || self.not_an_object()? {
            // A date-time is a two-field map to `deserialize_any`; a
            // document that isn't an object is only damage since §95.
            return self
                .decoded()?
                .deserialize_any(visitor)
                .map_err(StoredError::Message);
        }
        let bytes = self.bytes;
        take_u8(bytes, "a document's type tag").map_err(damaged)?;
        match tag {
            TAG_NULL => visitor.visit_unit(),
            TAG_BOOL => visitor.visit_bool(take_u8(bytes, "a bool").map_err(damaged)? != 0),
            TAG_INT => {
                let int = i64::from_le_bytes(take_array(bytes, "an int").map_err(damaged)?);
                visitor.visit_i64(int)
            }
            TAG_FLOAT => {
                let float = f64::from_le_bytes(take_array(bytes, "a float").map_err(damaged)?);
                visitor.visit_f64(float)
            }
            TAG_STRING => {
                visitor.visit_str(document::take_str(bytes, "a string").map_err(damaged)?)
            }
            TAG_BINARY => {
                let binary = document::take_len_prefixed(bytes, "binary").map_err(damaged)?;
                visitor.visit_byte_buf(binary.to_vec())
            }
            // As its text, as the bridge hands it over (SPEC §13.5),
            // formatted on the stack.
            TAG_ID => {
                let id = take_array::<16>(bytes, "an id").map_err(damaged)?;
                let mut text = [0u8; uuid::fmt::Hyphenated::LENGTH];
                visitor.visit_str(
                    uuid::Uuid::from_bytes(id)
                        .hyphenated()
                        .encode_lower(&mut text),
                )
            }
            TAG_ARRAY => {
                Value { bytes, ..self }.too_deep()?;
                let remaining = take_u32(bytes, "an array's length").map_err(damaged)?;
                let mut items = Items {
                    bytes,
                    remaining,
                    levels: self.levels - 1,
                };
                let value = visitor.visit_seq(&mut items)?;
                items.finish()?;
                Ok(value)
            }
            TAG_OBJECT => {
                Value { bytes, ..self }.too_deep()?;
                let remaining = take_u32(bytes, "an object's length").map_err(damaged)?;
                let mut entries = Entries {
                    bytes,
                    remaining,
                    levels: self.levels - 1,
                    id: self.id,
                    top: self.id.is_some(),
                    id_value: None,
                    value_pending: false,
                };
                let value = visitor.visit_map(&mut entries)?;
                entries.finish()?;
                Ok(value)
            }
            other => Err(damaged(crate::decode::corrupt(format_args!(
                "unknown document type tag {other}"
            )))),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, StoredError> {
        if self.not_an_object()? {
            return self
                .decoded()?
                .deserialize_option(visitor)
                .map_err(StoredError::Message);
        }
        if self.tag()? == TAG_NULL {
            take_u8(self.bytes, "a document's type tag").map_err(damaged)?;
            return visitor.visit_none();
        }
        visitor.visit_some(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, StoredError> {
        self.decoded()?
            .deserialize_enum(name, variants, visitor)
            .map_err(StoredError::Message)
    }

    /// A date-time as its text (SPEC §69), as the bridge reads it.
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, StoredError> {
        if self.tag()? == TAG_DATETIME || self.not_an_object()? {
            return self
                .decoded()?
                .deserialize_str(visitor)
                .map_err(StoredError::Message);
        }
        self.deserialize_any(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, StoredError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, StoredError> {
        if name == crate::datetime::DATETIME_NEWTYPE {
            return self.deserialize_str(visitor);
        }
        self.deserialize_any(visitor)
    }

    /// Skipped, with the checks a decode makes, and nothing built.
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, StoredError> {
        document::skip_value(self.bytes, self.levels).map_err(damaged)?;
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char
        bytes byte_buf unit unit_struct seq tuple
        tuple_struct map struct identifier
    }
}

/// An array's items, `remaining` still to come.
struct Items<'a, 'b> {
    bytes: &'b mut &'a [u8],
    remaining: u32,
    levels: usize,
}

impl Items<'_, '_> {
    /// Past the items the visitor didn't take, checked.
    fn finish(&mut self) -> Result<(), StoredError> {
        while self.remaining > 0 {
            self.remaining -= 1;
            document::skip_value(self.bytes, self.levels).map_err(damaged)?;
        }
        Ok(())
    }
}

impl<'de> SeqAccess<'de> for Items<'_, '_> {
    type Error = StoredError;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, StoredError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let value = Value {
            bytes: self.bytes,
            levels: self.levels,
            id: None,
        };
        seed.deserialize(value).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining as usize)
    }
}

/// An object's entries, `remaining` still to come — for the document
/// itself (`top`), its `_id` first, and a stored `_id` passed over, as
/// `with_id` replaces it.
struct Entries<'a, 'b> {
    bytes: &'b mut &'a [u8],
    remaining: u32,
    levels: usize,
    /// The `_id` whose key is still to come.
    id: Option<DocId>,
    top: bool,
    /// The `_id` whose value is next.
    id_value: Option<DocId>,
    /// A key was handed out, its value not taken yet.
    value_pending: bool,
}

impl Entries<'_, '_> {
    /// Past the entries the visitor didn't take, checked.
    fn finish(&mut self) -> Result<(), StoredError> {
        if std::mem::take(&mut self.value_pending) {
            document::skip_value(self.bytes, self.levels).map_err(damaged)?;
        }
        while self.remaining > 0 {
            self.remaining -= 1;
            document::take_str(self.bytes, "a key").map_err(damaged)?;
            document::skip_value(self.bytes, self.levels).map_err(damaged)?;
        }
        Ok(())
    }
}

impl<'de> MapAccess<'de> for Entries<'_, '_> {
    type Error = StoredError;

    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, StoredError> {
        if let Some(id) = self.id.take() {
            self.id_value = Some(id);
            return seed.deserialize(ID_FIELD.into_deserializer()).map(Some);
        }
        if std::mem::take(&mut self.value_pending) {
            document::skip_value(self.bytes, self.levels).map_err(damaged)?;
        }
        while self.remaining > 0 {
            self.remaining -= 1;
            let key = document::take_str(self.bytes, "a key").map_err(damaged)?;
            if self.top && key == ID_FIELD {
                document::skip_value(self.bytes, self.levels).map_err(damaged)?;
                continue;
            }
            self.value_pending = true;
            return seed.deserialize(key.into_deserializer()).map(Some);
        }
        Ok(None)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<S::Value, StoredError> {
        if let Some(id) = self.id_value.take() {
            return seed
                .deserialize(Document::Id(id))
                .map_err(StoredError::Message);
        }
        self.value_pending = false;
        let value = Value {
            bytes: self.bytes,
            levels: self.levels,
            id: None,
        };
        seed.deserialize(value)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.remaining as usize + self.id.is_some() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::DateTime;
    use crate::document::{decode_document, encode_stored, with_id};
    use crate::serde_bridge::from_document;
    use crate::testing::XorShift;
    use serde::Deserialize;
    use std::collections::{BTreeMap, HashMap};
    use std::time::SystemTime;

    /// What the bridge makes of `encoded`: decoded whole, `_id` added,
    /// then converted (SPEC §13).
    fn bridged<T: DeserializeOwned>(encoded: &[u8], id: DocId) -> Result<T, String> {
        let (doc, _) = decode_document(encoded).map_err(|e| format!("damaged: {e}"))?;
        from_document(with_id(doc, id)).map_err(|e| e.to_string())
    }

    fn direct<T: DeserializeOwned>(encoded: &[u8], id: DocId) -> Result<T, String> {
        from_stored(encoded, id).map_err(|e| match e {
            StoredError::Damaged(e) => format!("damaged: {e}"),
            StoredError::Message(e) => e.to_string(),
        })
    }

    /// The deserializer and the bridge agree on `doc` as a `T`: the same
    /// value, or the same error.
    fn agree<T: DeserializeOwned + PartialEq + std::fmt::Debug>(doc: &Document) -> bool {
        agree_on::<T>(&encode_stored(doc))
    }

    fn agree_on<T: DeserializeOwned + PartialEq + std::fmt::Debug>(encoded: &[u8]) -> bool {
        let id = DocId([0x5A; 16]);
        let (want, got) = (bridged::<T>(encoded, id), direct::<T>(encoded, id));
        assert_eq!(got, want, "{} from {encoded:?}", std::any::type_name::<T>());
        want.is_ok()
    }

    /// Reads a map's first entry and stops, as a visitor may.
    #[derive(Debug, PartialEq)]
    struct FirstEntry(Option<(String, i64)>);

    impl<'de> Deserialize<'de> for FirstEntry {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct First;
            impl<'de> Visitor<'de> for First {
                type Value = FirstEntry;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a map")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<FirstEntry, A::Error> {
                    Ok(FirstEntry(map.next_entry()?))
                }
            }
            deserializer.deserialize_map(First)
        }
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Early {
        m: Option<FirstEntry>,
        tuple: Option<(i64, String)>,
        i: Option<i64>,
    }

    /// A visitor that stops early in a map or an array leaves the rest
    /// to be passed over, so what follows reads as it should; a stored
    /// `_id`, as files before §59 have, gives way to the cell's id.
    #[test]
    fn what_a_visitor_leaves_and_a_stored_id_are_passed_over() {
        let doc: Document = r#"{"m":{"a":1,"b":2},"tuple":[1,"z",[3]],"i":5}"#.parse().unwrap();
        assert!(agree::<Early>(&doc));
        let early: Early = direct(&encode_stored(&doc), DocId([1; 16])).unwrap();
        assert_eq!(early.i, Some(5));
        assert_eq!(early.m, Some(FirstEntry(Some(("a".into(), 1)))));
        let old: Document = r#"{"_id":"stale","i":1}"#.parse().unwrap();
        let encoded = crate::document::encode_document(&old);
        assert!(agree_on::<Everything>(&encoded));
        assert!(agree_on::<Strict>(&encoded));
    }

    #[derive(Debug, PartialEq, Deserialize)]
    enum Kind {
        Plain,
        Wrapped(i64),
        Pair(i64, String),
        Shaped { x: i64 },
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(tag = "t")]
    enum Tagged {
        A { x: Option<i64> },
        B,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(untagged)]
    enum Anything {
        Nothing,
        Flag(bool),
        Int(i64),
        Float(f64),
        Text(String),
        Bytes(Vec<u8>),
        List(Vec<Anything>),
        Map(BTreeMap<String, Anything>),
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Newtype(i64);

    #[derive(Debug, PartialEq, Deserialize)]
    struct Everything {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        b: Option<bool>,
        i: Option<i64>,
        u: Option<u8>,
        f: Option<f32>,
        s: Option<String>,
        c: Option<char>,
        bin: Option<Vec<u8>>,
        items: Option<Vec<i64>>,
        map: Option<HashMap<String, i64>>,
        when: Option<DateTime>,
        st: Option<SystemTime>,
        e: Option<Kind>,
        t: Option<Tagged>,
        n: Option<Newtype>,
        tuple: Option<(i64, String)>,
        any: Option<Anything>,
        other: Option<DocId>,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        #[serde(rename = "_id")]
        id: DocId,
        i: i64,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Summary {
        s: String,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    struct Flattened {
        #[serde(flatten)]
        rest: HashMap<String, Anything>,
    }

    const FIELDS: [&str; 18] = [
        "b", "i", "u", "f", "s", "c", "bin", "items", "map", "when", "st", "e", "t", "n", "tuple",
        "any", "other", "_id",
    ];

    fn random_value(rng: &mut XorShift, depth: usize) -> Document {
        match rng.below(if depth == 0 { 10 } else { 13 }) {
            0 => Document::Null,
            1 => Document::Bool(rng.below(2) == 0),
            2 => Document::Int(rng.below(600) as i64 - 300),
            3 => Document::Float((rng.below(1000) as f64 - 500.0) / 8.0),
            4 => Document::String(
                ["", "x", "Plain", "B", "A", "héllo", "Wrapped"][rng.below(7)].into(),
            ),
            5 => Document::Binary((0..rng.below(4)).map(|i| i as u8).collect()),
            6 => Document::Id(DocId([rng.below(256) as u8; 16])),
            7 => Document::DateTime(DateTime::from_unix(rng.below(1 << 30) as i64, 5).unwrap()),
            8 => Document::String("9".into()),
            9 => Document::Int(7),
            10 => Document::Array(
                (0..rng.below(4))
                    .map(|_| random_value(rng, depth - 1))
                    .collect(),
            ),
            _ => random_object(rng, depth - 1),
        }
    }

    fn random_object(rng: &mut XorShift, depth: usize) -> Document {
        let mut map = crate::Map::new();
        for _ in 0..rng.below(5) {
            let key = FIELDS[rng.below(FIELDS.len())].to_string();
            map.insert(key, random_value(rng, depth));
        }
        Document::Object(map)
    }

    /// SPEC §99.4: for random documents, as every kind of target, the
    /// deserializer gives what the bridge gives — values and errors.
    #[test]
    fn the_deserializer_agrees_with_the_bridge() {
        let mut rng = XorShift(0x6A09_E667_F3BC_C908);
        let mut fitted = [0usize; 7];
        for _ in 0..20_000 {
            let doc = random_object(&mut rng, 3);
            fitted[0] += agree::<Everything>(&doc) as usize;
            fitted[1] += agree::<Strict>(&doc) as usize;
            fitted[2] += agree::<Summary>(&doc) as usize;
            fitted[3] += agree::<Flattened>(&doc) as usize;
            fitted[4] += agree::<Anything>(&doc) as usize;
            fitted[5] += agree::<BTreeMap<String, Anything>>(&doc) as usize;
            fitted[6] += agree::<Option<Everything>>(&doc) as usize;
        }
        // Each target fits some; the strict ones fail many too, so the
        // errors are compared as well as the values.
        assert!(fitted.iter().all(|&n| n > 0), "{fitted:?}");
        assert!(fitted[..3].iter().all(|&n| n < 10_000), "{fitted:?}");
    }

    /// Shapes chosen by hand: each enum variant, a date-time each way,
    /// ids, binary, a stored `_id` that the cell's id replaces.
    #[test]
    fn chosen_shapes_agree_with_the_bridge() {
        let parse = |json: &str| -> Document { json.parse().unwrap() };
        let time = Document::DateTime(DateTime::from_unix(1_790_000_000, 42).unwrap());
        let object = |pairs: Vec<(&str, Document)>| {
            Document::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        };
        for doc in [
            parse(r#"{"e":"Plain"}"#),
            parse(r#"{"e":{"Wrapped":5}}"#),
            parse(r#"{"e":{"Pair":[1,"a"]}}"#),
            parse(r#"{"e":{"Shaped":{"x":1}}}"#),
            parse(r#"{"e":{"Plain":1,"Wrapped":2}}"#),
            parse(r#"{"e":{}}"#),
            parse(r#"{"e":5}"#),
            parse(r#"{"t":{"t":"A","x":3}}"#),
            parse(r#"{"t":{"t":"B"}}"#),
            parse(r#"{"n":4,"tuple":[1,"z"],"c":"q"}"#),
            parse(r#"{"c":"too long"}"#),
            object(vec![
                ("when", time.clone()),
                ("st", time.clone()),
                ("any", time.clone()),
            ]),
            object(vec![("s", time.clone())]),
            object(vec![
                ("other", Document::Id(DocId([3; 16]))),
                ("bin", Document::Binary(vec![1, 2])),
            ]),
            object(vec![
                ("_id", Document::String("stale".into())),
                ("i", Document::Int(1)),
            ]),
        ] {
            agree::<Everything>(&doc);
            agree::<Strict>(&doc);
            agree::<Anything>(&doc);
            agree::<Flattened>(&doc);
        }
        // Not an object (only damage since §95): handed to the bridge.
        for doc in [Document::Int(5), Document::Null, time] {
            agree::<Option<Everything>>(&doc);
            agree::<Anything>(&doc);
            agree::<String>(&doc);
        }
    }

    /// SPEC §99.4: damage reads as damage, exactly where the full decode
    /// finds it — in what `T` takes, in what it skips, and before a
    /// value that doesn't fit `T`.
    #[test]
    fn damage_is_found_as_the_full_decode_finds_it() {
        let mut rng = XorShift(0x3C6E_F372_FE94_F82B);
        for _ in 0..300 {
            let doc = random_object(&mut rng, 3);
            let encoded = encode_stored(&doc);
            let id = DocId([1; 16]);
            let mut cases: Vec<Vec<u8>> =
                (0..encoded.len()).map(|n| encoded[..n].to_vec()).collect();
            for at in 0..encoded.len() {
                for byte in [0xFF, 0x80, 0x0A] {
                    let mut altered = encoded.clone();
                    altered[at] = byte;
                    cases.push(altered);
                }
            }
            for bytes in cases {
                let full = decode_document(&bytes).map(|(doc, _)| doc);
                for got in [
                    direct::<Everything>(&bytes, id).err(),
                    direct::<Summary>(&bytes, id).err(),
                    direct::<Anything>(&bytes, id).err(),
                    direct::<serde::de::IgnoredAny>(&bytes, id)
                        .err()
                        .filter(|_| true),
                ] {
                    match &full {
                        Err(e) => assert_eq!(got, Some(format!("damaged: {e}")), "{bytes:?}"),
                        Ok(_) => {
                            assert!(!got.is_some_and(|e| e.starts_with("damaged")), "{bytes:?}")
                        }
                    }
                }
            }
        }
    }

    /// The nesting limit (SPEC §56) holds in what is read and in what is
    /// skipped.
    #[test]
    fn nesting_past_the_limit_is_damage_read_or_skipped() {
        let mut deep = Document::Int(1);
        for _ in 0..Document::MAX_NESTING {
            deep = Document::Array(vec![deep]);
        }
        let mut map = crate::Map::new();
        map.insert("any".to_string(), deep);
        let encoded = encode_stored(&Document::Object(map));
        let id = DocId([2; 16]);
        let full = decode_document(&encoded).unwrap_err();
        let want = format!("damaged: {full}");
        assert_eq!(direct::<Everything>(&encoded, id).unwrap_err(), want);
        assert_eq!(direct::<Summary>(&encoded, id).unwrap_err(), want);
    }
}
