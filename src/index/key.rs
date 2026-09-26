//! Index keys are byte strings, compared byte by byte (`[u8]`'s `Ord`) —
//! so the B-tree needs to know nothing about documents (SPEC §28.1).
//!
//! - The primary index's key is the 16-byte `DocId`, as it always was.
//! - A secondary index's key is the indexed field's value, encoded so
//!   that byte order matches `Filter`'s order, followed by the `DocId`:
//!   every key is unique even when many documents share a value, and
//!   removing one document's entry names exactly one key.
//! - A compound index's key is several values encoded one after another,
//!   then the `DocId` (SPEC §43). The encoding is prefix-free, so the
//!   keys sort by the first value, then the second, and so on.

use crate::document::{DocId, Document};

/// Longest key the B-tree accepts. Small enough that any page holds at
/// least four entries, which is what keeps splits safe with keys of
/// different lengths (SPEC §28.2).
pub const MAX_KEY_LEN: usize = 1024;

// Type tags, the first byte of an encoded value. Values of different
// types never compare equal or ordered in `Filter` (`query::compare`), so
// their relative order here is arbitrary; grouping them by tag is what
// matters, so a range stays within one type.
const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_NUMBER: u8 = 2;
const TAG_STRING: u8 = 3;
/// A value only compound keys hold (SPEC §43.2): arrays, objects,
/// binary, ids, NaN — everything `encode_value` leaves out. Last, as
/// such values sort after everything (§34.1). No bytes after the tag:
/// they're all one value to the index.
const TAG_OTHER: u8 = 0xFF;

/// Most fields a compound index may have: each gets an even share of
/// the key, so strings in an 8-field key keep 123 bytes.
pub const MAX_COMPOUND_FIELDS: usize = 8;

/// How many bytes an escaped string may take in a key of `fields`
/// values, so that each value's tag + string + terminator, and the
/// `DocId`, stay within `MAX_KEY_LEN`.
const fn string_budget(fields: usize) -> usize {
    (MAX_KEY_LEN - 16) / fields - 3
}

pub fn primary(id: DocId) -> Vec<u8> {
    id.0.to_vec()
}

// The functions below take keys apart that were read from the file. A
// damaged one gets a cautious answer from them, never a panic (SPEC §55):
// a key too short for its id, a number cut short or a string without its
// terminator. Finding the damage is `Database::check`'s job; every document
// is checked against the filter anyway, so a wrong answer here changes
// how much is read, not what matches.

/// The document id a key ends with — the whole key for the primary
/// index, the last 16 bytes for a secondary one. A damaged key shorter
/// than an id is zero-padded in front.
pub fn doc_id(key: &[u8]) -> DocId {
    let tail = &key[key.len().saturating_sub(16)..];
    let mut id = [0u8; 16];
    id[16 - tail.len()..].copy_from_slice(tail);
    DocId(id)
}

/// A secondary key without its trailing `DocId`: the encoded value.
pub fn value_part(key: &[u8]) -> &[u8] {
    &key[..key.len().saturating_sub(16)]
}

/// Whether every value encoded as `value_part` is equal to every other
/// (as `Filter` compares). Almost always: only numbers beyond 2^53 (where
/// different `Int`s round to one `f64`) and strings cut to the key budget
/// can share an encoding without being equal (SPEC §28.1). Errs towards
/// `false` — a string exactly as long as the budget counts as cut.
#[cfg(test)]
pub fn is_exact(value_part: &[u8]) -> bool {
    part_is_exact(value_part, 1)
}

