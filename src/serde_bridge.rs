//! Lets any `T: Serialize + DeserializeOwned` convert to/from `Document`,
//! by driving `T`'s own (usually derive-generated) serde impl against a
//! `Document`-shaped `Serializer`, and letting `Document` itself act as a
//! `Deserializer`. This is the same technique `serde_json` uses for
//! `serde_json::Value` and `bson` uses for `bson::Bson` — `Document` is
//! playing the same role here that a generic "value" type plays for those
//! crates.

use crate::document::Document;
use indexmap::IndexMap;
use serde::de::{
    self, Deserialize, DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, VariantAccess,
    Visitor,
    value::{MapDeserializer, SeqDeserializer},
};
use serde::ser::{self, Serialize, Serializer};
use std::fmt;

/// Converts any `T: Serialize` into a `Document` by driving `T`'s
/// `Serialize` impl against `DocumentSerializer` instead of, say,
/// `serde_json`'s serializer.
pub fn to_document<T: Serialize>(value: &T) -> Result<Document, DocumentError> {
    value.serialize(DocumentSerializer)
}

/// Converts a `Document` back into any `T: Deserialize`, by letting
/// `Document` itself act as the `Deserializer`.
pub fn from_document<T>(doc: Document) -> Result<T, DocumentError>
where
    T: for<'de> Deserialize<'de>,
{
    T::deserialize(doc)
}

/// The bridge's error type. Serde only requires `Error: serde::ser::Error
/// + serde::de::Error`, each of which needs just one constructor —
/// `custom(msg)` — so everything funnels into one `Message` variant.
/// Unlike a text/byte format's parser, there's no byte offset or line
/// number to report here; every failure is already a description of
/// *what* went wrong (an unsupported type, a malformed enum shape), so a
/// richer error enum wouldn't add anything a `String` doesn't already say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentError(String);

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DocumentError {}

impl DocumentError {
    // A plain inherent method, so the rest of this module can just call
    // `DocumentError::custom(...)` without needing either serde error
    // trait in scope (and without the two trait impls below — which serde
    // itself calls via generic dispatch — becoming ambiguous with each
    // other for that call).
    fn custom(msg: impl fmt::Display) -> Self {
        DocumentError(msg.to_string())
    }
}

impl ser::Error for DocumentError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DocumentError::custom(msg)
    }
}

impl de::Error for DocumentError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DocumentError::custom(msg)
    }
}

// ---------- Serialize: T -> Document ----------

struct DocumentSerializer;

impl Serializer for DocumentSerializer {
    type Ok = Document;
    type Error = DocumentError;

    type SerializeSeq = SerializeVec;
    type SerializeTuple = SerializeVec;
    type SerializeTupleStruct = SerializeVec;
    type SerializeTupleVariant = SerializeTupleVariant;
    type SerializeMap = SerializeMapImpl;
    type SerializeStruct = SerializeMapImpl;
    type SerializeStructVariant = SerializeStructVariant;

