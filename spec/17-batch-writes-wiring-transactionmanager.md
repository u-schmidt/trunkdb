# 17. Batch writes: wiring `TransactionManager` (`database.rs`, `txn.rs`)

Real and tested: `Database::write_batch(ops: Vec<WriteOp>)` is the first
real, public entry point for `TransactionManager`/`GlobalLockTxnManager`
— both existed as dead code (never called by anything) before this. Ops
may target different collections in one call, which is the actual point:
a multi-entity atomic update (§4.4's requirement) needs
exactly that, and a sequence of separate `Collection::insert`/`update`/
`delete` calls can't give it, since each of those is its own
independently-durable unit.

`Collection<Document>`'s own `insert`/`update`/`delete` now delegate to
`write_batch(vec![op])` — collapsing what would otherwise be two
near-identical copies of the log/apply/sync/checkpoint protocol (§16)
into one. A side effect: single-document writes now also go through
`GlobalLockTxnManager`'s lock, which they didn't before.

## 17.1 The prerequisite: giving `WriteOp` its own collection name
`Durability::log`'s signature had grown a bolted-on `collection: &str`
parameter during §16, since at the time every `log` call came from one
`Collection` instance operating on one collection. That doesn't work for
a batch spanning several collections. Fix: move the collection name into
`WriteOp` itself (`Insert(String, DocId, Document)`, etc.) — which let
`Durability::log` drop the parameter and go back to its original,
cleaner `log(&mut self, ops: &[WriteOp])` shape, and let
`durability/wal.rs`'s record encoding drop a whole layer of
now-redundant name-handling (each op already carries it).

## 17.2 Partial batch application: recovery replays the whole batch, not just what's missing
*Superseded by §19: recovery now restores page images, not ops.*
A batch is logged as one WAL unit but still applied op-by-op, so a crash
partway through the apply loop can leave some ops durably on the main
file and others not, even though the whole batch is still sitting in the
(not yet checkpointed) WAL. No batch-specific recovery logic was needed
for this: replaying every op in the batch again and relying on §16.1's
per-op idempotency (insert skips if present, update/delete skip if
absent) produces the correct end state regardless of how far the crash
got. Tested directly (`recovers_a_batch_where_only_some_ops_were_already_applied`,
`database.rs`): manually applies only the first op of a two-op batch,
then confirms `Database::open` recovers both correctly.

## 17.3 No rollback — now finally exercisable, still unchanged
*Superseded by §19.5: rollback is real now; the test described here
became `a_failed_batch_leaves_no_trace`.*
`GlobalLockTxnManager`'s doc comment documented "no rollback of
already-applied ops on partial failure" before any of this was wired up.
Tested directly now that there's a real way to trigger it
(`a_failed_batch_does_not_roll_back_ops_already_applied`): a batch where
op 1 succeeds and op 2 is an oversized document `data::insert_record`
rejects — `write_batch` returns an error, but op 1's effect is still
visible afterward. Real rollback needs staged writes in the storage
layer; still not built.
