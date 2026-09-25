use crate::storage::PageId;

/// A branch-page cell: `[separator key][u64 child]` (see `btree.rs` for
/// which child that is). Like a leaf cell, the key has no length prefix:
/// it's everything but the last 8 bytes.
pub(super) fn encode_branch_entry(key: &[u8], child: PageId) -> Vec<u8> {
    let mut buffer: Vec<u8> = Vec::with_capacity(key.len() + 8);
    buffer.extend_from_slice(key);
    buffer.extend_from_slice(&child.to_le_bytes());
    buffer
}

/// The shortest branch cell: an empty key and the child.
pub(super) const MIN_BRANCH_ENTRY_LEN: usize = 8;

/// Trusts `bytes` to be at least `MIN_BRANCH_ENTRY_LEN` long, which
/// `btree::read_node` checks for every cell of a page it reads.
pub(super) fn decode_branch_entry(bytes: &[u8]) -> (&[u8], PageId) {
    let (key, child) = bytes.split_at(bytes.len() - 8);
    (key, PageId::from_le_bytes(child.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_branch_entry() {
        for key in [&[1u8; 16][..], b"", b"a longer separator"] {
            let encoded = encode_branch_entry(key, 42);
            assert_eq!(decode_branch_entry(&encoded), (key, 42));
        }
    }
}
