use crate::decode::{put_varint, take_varint};
use crate::storage::RecordLocation;
use std::cmp::Ordering;

/// A leaf-page cell: `[key][u64 page][u16 slot]`. The key comes first
/// and has no length prefix — the slot directory already records the
/// cell's length, so the key is everything but the last 10 bytes. For
/// the primary index (16-byte `DocId` keys) that's byte for byte the
/// fixed 26-byte entry this always was. Leaves written before file
/// format 14 hold these; a write to one rewrites it packed.
#[cfg(test)]
pub(super) fn encode_index_entry(key: &[u8], location: RecordLocation) -> Vec<u8> {
    let mut buffer: Vec<u8> = Vec::with_capacity(key.len() + 8 + 2);
    buffer.extend_from_slice(key);
    buffer.extend_from_slice(&location.page.to_le_bytes());
    buffer.extend_from_slice(&location.slot.to_le_bytes());
    buffer
}

/// The shortest leaf cell: an empty key and the location.
pub(crate) const MIN_INDEX_ENTRY_LEN: usize = 10;

/// Trusts `bytes` to be at least `MIN_INDEX_ENTRY_LEN` long, which
/// `btree::read_node` checks for every cell of a page it reads.
pub(super) fn decode_index_entry(bytes: &[u8]) -> (&[u8], RecordLocation) {
    let (key, loc) = bytes.split_at(bytes.len() - 10);
    let page = u64::from_le_bytes(loc[0..8].try_into().unwrap());
    let slot = u16::from_le_bytes(loc[8..10].try_into().unwrap());
    (key, RecordLocation { page, slot })
}

// A packed leaf (`PageType::IndexLeafPacked`, file format 14): its first
// cell is the page's anchor, `[0][anchor key]`, any key, chosen when the
// page is built; every other cell is an entry, `[varint shared][varint
// page][varint slot][rest]`, whose key is the anchor's first `shared`
// bytes and then `rest`. Neighbouring keys share long beginnings — the
// same first field of a compound index, the same leading path, ids made
// in the same second — so most of each key is the anchor's. Any entry
// reads on its own, by its slot: a binary search, a walk, an insert or a
// remove touches no other entry.

/// The anchor cell's first byte, so even an empty anchor is a cell, not
/// a tombstone.
const ANCHOR: u8 = 0;

/// The anchor cell for `anchor`.
pub(super) fn encode_anchor(anchor: &[u8]) -> Vec<u8> {
    let mut cell = Vec::with_capacity(1 + anchor.len());
    cell.push(ANCHOR);
    cell.extend_from_slice(anchor);
    cell
}

/// The anchor in its cell, which `check_packed` has checked.
pub(super) fn decode_anchor(cell: &[u8]) -> &[u8] {
    &cell[1..]
}

/// How many bytes `a` and `b` begin with alike.
pub(super) fn shared_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// `key`'s entry on a page anchored at `anchor`.
pub(super) fn encode_packed(anchor: &[u8], key: &[u8], location: RecordLocation) -> Vec<u8> {
    let shared = shared_len(anchor, key);
    let mut cell = Vec::with_capacity(8 + key.len() - shared);
    put_varint(shared as u64, &mut cell);
    put_varint(location.page, &mut cell);
    put_varint(u64::from(location.slot), &mut cell);
    cell.extend_from_slice(&key[shared..]);
    cell
}

/// A packed entry's key and location; `anchor` is its page's. Trusts the
/// cell, which `check_packed` has checked with the page.
pub(super) fn decode_packed<'a>(
    cell: &'a [u8],
    anchor: &'a [u8],
) -> (KeyParts<'a>, RecordLocation) {
    let mut rest = cell;
    let mut next = || take_varint(&mut rest, "a packed index entry").unwrap_or(0);
    let shared = next() as usize;
    let page = next();
    let slot = next() as u16;
    let head = &anchor[..shared.min(anchor.len())];
    (KeyParts { head, tail: rest }, RecordLocation { page, slot })
}

/// Checks a packed leaf's cells, in slot order (SPEC §55): the first the
/// anchor, the others entries whose varints end, whose slot is a `u16`,
/// and which share no more of the anchor than it has. What
/// `decode_packed` then trusts. The page's layout check runs it, once
/// for each page read from the file.
pub(crate) fn check_packed<'a>(mut cells: impl Iterator<Item = &'a [u8]>) -> Result<(), String> {
    let Some(first) = cells.next() else {
        return Ok(());
    };
    if first.first() != Some(&ANCHOR) {
        return Err("a packed index leaf's first cell isn't its anchor".into());
    }
    let anchor = decode_anchor(first);
    for cell in cells {
        let mut rest = cell;
        let mut next = || {
            take_varint(&mut rest, "")
                .map_err(|_| "a packed index entry's varint doesn't end".to_string())
        };
        let shared = next()?;
        next()?;
        let slot = next()?;
        if shared > anchor.len() as u64 {
            return Err(format!(
                "a packed index entry shares {shared} bytes of an anchor of {}",
                anchor.len()
            ));
        }
        if slot > u64::from(u16::MAX) {
            return Err(format!("a packed index entry's slot is {slot}"));
        }
    }
    Ok(())
}

