mod noop;
mod wal;
pub use noop::NoopDurability;
pub use wal::WalDurability;

use crate::storage::PageId;

/// `WalDurability` is the real implementation: an append-only log file
/// separate from the main data file, holding *page images* — the full
/// after-state of every page a batch changed (SPEC §19). `log` writes
/// one batch's images to it and `fsync`s before any of those pages
/// reaches the main file; once they have (and are themselves `fsync`ed),
/// `checkpoint` truncates the log back to empty. `Database::open` writes
/// back whatever `checkpoint` never got to truncate. Rewriting a page
/// image is idempotent by nature, so a batch that already fully reached
/// the main file can be replayed again harmlessly.
pub trait Durability {
    /// Durably records one batch's changed pages — `(page id, page bytes)`,
    /// each exactly `storage::USABLE_PAGE_SIZE` long — as a single unit: after a
    /// crash, recovery sees either all of them or none.
    fn log(&mut self, pages: &[(PageId, &[u8])]) -> std::io::Result<()>;
    /// Marks every batch logged so far as durably reflected in the main
    /// file — safe to forget, since replaying it is no longer necessary.
    fn checkpoint(&mut self) -> std::io::Result<()>;
}
