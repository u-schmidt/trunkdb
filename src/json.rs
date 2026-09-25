//! Tagged JSON: `Document` to `serde_json::Value` and back, without
//! losing anything (SPEC §30.1). Plain JSON can't tell an `Id` from a
//! string or hold `Binary`, `NaN` or infinity, so those become one-key
//! objects with a `$` tag, like MongoDB's Extended JSON:
//!
//! | `Document`             | JSON                                  |
//! |------------------------|---------------------------------------|
//! | `Id`                   | `{"$id": "<uuid>"}`                   |
//! | `Binary`               | `{"$binary": "<base64>"}`             |
//! | `Float` NaN / ±inf     | `{"$float": "NaN"}`, `"Infinity"`, `"-Infinity"` |
//! | `Object` that looks like a tag | `{"$object": {...}}`          |
//!
//! Everything else is plain JSON: `Int` is an integer, `Float` a number
//! with a fraction or exponent (`serde_json` writes `3.0`, not `3`), so
//! the two stay apart on the way back. Plain JSON is therefore valid
//! tagged JSON — a hand-written file needs no tags.
//!
//! Only a one-key object whose key is a tag name is read as a tag; any
//! other object, including one with an unknown `$` key such as MongoDB's
//! `$date`, is an ordinary object. An `Object` that is itself a one-key
//! object with a tag name is written inside `$object`, so it can't be
//! mistaken for a tag.

use crate::document::{DocId, Document};
use indexmap::IndexMap;
use serde_json::{Map, Number, Value};

const TAG_ID: &str = "$id";
const TAG_BINARY: &str = "$binary";
const TAG_FLOAT: &str = "$float";
const TAG_OBJECT: &str = "$object";
/// Only at the top of an export line: a document that isn't an `Object`
/// (see `document_line`).
const TAG_VALUE: &str = "$value";
const TAGS: [&str; 5] = [TAG_ID, TAG_BINARY, TAG_FLOAT, TAG_OBJECT, TAG_VALUE];

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct JsonError(String);

fn error(message: impl Into<String>) -> JsonError {
    JsonError(message.into())
}

/// A `Document` as tagged JSON.
pub fn to_json(doc: &Document) -> Value {
    match doc {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Int(n) => Value::Number((*n).into()),
        Document::Float(f) => match Number::from_f64(*f) {
            Some(n) => Value::Number(n),
            None => tagged(TAG_FLOAT, Value::String(non_finite_name(*f).into())),
        },
        Document::String(s) => Value::String(s.clone()),
        Document::Binary(bytes) => tagged(TAG_BINARY, Value::String(base64_encode(bytes))),
        Document::Id(id) => tagged(TAG_ID, Value::String(id.to_string())),
        Document::Array(items) => Value::Array(items.iter().map(to_json).collect()),
        Document::Object(map) => {
            let object = object_to_json(map);
            if looks_like_tag(&object) {
                tagged(TAG_OBJECT, Value::Object(object))
            } else {
                Value::Object(object)
            }
        }
    }
}

/// Tagged JSON back to a `Document`. Accepts any JSON, except integers
/// outside `i64` (a `Document::Int` can't hold them) and malformed tags.
pub fn from_json(value: Value) -> Result<Document, JsonError> {
    Ok(match value {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(b),
        Value::Number(n) => number(&n)?,
        Value::String(s) => Document::String(s),
        Value::Array(items) => {
            Document::Array(items.into_iter().map(from_json).collect::<Result<_, _>>()?)
        }
        Value::Object(object) if looks_like_tag(&object) => {
            let (tag, inner) = object.into_iter().next().expect("one key");
            from_tag(tag, inner)?
        }
        Value::Object(object) => Document::Object(object_from_json(object)?),
    })
}

