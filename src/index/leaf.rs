use crate::storage::RecordLocation;

/// A leaf-page cell: `[key][u64 page][u16 slot]`. The key comes first
/// and has no length prefix — the slot directory already records the
/// cell's length, so the key is everything but the last 10 bytes. For
/// the primary index (16-byte `DocId` keys) that's byte for byte the
/// fixed 26-byte entry this always was.
pub(super) fn encode_index_entry(key: &[u8], location: RecordLocation) -> Vec<u8> {
    let mut buffer: Vec<u8> = Vec::with_capacity(key.len() + 8 + 2);
    buffer.extend_from_slice(key);
    buffer.extend_from_slice(&location.page.to_le_bytes());
    buffer.extend_from_slice(&location.slot.to_le_bytes());
    buffer
}

pub(super) fn decode_index_entry(bytes: &[u8]) -> (&[u8], RecordLocation) {
    let (key, loc) = bytes.split_at(bytes.len() - 10);
    let page = u64::from_le_bytes(loc[0..8].try_into().unwrap());
    let slot = u16::from_le_bytes(loc[8..10].try_into().unwrap());
    (key, RecordLocation { page, slot })
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
}
