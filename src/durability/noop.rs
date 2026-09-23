use super::Durability;
use crate::storage::PageId;

/// Does nothing. Kept only for tests/callers that want `Collection`'s
/// storage plumbing without any real file I/O — `Database` itself uses
/// `WalDurability`, the real implementation. A crash mid-write against
/// `NoopDurability` can corrupt the file; that's the explicit trade-off
/// of choosing this over the real one.
#[derive(Default)]
pub struct NoopDurability;

impl Durability for NoopDurability {
    fn log(&mut self, _pages: &[(PageId, &[u8])]) -> std::io::Result<()> {
        Ok(())
    }

    fn checkpoint(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