/// One document of an export: an `Object` as itself (it already holds
/// its `_id`, SPEC §18). Any other document has no field for its id, so
/// it's wrapped: `{"_id": {"$id": ...}, "$value": <the document>}`. An
/// `Object` whose only field besides `_id` has a tag name is written
/// whole inside `$object`, with its `_id` repeated outside, so it can't
/// be read as that wrapper.
pub(crate) fn document_line(id: DocId, doc: &Document) -> Value {
    let id_json = to_json(&Document::Id(id));
    match doc {
        Document::Object(map) => {
            let object = object_to_json(map);
            let mut others = object.keys().filter(|k| *k != "_id");
            let tag_like = matches!(
                (others.next(), others.next()),
                (Some(key), None) if TAGS.contains(&key.as_str())
            );
            if tag_like {
                let mut line = Map::new();
                line.insert("_id".into(), id_json);
                line.insert(TAG_OBJECT.into(), Value::Object(object));
                Value::Object(line)
            } else {
                Value::Object(object)
            }
        }
        other => {
            let mut line = Map::new();
            line.insert("_id".into(), id_json);
            line.insert(TAG_VALUE.into(), to_json(other));
            Value::Object(line)
        }
    }
}

/// Reverses `document_line`: the document, and its id if the line has
/// an `_id` (a hand-written line may leave it out to get a new one).
pub(crate) fn parse_document_line(line: Value) -> Result<(Option<DocId>, Document), JsonError> {
    let Value::Object(mut object) = line else {
        return Err(error("a document line must be a JSON object"));
    };
    let id = match object.get("_id") {
        None => None,
        Some(value) => match from_json(value.clone())? {
            Document::Id(id) => Some(id),
            _ => {
                return Err(error(
                    r#"`_id` must be {"$id": "<uuid>"}; leave it out to get a new id"#,
                ));
            }
        },
    };
    let mut others = object.keys().filter(|k| *k != "_id");
    let wrapper = match (others.next(), others.next()) {
        (Some(key), None) if key == TAG_VALUE || key == TAG_OBJECT => Some(key.clone()),
        _ => None,
    };
    let doc = match wrapper.as_deref() {
        Some(TAG_VALUE) => from_json(object.shift_remove(TAG_VALUE).unwrap())?,
        // The whole object, `_id` included, in its original field order.
        Some(_) => from_tag(TAG_OBJECT.into(), object.shift_remove(TAG_OBJECT).unwrap())?,
        None => Document::Object(object_from_json(object)?),
    };
    Ok((id, doc))
}

fn tagged(tag: &str, value: Value) -> Value {
    let mut object = Map::new();
    object.insert(tag.into(), value);
    Value::Object(object)
}

/// Whether `key` is one of the tags an export writes (SPEC §30.1).
pub(crate) fn is_tag(key: &str) -> bool {
    TAGS.contains(&key)
}

fn looks_like_tag(object: &Map<String, Value>) -> bool {
    object.len() == 1 && object.keys().all(|k| TAGS.contains(&k.as_str()))
}

fn from_tag(tag: String, value: Value) -> Result<Document, JsonError> {
    let text = |value: Value| match value {
        Value::String(s) => Ok(s),
        other => Err(error(format!("`{tag}` must hold a string, got {other}"))),
    };
    Ok(match tag.as_str() {
        TAG_ID => {
            let s = text(value)?;
            let uuid = uuid::Uuid::parse_str(&s)
                .map_err(|e| error(format!("`$id` {s:?} is not a UUID: {e}")))?;
            Document::Id(DocId(uuid.into_bytes()))
        }
        TAG_BINARY => {
            let s = text(value)?;
            Document::Binary(
                base64_decode(&s).ok_or_else(|| error(format!("`$binary` {s:?} is not base64")))?,
            )
        }
        TAG_FLOAT => match text(value)?.as_str() {
            "NaN" => Document::Float(f64::NAN),
            "Infinity" => Document::Float(f64::INFINITY),
            "-Infinity" => Document::Float(f64::NEG_INFINITY),
            other => {
                return Err(error(format!(
                    "`$float` must be \"NaN\", \"Infinity\" or \"-Infinity\", got {other:?}"
                )));
            }
        },
        TAG_OBJECT => match value {
            Value::Object(inner) => Document::Object(object_from_json(inner)?),
            other => return Err(error(format!("`$object` must hold an object, got {other}"))),
        },
        _ => {
            return Err(error(
                "`$value` is only allowed at the top of a document line",
            ));
        }
    })
}

