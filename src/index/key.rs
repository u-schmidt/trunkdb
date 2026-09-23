//! Index keys are byte strings, compared byte by byte (`[u8]`'s `Ord`) —
//! so the B-tree needs to know nothing about documents (SPEC §28.1).
//!
//! - The primary index's key is the 16-byte `DocId`, as it always was.
//! - A secondary index's key is the indexed field's value, encoded so
//!   that byte order matches `Filter`'s order, followed by the `DocId`:
//!   every key is unique even when many documents share a value, and
//!   removing one document's entry names exactly one key.

use crate::document::{DocId, Document};

/// Longest key the B-tree accepts. Small enough that any page holds at
/// least four entries, which is what keeps splits safe with keys of
/// different lengths (SPEC §28.2).
pub const MAX_KEY_LEN: usize = 1024;

// Type tags, the first byte of an encoded value. Values of different
// types never compare equal or ordered in `Filter` (`query::compare`), so
// their relative order here is arbitrary; grouping them by tag is what
// matters, so a range stays within one type.
const TAG_BOOL: u8 = 1;
const TAG_NUMBER: u8 = 2;
const TAG_STRING: u8 = 3;

/// How many bytes an escaped string may take, so that tag + string +
/// terminator + `DocId` stays within `MAX_KEY_LEN`.
const STRING_BUDGET: usize = MAX_KEY_LEN - 1 - 2 - 16;

pub fn primary(id: DocId) -> Vec<u8> {
    id.0.to_vec()
}

/// The document id a key ends with — the whole key for the primary
/// index, the last 16 bytes for a secondary one.
pub fn doc_id(key: &[u8]) -> DocId {
    DocId(key[key.len() - 16..].try_into().unwrap())
}

/// The secondary-index key for a document whose field holds `value`, or
/// `None` if that value isn't indexed (see `encode_value`).
pub fn secondary(value: &Document, id: DocId) -> Option<Vec<u8>> {
    let mut key = encode_value(value)?;
    key.extend_from_slice(&id.0);
    Some(key)
}

/// Encodes a field value so that byte order agrees with `Filter`'s
/// comparisons: `a < b` in `Filter` implies `encode(a) <= encode(b)`.
/// Not strictly: some different values encode the same (large `Int`s
/// that round to one `f64`, strings that share their first ~1000 bytes).
/// That's fine, since every document an index returns is checked against
/// the full filter again (SPEC §28.3). Only the types a comparison can
/// match are indexed — `Bool`, `Int`/`Float`, `String`. Anything else
/// (`Null`, arrays, objects, binary, ids, `NaN`) is `None`: no `Eq`/`Lt`/
/// `Lte`/`Gt`/`Gte` condition can match it, so leaving it out of the
/// index loses nothing.
///
/// The encoding is prefix-free — no encoded value is a proper prefix of
/// another — so the `DocId` appended after it can't change the order
/// between two different values.
pub fn encode_value(value: &Document) -> Option<Vec<u8>> {
    match value {
        Document::Bool(b) => Some(vec![TAG_BOOL, *b as u8]),
        // `as f64`, like `query::compare` does for `Int` vs. `Float`.
        Document::Int(n) => encode_number(*n as f64),
        Document::Float(f) => encode_number(*f),
        Document::String(s) => Some(encode_string(s.as_bytes())),
        _ => None,
    }
}

/// Big-endian IEEE 754 bits, with the sign bit flipped for positives and
/// all bits flipped for negatives — the standard trick that makes byte
/// order equal numeric order.
fn encode_number(f: f64) -> Option<Vec<u8>> {
    if f.is_nan() {
        return None;
    }
    let f = if f == 0.0 { 0.0 } else { f }; // -0.0 == 0.0 in `Filter`
    let bits = f.to_bits();
    let sortable = if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    };
    let mut out = vec![TAG_NUMBER];
    out.extend_from_slice(&sortable.to_be_bytes());
    Some(out)
}

/// The string's bytes with `0x00` escaped as `0x00 0xFF`, then a
/// `0x00 0x00` terminator: sorts like the plain bytes, and the
/// terminator makes it prefix-free ("ab" < "abc" still holds with a
/// `DocId` after each). Cut off after `STRING_BUDGET` escaped bytes —
/// always at the same point for two strings that agree up to it, so the
/// cut keeps the order (non-strictly).
fn encode_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![TAG_STRING];
    let mut used = 0;
    for &b in bytes {
        let width = if b == 0 { 2 } else { 1 };
        if used + width > STRING_BUDGET {
            break;
        }
        used += width;
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// A half-open range of keys, `start <= key < end` (`end: None` =
/// unbounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    pub start: Vec<u8>,
    pub end: Option<Vec<u8>>,
}

impl KeyRange {
    /// Every key that begins with `prefix`.
    pub fn prefixed(prefix: &[u8]) -> Self {
        KeyRange {
            start: prefix.to_vec(),
            end: prefix_end(prefix),
        }
    }

    /// Keys in both ranges.
    pub fn intersect(self, other: KeyRange) -> KeyRange {
        let start = self.start.max(other.start);
        let end = match (self.end, other.end) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        KeyRange { start, end }
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && self.end.as_deref().is_none_or(|end| key < end)
    }
}

