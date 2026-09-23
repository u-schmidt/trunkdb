mod branch;
mod btree;
mod in_memory;
pub mod key;
mod leaf;
pub use btree::BTreeIndex;
pub use in_memory::InMemoryIndex;
pub use key::KeyRange;

use crate::storage::{PageStore, RecordLocation};

/// An ordered map from byte-string keys to record locations. Keys are
/// compared byte by byte; `key.rs` builds them, so an implementation
/// needs to know nothing about documents (SPEC §28.1).
///
/// Fakeable by design: `InMemoryIndex` ignores the `store` parameter
/// entirely (it's pure in-memory), kept around for tests that don't want
/// real file I/O. `BTreeIndex` is the real, disk-backed implementation
/// `Database` actually uses, for the primary `_id` index and for every
/// secondary index. `range` returns an eagerly-collected `Vec` rather
/// than a lazy iterator — a persisted implementation's iteration is
/// fallible (disk I/O per step), and a lazily-evaluated fallible iterator
/// isn't worth the complexity at this stage.
pub trait Index {
    fn insert(
        &mut self,
        store: &mut dyn PageStore,
        key: &[u8],
        loc: RecordLocation,
    ) -> std::io::Result<()>;
    fn remove(&mut self, store: &mut dyn PageStore, key: &[u8]) -> std::io::Result<()>;
    fn lookup(&self, store: &dyn PageStore, key: &[u8]) -> std::io::Result<Option<RecordLocation>>;
    /// Every entry with a key in `range`, in ascending key order.
    fn range(
        &self,
        store: &dyn PageStore,
        range: &KeyRange,
    ) -> std::io::Result<Vec<(Vec<u8>, RecordLocation)>>;

    /// Every entry, in ascending key order.
    fn scan(&self, store: &dyn PageStore) -> std::io::Result<Vec<(Vec<u8>, RecordLocation)>> {
        self.range(
            store,
            &KeyRange {
                start: Vec::new(),
                end: None,
            },
        )
    }
}
