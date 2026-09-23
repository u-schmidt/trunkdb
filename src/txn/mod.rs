mod global_lock;
pub use global_lock::GlobalLockTxnManager;

use crate::document::{DocId, Document};

/// Every op names its own collection — necessary once ops can travel
/// together in a batch (`Database::write_batch`) spanning more than one
/// collection at once, which is the actual motivating case (§4.4:
/// multi-entity updates). A single-collection call (`Collection::write`)
/// just fills this in from `self.name`.
#[derive(Debug, Clone)]
pub enum WriteOp {
    Insert(String, DocId, Document),
    Update(String, DocId, Document),
    Delete(String, DocId),
}

#[derive(Debug, thiserror::Error)]
pub enum TxnError {
    #[error("write failed: {0}")]
    Failed(String),
}

/// Fakeable by design: v0's `GlobalLockTxnManager` serializes batches
/// with one lock and runs each op through the caller's `apply` closure,
/// stopping at the first error. All-or-nothing itself comes from the
/// storage layer: `Database::write_batch` stages every page write
/// (`FileStore::begin`) and rolls the whole batch back on error. No
/// isolation yet — that can come later behind this same trait, e.g. MVCC.
///
/// An op's error passes through unchanged (not wrapped in `TxnError`),
/// so callers can match on e.g. `Error::NotFound`.
pub trait TransactionManager {
    fn apply_batch(
        &self,
        ops: Vec<WriteOp>,
        apply: &mut dyn FnMut(WriteOp) -> crate::Result<()>,
    ) -> crate::Result<()>;
}