/// A key as a packed leaf holds it: `head`, from the anchor, then
/// `tail`. A plain leaf's key is all `tail`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyParts<'a> {
    pub head: &'a [u8],
    pub tail: &'a [u8],
}

impl<'a> KeyParts<'a> {
    pub fn whole(key: &'a [u8]) -> Self {
        KeyParts {
            head: &[],
            tail: key,
        }
    }

    /// The key against `other`, byte by byte, as `[u8]` compares.
    pub fn cmp(&self, other: &[u8]) -> Ordering {
        let n = self.head.len().min(other.len());
        match self.head.cmp(&other[..n]) {
            Ordering::Equal if n < self.head.len() => Ordering::Greater,
            Ordering::Equal => self.tail.cmp(&other[n..]),
            unequal => unequal,
        }
    }

    pub fn len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// The key, written over `buffer`.
    pub fn write_to(&self, buffer: &mut Vec<u8>) {
        buffer.clear();
        buffer.extend_from_slice(self.head);
        buffer.extend_from_slice(self.tail);
    }

    /// The key, written over `buffer`, whose first `kept` bytes are
    /// already the anchor's: only what differs is copied. `kept` is then
    /// how many of the new key's first bytes are the anchor's. A plain
    /// leaf's key, with no head, is copied whole.
    pub fn write_over(&self, buffer: &mut Vec<u8>, kept: &mut usize) {
        let shared = self.head.len();
        buffer.truncate(shared.min(*kept));
        if shared > *kept {
            buffer.extend_from_slice(&self.head[*kept..]);
        }
        buffer.extend_from_slice(self.tail);
        *kept = shared;
    }

    pub fn to_vec(self) -> Vec<u8> {
        let mut key = Vec::with_capacity(self.len());
        self.write_to(&mut key);
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_index_entry() {
        for key in [&[1u8; 16][..], b"", b"a longer, variable-length key"] {
            let location = RecordLocation { page: 42, slot: 7 };
            let encoded = encode_index_entry(key, location);
            assert_eq!(decode_index_entry(&encoded), (key, location));
        }
    }

    #[test]
    fn a_packed_entry_is_its_key_and_location_whatever_the_anchor() {
        let keys: [&[u8]; 6] = [b"", b"a", b"abc", b"abd", b"abcdef", b"zz"];
        for anchor in keys {
            for key in keys {
                for location in [
                    RecordLocation { page: 0, slot: 0 },
                    RecordLocation {
                        page: u64::MAX,
                        slot: u16::MAX,
                    },
                    RecordLocation {
                        page: 300,
                        slot: 127,
                    },
                ] {
                    let cell = encode_packed(anchor, key, location);
                    let anchor_cell = encode_anchor(anchor);
                    assert_eq!(
                        check_packed([&anchor_cell[..], &cell[..]].into_iter()),
                        Ok(())
                    );
                    let (parts, found) = decode_packed(&cell, decode_anchor(&anchor_cell));
                    assert_eq!((parts.to_vec(), found), (key.to_vec(), location));
                    for other in keys {
                        assert_eq!(parts.cmp(other), key.cmp(other), "{key:?} {other:?}");
                    }
                }
            }
        }
        let small = encode_packed(
            b"tenant-7/0001",
            b"tenant-7/0002",
            RecordLocation { page: 9, slot: 3 },
        );
        assert_eq!(small.len(), 3 + 1, "the shared 12 bytes aren't stored");
    }

    #[test]
    fn a_damaged_packed_leaf_is_refused() {
        let anchor = encode_anchor(b"abc");
        let good = encode_packed(b"abc", b"abd", RecordLocation { page: 1, slot: 1 });
        assert!(
            check_packed([&good[..]].into_iter()).is_err(),
            "no anchor first"
        );
        let mut sharing_too_much = good.clone();
        sharing_too_much[0] = 9;
        assert!(check_packed([&anchor[..], &sharing_too_much[..]].into_iter()).is_err());
        assert!(
            check_packed([&anchor[..], &[0x80][..]].into_iter()).is_err(),
            "cut short"
        );
        let mut big_slot = Vec::new();
        for v in [0, 1, 70_000] {
            put_varint(v, &mut big_slot);
        }
        assert!(check_packed([&anchor[..], &big_slot[..]].into_iter()).is_err());
    }
}
