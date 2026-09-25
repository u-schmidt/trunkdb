# 16. Durability: a real write-ahead log (`durability/wal.rs`, `txn.rs`)

*History: this op-level WAL was replaced by a page-image WAL in §19,
after it turned out replay couldn't repair a structure a crash had left
half-written. Kept for the reasoning; §16.1–§16.4 describe code that no
longer exists in this form.*

Real and tested: `WalDurability` replaces `NoopDurability` as what
`Database` actually uses. Every `Collection<Document>` write (`insert`/
`update`/`delete` — `get`/`find` need none of this) now goes through
`Collection::write`'s four-step protocol: **log** the op (a `WriteOp`) to
a separate append-only file (`<path>.wal`), `fsync`ed; **apply** it for
real (the same `data.rs`/`BTreeIndex` calls as before); **sync** the main
file (one `fsync` after all of the operation's page writes, not one per
write — a single `fsync` flushes every dirty page for that file
regardless of how many separate writes produced them); **checkpoint**
(truncate the WAL to empty, now that the op is durably reflected in the
main file). `Database::open` reads back whatever a prior run's crash left
pending and replays it — via `apply_write_op` (§16.2), the same function
`Collection::write` itself calls — before returning a usable `Database`.

## 16.1 Why replay has to be idempotent
A crash can land anywhere in the four-step protocol, including *after*
the main file was fully synced but *before* `checkpoint` finished
truncating the WAL — so recovery may replay an op that's already durably
applied. `apply_write_op` is written to tolerate that: `Insert` skips if
the id already exists, `Update`/`Delete` skip if it's already gone —
either way a no-op, never a duplicate or an error. Two tests
(`database.rs`) inject a WAL record directly, bypassing `Collection`, to
exercise both crash points: one where the op was only ever logged, one
where it was already applied — confirming recovery produces the same end
state either way.

## 16.2 One apply function, shared by the live path and recovery
`apply_write_op(catalog, store, collection, op)` (`collection.rs`) is the
single place a `WriteOp` actually gets applied to data/index pages — used
by both `Collection::write` and `Database::open`'s recovery loop, so the
two can't drift apart. `get_or_create_meta` (the former `Collection::meta`
method) became a free function for the same reason: recovery has no
`Collection` instance to call a method on.

## 16.3 A real conflict with `decode_document`, and how it's resolved
§11.2 documents `decode_document` as trusting its input enough to panic
on a truncated buffer rather than erroring — reasonable everywhere else
in the crate, since nothing reads a cell's bytes except code that wrote
them. A WAL's tail is exactly the place truncation legitimately happens
(a crash mid-append), and handing that buffer to a function documented to
panic on truncation would crash `Database::open` itself — the opposite of
durability.

Fix: the WAL's own framing keeps `decode_document` from ever seeing a
possibly-torn buffer. Each record carries its own `u32` length prefix
(`durability/wal.rs`); recovery checks "are there `record_len` more bytes
here?" *before* slicing that record out and decoding it. A torn tail
(fewer bytes than promised) is caught at that check and silently
discarded. A record that *is* length-complete but still fails to decode
is a different, stronger signal — real corruption, not a crash artifact —
and errors hard instead. Tested directly: a test truncates a real WAL
file mid-record and confirms recovery drops it rather than erroring.

## 16.4 Smaller decisions
- `FileStore::sync` is a new inherent method (`file.sync_all()`), not
  added to the `PageStore` trait — §7's "one load-bearing seam... not
  meant to be faked" stance argues for leaving that trait alone;
  `Collection::write` calls `sync()` directly on the concrete `FileStore`
  it already holds.
- `Durability::log` gained a `collection: &str` parameter. `WriteOp`
  itself wasn't changed to carry it, since within one `Collection`'s
  method the collection name is already available from `self.name` — but
  recovery has no such context, so the WAL's on-disk record needs the
  name explicitly even though the in-memory `WriteOp` doesn't.
- `WriteOp`'s byte codec (`encode_write_op`/`decode_write_op`, `txn.rs`)
  reuses `document::encode_document`/`decode_document` for the payload,
  the same pattern `data.rs`'s record codec already used.

## 16.5 Deliberately still out of scope, at the time this was built
`TransactionManager`/`GlobalLockTxnManager` (§4.4, multi-op batch
atomicity) was still completely unwired when this milestone landed —
`Durability::log` took a slice because it was designed to compose with
batches later, but `Collection` only ever passed single-op slices.
Wired up next, §17. Concurrency (§4.6) remains untouched.

## 16.6 One WAL record per batch, not per op (a bug fix)
As first built (§16, carried into §17), `log` framed each op as its own
`[u32 len][op]` record. That was fine while every batch had one op, but
once §17 made multi-op batches real it broke their atomicity: a crash
*during* `log` could leave the first ops' records complete on disk and
the last one torn. Recovery would discard the torn record and replay the
rest — surfacing part of a batch whose `write_batch` never returned, and
which was never applied at all.

Fix: one record per batch — `[u32 body_len][u32 crc32(body)][body]`,
with `body` = `[u32 op_count]` + `op_count` × `[u32 op_len][op]`. The
record is either wholly intact (replay every op) or torn (replay none).
Recovery still hands `Database::open` a flat `Vec<WriteOp>`; batch
boundaries don't need to survive past decoding, since §16.1's per-op
idempotency makes replaying whole batches in log order correct.

The CRC closes a second gap the length prefix alone couldn't: a record
whose length is complete but whose bytes didn't all reach the disk. A
bad CRC on the *last* record is treated as a torn tail (ignored); a bad
CRC with more records after it can't be a crash artifact, so it's a
hard `InvalidData` error. CRC-32 is implemented inline (bitwise, ~10
lines) rather than pulled in as a crate — it runs once per batch.
*Since §40 it's CRC-32C, shared with the page checksums, in hardware
where the CPU has it.*

Tested at both levels: `a_batch_torn_between_its_ops_recovers_none_of_them`
(`wal.rs`) and `a_batch_torn_while_being_logged_is_not_partially_recovered`
(`database.rs`, fails against the old framing). The framing and CRC
carried over into §19's page-image records unchanged.