    fn serialize_bool(self, v: bool) -> Result<Document, DocumentError> {
        Ok(Document::Bool(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Document, DocumentError> {
        Ok(Document::Int(v as i64))
    }
    fn serialize_i16(self, v: i16) -> Result<Document, DocumentError> {
        Ok(Document::Int(v as i64))
    }
    fn serialize_i32(self, v: i32) -> Result<Document, DocumentError> {
        Ok(Document::Int(v as i64))
    }
    fn serialize_i64(self, v: i64) -> Result<Document, DocumentError> {
        Ok(Document::Int(v))
    }

    fn serialize_u8(self, v: u8) -> Result<Document, DocumentError> {
        Ok(Document::Int(v as i64))
    }
    fn serialize_u16(self, v: u16) -> Result<Document, DocumentError> {
        Ok(Document::Int(v as i64))
    }
    fn serialize_u32(self, v: u32) -> Result<Document, DocumentError> {
        Ok(Document::Int(v as i64))
    }
    fn serialize_u64(self, v: u64) -> Result<Document, DocumentError> {
        // Document::Int is an i64 — a u64 bigger than i64::MAX genuinely
        // can't round-trip through it, so this is a real error, not a
        // silent truncation.
        i64::try_from(v)
            .map(Document::Int)
            .map_err(|_| DocumentError::custom(format!("{v} doesn't fit in an i64")))
    }

    fn serialize_f32(self, v: f32) -> Result<Document, DocumentError> {
        Ok(Document::Float(v as f64))
    }
    fn serialize_f64(self, v: f64) -> Result<Document, DocumentError> {
        Ok(Document::Float(v))
    }

    fn serialize_char(self, v: char) -> Result<Document, DocumentError> {
        Ok(Document::String(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Document, DocumentError> {
        Ok(Document::String(v.to_string()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Document, DocumentError> {
        Ok(Document::Binary(v.to_vec()))
    }

    fn serialize_none(self) -> Result<Document, DocumentError> {
        Ok(Document::Null)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Document, DocumentError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Document, DocumentError> {
        Ok(Document::Null)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Document, DocumentError> {
        Ok(Document::Null)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Document, DocumentError> {
        // The common, "externally tagged" enum representation (same
        // default serde_json uses): a unit variant is just its own name.
        Ok(Document::String(variant.to_string()))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Document, DocumentError> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Document, DocumentError> {
        // {"Variant": value} — one-entry object, same shape deserialize_enum
        // expects back.
        let mut map = IndexMap::new();
        map.insert(variant.to_string(), value.serialize(DocumentSerializer)?);
        Ok(Document::Object(map))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<SerializeVec, DocumentError> {
        Ok(SerializeVec {
            items: Vec::with_capacity(len.unwrap_or(0)),
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<SerializeVec, DocumentError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SerializeVec, DocumentError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<SerializeTupleVariant, DocumentError> {
        Ok(SerializeTupleVariant {
            variant: variant.to_string(),
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<SerializeMapImpl, DocumentError> {
        Ok(SerializeMapImpl {
            map: IndexMap::new(),
            next_key: None,
        })
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SerializeMapImpl, DocumentError> {
        Ok(SerializeMapImpl {
            map: IndexMap::with_capacity(len),
            next_key: None,
        })
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<SerializeStructVariant, DocumentError> {
        Ok(SerializeStructVariant {
            variant: variant.to_string(),
            map: IndexMap::with_capacity(len),
        })
    }
}

/// Backs `SerializeSeq`/`SerializeTuple`/`SerializeTupleStruct` — all three
/// are "a list of values with no field names," so one `Vec<Document>`
/// accumulator serves all three.
struct SerializeVec {
    items: Vec<Document>,
}

impl ser::SerializeSeq for SerializeVec {
    type Ok = Document;
    type Error = DocumentError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DocumentError> {
        self.items.push(value.serialize(DocumentSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Document, DocumentError> {
        Ok(Document::Array(self.items))
    }
}

impl ser::SerializeTuple for SerializeVec {
    type Ok = Document;
    type Error = DocumentError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DocumentError> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Document, DocumentError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for SerializeVec {
    type Ok = Document;
    type Error = DocumentError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DocumentError> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Document, DocumentError> {
        ser::SerializeSeq::end(self)
    }
}

struct SerializeTupleVariant {
    variant: String,
    items: Vec<Document>,
}

impl ser::SerializeTupleVariant for SerializeTupleVariant {
    type Ok = Document;
    type Error = DocumentError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DocumentError> {
        self.items.push(value.serialize(DocumentSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Document, DocumentError> {
        let mut map = IndexMap::new();
        map.insert(self.variant, Document::Array(self.items));
        Ok(Document::Object(map))
    }
}

/// Backs both `SerializeMap` (arbitrary key/value pairs) and
/// `SerializeStruct` (fixed, known field names) — a struct field is just a
/// map entry whose key is already a plain `&'static str`, so no key
/// conversion is needed on that path.
struct SerializeMapImpl {
    map: IndexMap<String, Document>,
    next_key: Option<String>,
}

impl ser::SerializeMap for SerializeMapImpl {
    type Ok = Document;
    type Error = DocumentError;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), DocumentError> {
        let key_doc = key.serialize(DocumentSerializer)?;
        self.next_key = Some(document_to_key(key_doc)?);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DocumentError> {
        let key = self
            .next_key
            .take()
            .expect("serialize_value called before serialize_key");
        self.map.insert(key, value.serialize(DocumentSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Document, DocumentError> {
        Ok(Document::Object(self.map))
    }
}

impl ser::SerializeStruct for SerializeMapImpl {
    type Ok = Document;
    type Error = DocumentError;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), DocumentError> {
        self.map
            .insert(key.to_string(), value.serialize(DocumentSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Document, DocumentError> {
        Ok(Document::Object(self.map))
    }
}

/// `Document::Object` keys are always `String` — a map key can be any
/// serializable type, so this is where an arbitrary key gets narrowed down
/// to something a `String` key can represent. String keys pass through
/// as-is; number/bool keys (a real, common case — `HashMap<i32, V>` etc.)
/// get their natural string form; anything else (a key that itself
/// serializes to an array or nested object) can't sensibly become an
/// object key at all.
fn document_to_key(doc: Document) -> Result<String, DocumentError> {
    match doc {
        Document::String(s) => Ok(s),
        Document::Int(i) => Ok(i.to_string()),
        Document::Float(f) => Ok(f.to_string()),
        Document::Bool(b) => Ok(b.to_string()),
        other => Err(DocumentError::custom(format!(
            "map keys must serialize to a string, int, float, or bool — got {other:?}"
        ))),
    }
}

struct SerializeStructVariant {
    variant: String,
    map: IndexMap<String, Document>,
}

impl ser::SerializeStructVariant for SerializeStructVariant {
    type Ok = Document;
    type Error = DocumentError;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), DocumentError> {
        self.map
            .insert(key.to_string(), value.serialize(DocumentSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Document, DocumentError> {
        let mut outer = IndexMap::new();
        outer.insert(self.variant, Document::Object(self.map));
        Ok(Document::Object(outer))
    }
}

// ---------- Deserialize: Document -> T ----------

/// A one-line, otherwise-unremarkable impl that `SeqDeserializer`/
/// `MapDeserializer` below need: they require their elements to be
/// "turnable into a deserializer," which for a type that's already a
/// `Deserializer` itself is just returning `self`. Serde doesn't provide
/// this generically for every `Deserializer` — every "value" type
/// (`serde_json::Value` included) writes this exact one-liner itself.
impl<'de> IntoDeserializer<'de, DocumentError> for Document {
    type Deserializer = Document;
    fn into_deserializer(self) -> Document {
        self
    }
}

impl<'de> Deserializer<'de> for Document {
    type Error = DocumentError;

    /// The one method that actually inspects `self`'s shape. Everything
    /// else below either forwards here (for types that don't need special
    /// handling — `Document` already knows what it *is*, so there's
    /// nothing format-specific like "peek the next byte" to do) or gets
    /// its own real implementation (`option`, `enum` — see below).
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DocumentError> {
        match self {
            Document::Null => visitor.visit_unit(),
            Document::Bool(v) => visitor.visit_bool(v),
            Document::Int(v) => visitor.visit_i64(v),
            Document::Float(v) => visitor.visit_f64(v),
            Document::String(v) => visitor.visit_string(v),
            Document::Binary(v) => visitor.visit_byte_buf(v),
            // Not a distinct wire type of its own for arbitrary T — a
            // stored _id round-trips through a typed field as its string
            // form (see SPEC.md for why this isn't given special
            // treatment here).
            Document::Id(id) => visitor.visit_string(id.to_string()),
            Document::Array(items) => visitor.visit_seq(SeqDeserializer::new(items.into_iter())),
            Document::Object(map) => visitor.visit_map(MapDeserializer::new(map.into_iter())),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DocumentError> {
        match self {
            Document::Null => visitor.visit_none(),
            other => visitor.visit_some(other),
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DocumentError> {
        let (variant, value) = match self {
            Document::String(variant) => (variant, None),
            Document::Object(map) => {
                let mut iter = map.into_iter();
                let (variant, value) = iter.next().ok_or_else(|| {
                    DocumentError::custom("expected a non-empty object for an enum variant")
                })?;
                if iter.next().is_some() {
                    return Err(DocumentError::custom(
                        "expected exactly one key for an enum variant, found more than one",
                    ));
                }
                (variant, Some(value))
            }
            other => {
                return Err(DocumentError::custom(format!(
                    "expected a string or single-entry object for an enum, got {other:?}"
                )));
            }
        };
        visitor.visit_enum(EnumDeserializer { variant, value })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

struct EnumDeserializer {
    variant: String,
    value: Option<Document>,
}

impl<'de> EnumAccess<'de> for EnumDeserializer {
    type Error = DocumentError;
    type Variant = VariantDeserializer;

    fn variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<(S::Value, VariantDeserializer), DocumentError> {
        let variant = seed.deserialize(self.variant.into_deserializer())?;
        Ok((variant, VariantDeserializer { value: self.value }))
    }
}

struct VariantDeserializer {
    value: Option<Document>,
}

impl<'de> VariantAccess<'de> for VariantDeserializer {
    type Error = DocumentError;

    fn unit_variant(self) -> Result<(), DocumentError> {
        match self.value {
            None => Ok(()),
            Some(_) => Err(DocumentError::custom(
                "expected a unit variant, found a variant with data",
            )),
        }
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<S::Value, DocumentError> {
        match self.value {
            Some(value) => seed.deserialize(value),
            None => Err(DocumentError::custom(
                "expected a newtype variant, found a unit variant",
            )),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, DocumentError> {
        match self.value {
            Some(Document::Array(items)) => {
                visitor.visit_seq(SeqDeserializer::new(items.into_iter()))
            }
            Some(other) => Err(DocumentError::custom(format!(
                "expected an array for a tuple variant, got {other:?}"
            ))),
            None => Err(DocumentError::custom(
                "expected a tuple variant, found a unit variant",
            )),
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DocumentError> {
        match self.value {
            Some(Document::Object(map)) => visitor.visit_map(MapDeserializer::new(map.into_iter())),
            Some(other) => Err(DocumentError::custom(format!(
                "expected an object for a struct variant, got {other:?}"
            ))),
            None => Err(DocumentError::custom(
                "expected a struct variant, found a unit variant",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocId;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    fn roundtrip<T>(value: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let doc = to_document(&value).unwrap();
        let back: T = from_document(doc).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn primitives_roundtrip() {
        roundtrip(true);
        roundtrip(42i64);
        roundtrip(3.5f64);
        roundtrip("hello".to_string());
        roundtrip(());
    }

    #[test]
    fn option_roundtrips_both_variants() {
        roundtrip(Some(42i64));
        roundtrip(None::<i64>);
    }

    #[test]
    fn vec_roundtrips() {
        roundtrip(vec![1i64, 2, 3]);
        roundtrip(Vec::<i64>::new());
    }

    #[test]
    fn map_roundtrips() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), 1i64);
        map.insert("b".to_string(), 2i64);
        roundtrip(map);
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Address {
        city: String,
        zip: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Person {
        name: String,
        age: i64,
        height: f64,
        active: bool,
        nickname: Option<String>,
        tags: Vec<String>,
        address: Address,
    }

    #[test]
    fn nested_struct_roundtrips() {
        roundtrip(Person {
            name: "Ada".to_string(),
            age: 30,
            height: 1.7,
            active: true,
            nickname: None,
            tags: vec!["admin".to_string(), "staff".to_string()],
            address: Address {
                city: "London".to_string(),
                zip: "SW1".to_string(),
            },
        });
    }

    #[test]
    fn struct_serializes_to_an_object_with_field_names() {
        let doc = to_document(&Address {
            city: "London".to_string(),
            zip: "SW1".to_string(),
        })
        .unwrap();

        let Document::Object(map) = doc else {
            panic!("expected an Object, got {doc:?}");
        };
        assert_eq!(map.get("city"), Some(&Document::String("London".into())));
        assert_eq!(map.get("zip"), Some(&Document::String("SW1".into())));
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    enum Shape {
        Point,
        Circle(f64),
        Rect { w: f64, h: f64 },
        Pair(f64, f64),
    }

    #[test]
    fn enum_unit_variant_roundtrips() {
        roundtrip(Shape::Point);
    }

    #[test]
    fn enum_newtype_variant_roundtrips() {
        roundtrip(Shape::Circle(2.0));
    }

    #[test]
    fn enum_struct_variant_roundtrips() {
        roundtrip(Shape::Rect { w: 3.0, h: 4.0 });
    }

    #[test]
    fn enum_tuple_variant_roundtrips() {
        roundtrip(Shape::Pair(1.0, 2.0));
    }

    #[test]
    fn unit_variant_serializes_to_a_bare_string() {
        assert_eq!(
            to_document(&Shape::Point).unwrap(),
            Document::String("Point".to_string())
        );
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Unit;

    #[test]
    fn unit_struct_roundtrips() {
        roundtrip(Unit);
    }

    #[test]
    fn u64_too_large_for_i64_is_an_error() {
        let err = to_document(&u64::MAX);
        assert!(err.is_err());
    }

    #[test]
    fn document_id_deserializes_as_its_string_form() {
        let id = DocId([7; 16]);
        let doc = Document::Id(id);
        let back: String = from_document(doc).unwrap();
        assert_eq!(back, id.to_string());
    }
}