/// The first key after every key that begins with `prefix`: the prefix
/// with its last non-`0xFF` byte incremented and anything after dropped.
/// `None` if there's no such key (the prefix is all `0xFF`).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// Every key a document can have if its field `op`-compares true against
/// `value`, or a superset of them — `None` if no range describes it
/// (`Ne`, `Contains`) or `value` isn't an indexed type. Bounds are
/// inclusive of `value`'s own encoding even for `Lt`/`Gt`: different
/// values can share an encoding, and the recheck sorts them out.
pub fn range_for(op: &crate::query::Op, value: &Document) -> Option<KeyRange> {
    use crate::query::Op;
    let encoded = encode_value(value)?;
    let same_type = KeyRange::prefixed(&encoded[..1]);
    let this_value = KeyRange::prefixed(&encoded);
    match op {
        Op::Eq => Some(this_value),
        Op::Gt | Op::Gte => Some(KeyRange {
            start: encoded,
            end: same_type.end,
        }),
        Op::Lt | Op::Lte => Some(KeyRange {
            start: same_type.start,
            end: this_value.end,
        }),
        Op::Ne | Op::Contains => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Op;

    fn key(value: Document) -> Vec<u8> {
        encode_value(&value).unwrap()
    }

    #[test]
    fn numbers_sort_numerically_across_int_and_float() {
        let values = [
            Document::Float(f64::NEG_INFINITY),
            Document::Int(-1_000_000),
            Document::Float(-2.5),
            Document::Int(-1),
            Document::Float(-0.0),
            Document::Int(0),
            Document::Float(0.5),
            Document::Int(1),
            Document::Float(1e300),
            Document::Float(f64::INFINITY),
        ];
        for pair in values.windows(2) {
            assert!(
                key(pair[0].clone()) <= key(pair[1].clone()),
                "{:?} vs {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(key(Document::Float(-0.0)), key(Document::Int(0)));
        assert_eq!(key(Document::Float(3.0)), key(Document::Int(3)));
        assert_eq!(encode_value(&Document::Float(f64::NAN)), None);
    }

    #[test]
    fn strings_sort_bytewise_and_are_prefix_free() {
        let values = ["", "\0", "\0\0", "a", "a\0", "a\0b", "ab", "abc", "b", "ß"];
        for pair in values.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(a < b);
            // With ids that would reverse the order if the encoding
            // weren't prefix-free.
            let ka = secondary(&Document::String(a.into()), DocId([0xFF; 16])).unwrap();
            let kb = secondary(&Document::String(b.into()), DocId([0; 16])).unwrap();
            assert!(ka < kb, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn long_strings_are_cut_but_keep_their_order() {
        let long = |tail: &str| Document::String("x".repeat(5000) + tail);
        let (a, b) = (key(long("a")), key(long("b")));
        assert_eq!(a, b, "they differ only past the cut");
        assert!(a.len() + 16 <= MAX_KEY_LEN);
        assert!(key(long("")) <= key(Document::String("y".into())));
        // Zero bytes take two bytes each; the budget still holds.
        let zeros = key(Document::String("\0".repeat(5000)));
        assert!(zeros.len() + 16 <= MAX_KEY_LEN);
    }

    #[test]
    fn only_comparable_types_are_indexed() {
        assert!(encode_value(&Document::Bool(true)).is_some());
        for value in [
            Document::Null,
            Document::Binary(vec![1]),
            Document::Array(vec![]),
            Document::Id(DocId([1; 16])),
        ] {
            assert_eq!(encode_value(&value), None, "{value:?}");
        }
    }

    #[test]
    fn ranges_cover_what_the_filter_would_match() {
        let id = DocId([7; 16]);
        let k = |n: i64| secondary(&Document::Int(n), id).unwrap();
        let five = Document::Int(5);

        let eq = range_for(&Op::Eq, &five).unwrap();
        assert!(eq.contains(&k(5)) && !eq.contains(&k(4)) && !eq.contains(&k(6)));
        let gt = range_for(&Op::Gt, &five).unwrap();
        assert!(gt.contains(&k(6)) && gt.contains(&k(5)) && !gt.contains(&k(4)));
        let lt = range_for(&Op::Lt, &five).unwrap();
        assert!(lt.contains(&k(4)) && lt.contains(&k(5)) && !lt.contains(&k(6)));

        // A range stays within its type.
        let string = secondary(&Document::String("a".into()), id).unwrap();
        let boolean = secondary(&Document::Bool(true), id).unwrap();
        assert!(!gt.contains(&string) && !lt.contains(&boolean));

        assert_eq!(range_for(&Op::Ne, &five), None);
        assert_eq!(
            range_for(&Op::Contains, &Document::String("a".into())),
            None
        );
        assert_eq!(range_for(&Op::Eq, &Document::Null), None);
    }

    #[test]
    fn intersect_narrows_to_both() {
        let r = range_for(&Op::Gte, &Document::Int(10))
            .unwrap()
            .intersect(range_for(&Op::Lte, &Document::Int(20)).unwrap());
        let k = |n: i64| secondary(&Document::Int(n), DocId([0; 16])).unwrap();
        assert!(r.contains(&k(10)) && r.contains(&k(15)) && r.contains(&k(20)));
        assert!(!r.contains(&k(9)) && !r.contains(&k(21)));
    }

    #[test]
    fn prefix_end_skips_trailing_ff() {
        assert_eq!(prefix_end(&[1, 2]), Some(vec![1, 3]));
        assert_eq!(prefix_end(&[1, 0xFF]), Some(vec![2]));
        assert_eq!(prefix_end(&[0xFF, 0xFF]), None);
    }
}