fn number(n: &Number) -> Result<Document, JsonError> {
    if let Some(i) = n.as_i64() {
        Ok(Document::Int(i))
    } else if n.is_u64() {
        Err(error(format!("{n} doesn't fit in an i64")))
    } else {
        Ok(Document::Float(
            n.as_f64().expect("a JSON number is i64, u64 or f64"),
        ))
    }
}

fn non_finite_name(f: f64) -> &'static str {
    if f.is_nan() {
        "NaN"
    } else if f > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    }
}

fn object_to_json(map: &IndexMap<String, Document>) -> Map<String, Value> {
    map.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()
}

fn object_from_json(object: Map<String, Value>) -> Result<IndexMap<String, Document>, JsonError> {
    object
        .into_iter()
        .map(|(k, v)| Ok((k, from_json(v)?)))
        .collect()
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding (RFC 4648) — a dozen lines, so no
/// dependency for it.
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i == bytes.len() / 4 - 1;
        let padding = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - padding] {
            let value = BASE64.iter().position(|&a| a == c)? as u32;
            n = (n << 6) | value;
        }
        n <<= 6 * padding as u32;
        let decoded = n.to_be_bytes();
        out.extend_from_slice(&decoded[1..4 - padding]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(fields: &[(&str, Document)]) -> Document {
        Document::Object(
            fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// `PartialEq` on `Document` says `NaN != NaN`; compare encodings.
    fn same(a: &Document, b: &Document) -> bool {
        crate::document::encode_document(a) == crate::document::encode_document(b)
    }

    fn roundtrip(doc: &Document) -> Document {
        let text = serde_json::to_string(&to_json(doc)).unwrap();
        from_json(serde_json::from_str(&text).unwrap()).unwrap()
    }

    #[test]
    fn every_kind_of_value_survives_text() {
        let id = DocId([7; 16]);
        let doc = object(&[
            ("null", Document::Null),
            ("bool", Document::Bool(true)),
            ("int", Document::Int(i64::MIN)),
            ("big", Document::Int((1 << 53) + 1)),
            ("float", Document::Float(3.0)),
            ("tiny", Document::Float(5e-324)),
            ("negzero", Document::Float(-0.0)),
            ("nan", Document::Float(f64::NAN)),
            ("inf", Document::Float(f64::INFINITY)),
            ("neginf", Document::Float(f64::NEG_INFINITY)),
            ("string", Document::String("ß \"q\" \0 \u{1F600}".into())),
            ("binary", Document::Binary((0..=255).collect())),
            ("empty", Document::Binary(vec![])),
            ("id", Document::Id(id)),
            (
                "array",
                Document::Array(vec![Document::Int(1), Document::Float(1.0)]),
            ),
            // Objects that look like tags, at any depth.
            ("fake_id", object(&[("$id", Document::String("x".into()))])),
            ("fake_object", object(&[("$object", Document::Int(1))])),
            ("mongo_date", object(&[("$date", Document::Int(0))])),
            ("zeta", Document::Null),
            ("alpha", Document::Null), // field order is kept, not sorted
        ]);
        let back = roundtrip(&doc);
        assert!(same(&back, &doc), "{back:?}");
    }

    #[test]
    fn int_and_float_stay_apart() {
        assert_eq!(roundtrip(&Document::Int(3)), Document::Int(3));
        assert_eq!(roundtrip(&Document::Float(3.0)), Document::Float(3.0));
        assert_eq!(roundtrip(&Document::Float(1e300)), Document::Float(1e300));
        assert_eq!(
            serde_json::to_string(&to_json(&Document::Float(3.0))).unwrap(),
            "3.0"
        );
    }

    /// Needs `serde_json`'s `float_roundtrip` feature: its default
    /// parser is faster but can be off by one bit (it read
    /// `4.1946076254797075e17` back as `4.194607625479707e17`).
    #[test]
    fn floats_come_back_bit_for_bit() {
        let mut rng = crate::testing::XorShift(0xF10A7);
        for _ in 0..100_000 {
            let f = f64::from_bits(rng.next());
            if f.is_finite() {
                let back = roundtrip(&Document::Float(f));
                assert_eq!(back, Document::Float(f), "{f:e}");
            }
            let g = rng.next() as f64 / (rng.below(1000) + 1) as f64;
            assert_eq!(roundtrip(&Document::Float(g)), Document::Float(g), "{g:e}");
        }
    }

    #[test]
    fn plain_json_is_tagged_json() {
        let value: Value =
            serde_json::from_str(r#"{"name":"Ada","age":36,"tags":["a"],"x":{"$date":1}}"#)
                .unwrap();
        let doc = from_json(value).unwrap();
        let Document::Object(map) = &doc else {
            panic!()
        };
        assert_eq!(map["age"], Document::Int(36));
        assert_eq!(map["x"], object(&[("$date", Document::Int(1))]));
    }

    #[test]
    fn bad_input_is_an_error_not_a_guess() {
        for text in [
            r#"18446744073709551615"#,
            r#"{"$id": "not-a-uuid"}"#,
            r#"{"$id": 5}"#,
            r#"{"$binary": "abc"}"#,
            r#"{"$binary": "a=bc"}"#,
            r#"{"$float": "nan"}"#,
            r#"{"$object": 1}"#,
            r#"{"$value": 1}"#,
        ] {
            let value: Value = serde_json::from_str(text).unwrap();
            assert!(from_json(value).is_err(), "{text}");
        }
    }

    #[test]
    fn document_lines_carry_the_id() {
        let id = DocId([9; 16]);
        let cases = [
            object(&[("a", Document::Int(1)), ("_id", Document::Id(id))]),
            object(&[("_id", Document::Id(id)), ("$value", Document::Int(1))]),
            object(&[("_id", Document::Id(id)), ("$object", Document::Int(1))]),
            object(&[("_id", Document::Id(id)), ("$id", Document::Int(1))]),
            object(&[("$float", Document::Int(1)), ("_id", Document::Id(id))]),
            object(&[("_id", Document::Id(id))]),
            Document::Int(5),
            Document::Array(vec![Document::Null]),
            Document::Id(DocId([1; 16])),
        ];
        for doc in cases {
            let text = serde_json::to_string(&document_line(id, &doc)).unwrap();
            let (back_id, back) =
                parse_document_line(serde_json::from_str(&text).unwrap()).unwrap();
            assert_eq!(back_id, Some(id), "{text}");
            assert!(same(&back, &doc), "{text} -> {back:?}");
        }
    }

    #[test]
    fn a_line_without_id_gets_none_and_a_bad_id_is_rejected() {
        let (id, doc) = parse_document_line(serde_json::from_str(r#"{"a":1}"#).unwrap()).unwrap();
        assert_eq!(id, None);
        assert_eq!(doc, object(&[("a", Document::Int(1))]));
        for text in [r#"{"_id":"abc","a":1}"#, r#"[1]"#] {
            assert!(parse_document_line(serde_json::from_str(text).unwrap()).is_err());
        }
    }

    #[test]
    fn base64_matches_rfc_4648() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
            assert_eq!(base64_decode(encoded).unwrap(), plain.as_bytes());
        }
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Z!==", "Zm9v\n"] {
            assert_eq!(base64_decode(bad), None, "{bad}");
        }
    }
}