/// `is_exact` for one value of a key of `fields` values (`parts`).
pub fn part_is_exact(part: &[u8], fields: usize) -> bool {
    let value_part = part;
    let Some(&tag) = value_part.first() else {
        return false;
    };
    match tag {
        TAG_NUMBER => {
            let Some(Ok(bytes)) = value_part.get(1..9).map(<[u8; 8]>::try_from) else {
                return false;
            };
            let sortable = u64::from_be_bytes(bytes);
            // `encode_number` backwards.
            let bits = if sortable >> 63 == 1 {
                sortable & !(1 << 63)
            } else {
                !sortable
            };
            f64::from_bits(bits).abs() < (1u64 << 53) as f64
        }
        // Tag, escaped bytes, two-byte terminator: a cut string used at
        // least `string_budget - 1` bytes, since the next byte (up to two
        // escaped) didn't fit.
        TAG_STRING => value_part
            .len()
            .checked_sub(3)
            .is_some_and(|len| len < string_budget(fields) - 1),
        TAG_OTHER => false,
        _ => true,
    }
}

/// Whether one value of a compound key is `TAG_OTHER`: a value that
/// sorts after everything, whichever the direction (§34.1).
pub fn is_other(part: &[u8]) -> bool {
    part.first() == Some(&TAG_OTHER)
}

/// A compound key's value part, split into its values' encodings. In a
/// damaged key, whatever is left when a value runs past the end is one
/// last part.
pub fn parts(value_part: &[u8]) -> Vec<&[u8]> {
    let mut parts = Vec::new();
    let mut rest = value_part;
    while let Some(&tag) = rest.first() {
        let len = match tag {
            TAG_NULL | TAG_OTHER => 1,
            TAG_BOOL => 2,
            TAG_NUMBER => 9,
            _ => string_len(rest).unwrap_or(rest.len()),
        };
        let (part, after) = rest.split_at(len.min(rest.len()));
        parts.push(part);
        rest = after;
    }
    parts
}

/// An encoded string's length, tag through the `0x00 0x00` terminator (an
/// escaped zero is `0x00 0xFF`); `None` if the terminator is missing.
fn string_len(encoded: &[u8]) -> Option<usize> {
    let mut at = 1;
    loop {
        match (*encoded.get(at)?, *encoded.get(at + 1)?) {
            (0, 0) => return Some(at + 2),
            (0, _) => at += 2,
            _ => at += 1,
        }
    }
}

/// A compound index's key for a document with these values in its
/// fields, in order (SPEC §43.2).
pub fn compound(values: &[&Document], id: DocId) -> Vec<u8> {
    let mut key = encode_values(values);
    key.extend_from_slice(&id.0);
    key
}

/// A compound key's value part: each value as `encode_value` encodes it,
/// strings cut to an even share of the key (`string_budget`), and a value
/// it leaves out as `TAG_OTHER`. A compound index holds every document:
/// an `Eq` on its first field must find them all, whatever the others
/// hold.
pub fn encode_values(values: &[&Document]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        out.extend(encode_value_in(value, values.len()).unwrap_or_else(|| vec![TAG_OTHER]));
    }
    out
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
/// match are indexed — `Null` (which also stands for a missing field,
/// SPEC §32), `Bool`, `Int`/`Float`, `String`. Anything else (arrays,
/// objects, binary, ids, `NaN`) is `None`: no `Eq`/`Lt`/`Lte`/`Gt`/`Gte`
/// condition can match it, so leaving it out of the index loses nothing.
///
/// The encoding is prefix-free — no encoded value is a proper prefix of
/// another — so the `DocId` appended after it can't change the order
/// between two different values.
pub fn encode_value(value: &Document) -> Option<Vec<u8>> {
    encode_value_in(value, 1)
}

