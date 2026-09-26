use super::{TransactionManager, WriteOp};
use std::sync::Mutex;

/// v0's entire concurrency story: one process-wide lock held for the
/// duration of a batch. No concurrent writers, no isolation levels — just
/// enough to make `write_batch` atomic, which is what multi-entity updates
/// need (SPEC §4.4: a file-based writer only has best-effort rollback via
/// re-writing file contents, not a real transaction).
///
/// Since SPEC §27, `Database::write_batch` holds the database's write
/// lock around this call, so this lock is never contended. It stays as
/// this implementation's own guarantee rather than something it borrows
/// from its caller.
///
/// Stops at the first failing op and returns its error. Undoing the ops
/// applied before it isn't this type's job: `Database::write_batch` runs
/// the whole batch against staged pages (`FileStore::begin`) and rolls
/// them back on error, so nothing of a failed batch ever reaches the file.
#[derive(Default)]
pub struct GlobalLockTxnManager {
    lock: Mutex<()>,
}

impl TransactionManager for GlobalLockTxnManager {
    fn apply_batch(
        &self,
        ops: Vec<WriteOp>,
        apply: &mut dyn FnMut(WriteOp) -> crate::Result<()>,
    ) -> crate::Result<()> {
        let _guard = self
            .lock
            .lock()
            // A writer panicked while holding it: like a failed write,
            // reopening recovers what's committed (SPEC §60).
            .map_err(|_| crate::Error::Poisoned)?;
        for op in ops {
            apply(op)?;
        }
        Ok(())
    }
}