/// `encode_value` for one of `fields` values in a key.
fn encode_value_in(value: &Document, fields: usize) -> Option<Vec<u8>> {
    match value {
        Document::Null => Some(vec![TAG_NULL]),
        Document::Bool(b) => Some(vec![TAG_BOOL, *b as u8]),
        // `as f64`, like `query::compare` does for `Int` vs. `Float`.
        Document::Int(n) => encode_number(*n as f64),
        Document::Float(f) => encode_number(*f),
        Document::String(s) => Some(encode_string(s.as_bytes(), string_budget(fields))),
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
/// `DocId` after each). Cut off after `budget` escaped bytes —
/// always at the same point for two strings that agree up to it, so the
/// cut keeps the order (non-strictly).
fn encode_string(bytes: &[u8], budget: usize) -> Vec<u8> {
    let mut out = vec![TAG_STRING];
    let mut used = 0;
    for &b in bytes {
        let width = if b == 0 { 2 } else { 1 };
        if used + width > budget {
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
    /// Every key.
    pub fn everything() -> Self {
        KeyRange {
            start: Vec::new(),
            end: None,
        }
    }

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

    #[cfg(test)]
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && self.end.as_deref().is_none_or(|end| key < end)
    }

    /// This range among the keys that begin with `prefix`: in a compound
    /// index, a range on one field after equal values in the fields
    /// before it (SPEC §43.3).
    pub fn under(self, prefix: &[u8]) -> KeyRange {
        KeyRange {
            start: [prefix, &self.start].concat(),
            end: match self.end {
                Some(end) => Some([prefix, &end].concat()),
                None => prefix_end(prefix),
            },
        }
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
    range_for_in(op, value, 1)
}

/// `range_for` on one of a compound key's `fields` values, relative to
/// where that value starts: strings are cut as the keys cut them.
pub fn range_for_in(op: &crate::query::Op, value: &Document, fields: usize) -> Option<KeyRange> {
    use crate::query::Op;
    let encoded = encode_value_in(value, fields)?;
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

    /// Compound keys sort by the first value, then the second — whatever
    /// the ids after them — and split back into their values.
    #[test]
    fn compound_keys_sort_field_by_field_and_split_back() {
        let s = |v: &str| Document::String(v.into());
        let tuples = [
            (Document::Null, s("z")),
            (Document::Int(1), Document::Null),
            (Document::Int(1), Document::Int(-3)),
            (Document::Int(1), s("")),
            (Document::Int(1), s("a")),
            (Document::Int(1), Document::Array(vec![])),
            (Document::Int(2), Document::Bool(false)),
            (s("a"), Document::Int(0)),
            (s("a\0"), Document::Int(0)),
            (Document::Float(f64::NAN), Document::Int(0)),
        ];
        let key = |(a, b): &(Document, Document), id| compound(&[a, b], DocId([id; 16]));
        for pair in tuples.windows(2) {
            // With ids that would reverse the order if they mattered.
            assert!(key(&pair[0], 0xFF) < key(&pair[1], 0), "{pair:?}");
        }
        for tuple in &tuples {
            let k = key(tuple, 7);
            let parts = parts(value_part(&k));
            assert_eq!(parts.len(), 2, "{tuple:?}");
            assert_eq!(parts.concat(), value_part(&k));
        }
        // Values no single-field index holds sort last, and are one value.
        let array = key(&(Document::Array(vec![1.into()]), Document::Null), 0);
        let object = key(&(Document::Object(Default::default()), Document::Null), 0);
        assert_eq!(value_part(&array), value_part(&object));
        assert!(is_other(parts(value_part(&array))[0]));
        assert!(!part_is_exact(parts(value_part(&array))[0], 2));
    }

    /// Each value of an n-field key gets an even share of it: the longest
    /// key still fits, and a string cut to its share is not exact.
    #[test]
    fn compound_keys_share_the_budget() {
        for fields in 1..=MAX_COMPOUND_FIELDS {
            let long = Document::String("\0".repeat(3000));
            let values = vec![&long; fields];
            let k = compound(&values, DocId([1; 16]));
            assert!(k.len() <= MAX_KEY_LEN, "{fields} fields: {}", k.len());
            let parts = parts(value_part(&k));
            assert_eq!(parts.len(), fields);
            assert!(parts.iter().all(|p| !part_is_exact(p, fields)));
        }
        let short = Document::String("x".repeat(100));
        let k = compound(&[&short, &short], DocId([1; 16]));
        assert!(parts(value_part(&k)).iter().all(|p| part_is_exact(p, 2)));
    }

    #[test]
    fn a_range_under_a_prefix_stays_within_it() {
        let prefix = encode_values(&[&Document::Int(1)]);
        let five = range_for(&Op::Gt, &Document::Int(5))
            .unwrap()
            .under(&prefix);
        let k = |a: i64, b: i64| compound(&[&a.into(), &b.into()], DocId([0; 16]));
        assert!(five.contains(&k(1, 6)) && five.contains(&k(1, 5)));
        assert!(!five.contains(&k(1, 4)) && !five.contains(&k(2, 6)) && !five.contains(&k(0, 9)));
        let everything = KeyRange::prefixed(&prefix);
        assert!(everything.contains(&k(1, i64::MIN)) && !everything.contains(&k(2, 0)));
    }

    #[test]
    fn only_comparable_types_are_indexed() {
        assert!(encode_value(&Document::Bool(true)).is_some());
        assert_eq!(key(Document::Null), [TAG_NULL]);
        for value in [
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

        // Null is a type of its own, with one value.
        let null = secondary(&Document::Null, id).unwrap();
        let eq_null = range_for(&Op::Eq, &Document::Null).unwrap();
        assert!(eq_null.contains(&null) && !eq_null.contains(&boolean));
        assert!(!lt.contains(&null) && !gt.contains(&null));
    }

    #[test]
    fn only_big_numbers_and_cut_strings_are_inexact() {
        let exact = |value: Document| is_exact(&key(value));
        let big = 1i64 << 53;
        for value in [
            Document::Null,
            Document::Bool(true),
            Document::Int(0),
            Document::Int(big - 1),
            Document::Int(-(big - 1)),
            Document::Float(-0.5),
            Document::Float(1e15),
            Document::String("x".repeat(string_budget(1) - 2)),
            Document::String("\0".repeat(string_budget(1) / 2 - 1)),
        ] {
            assert!(exact(value.clone()), "{value:?}");
        }
        for value in [
            Document::Int(big),
            Document::Int(-big),
            Document::Int(i64::MAX),
            Document::Float(1e300),
            Document::Float(f64::NEG_INFINITY),
            Document::String("x".repeat(string_budget(1) - 1)),
            Document::String("x".repeat(5000)),
            Document::String("\0".repeat(5000)),
        ] {
            assert!(!exact(value.clone()), "{value:?}");
        }
        // And a cut string does share its key with a different one.
        let cut = |tail: &str| key(Document::String("x".repeat(string_budget(1)) + tail));
        assert_eq!(cut("a"), cut("b"));
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

    /// Found by fuzzing (SPEC §55): a key cut short anywhere, or shorter
    /// than an id, gets an answer from every helper, not a panic; a cut
    /// value is never called exact.
    #[test]
    fn damaged_keys_get_cautious_answers() {
        let values = [
            Document::String("with\0zero".into()),
            Document::Int(42),
            Document::Null,
            Document::Bool(true),
            Document::Array(vec![]),
        ];
        let refs: Vec<&Document> = values.iter().collect();
        let key = compound(&refs, DocId([7; 16]));
        let whole = value_part(&key);
        assert_eq!(parts(whole).len(), values.len());
        assert_eq!(doc_id(&key), DocId([7; 16]));
        for len in 0..key.len() {
            let cut = &key[..len];
            let _ = doc_id(cut);
            let value = value_part(cut);
            let split = parts(value);
            assert_eq!(split.concat(), value, "{len}: parts cover the bytes");
            for part in &split {
                let _ = is_other(part);
                let _ = part_is_exact(part, values.len());
            }
        }
        assert_eq!(
            doc_id(&[1, 2]),
            DocId([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2])
        );
        assert_eq!(value_part(&[1, 2]), &[] as &[u8]);
        assert!(!part_is_exact(&[], 1));
        assert!(!part_is_exact(&[TAG_NUMBER, 1, 2], 1), "a number cut short");
        assert!(
            !part_is_exact(&[TAG_STRING, b'a'], 1),
            "a string without its end"
        );
        assert!(!is_other(&[]));
        assert_eq!(parts(&[TAG_STRING, b'a', 0]), [&[TAG_STRING, b'a', 0][..]]);
    }
}
