# 3. Architecture: layers and what's real vs. fakeable

An embedded document database breaks into layers. Each sits behind a small
trait so a naive v0 implementation can be swapped for a real one later
without reshaping anything above it — **except** the storage/page layer and
the document format, where the *contract* needs to be right early because
everything else is expressed in terms of it (even though the storage
layer's own implementation is still incomplete in v0).

| Layer | Trait | v0 implementation | Real from day one? |
|---|---|---|---|
| Storage/pages | `PageStore` | `FileStore` (real, tested — see §7) | Yes |
| Document format | — (`Document` enum) | real enum + byte encode/decode (§11) + serde bridge (§13) + tagged JSON (§30.1) | Yes |
| Id generation | `IdGenerator` | `UuidV7Generator` | Yes |
| Indexing | `Index` | `BTreeIndex` (persisted B-tree, O(log n), byte-string keys; primary `_id` index §10, secondary indexes §28) + `InMemoryIndex` (fake, tests only) | Yes |
| Transactions | `TransactionManager` | `GlobalLockTxnManager` (one global lock), wired via `Database::write_batch` (§17); rollback via staged pages (§19.5) | Yes — atomicity and rollback; no isolation |
| Durability | `Durability` | `WalDurability` (page-image write-ahead log, §19; op-level before that, §16) + `NoopDurability` (fake, tests only) | Yes |
| Query execution | — (`Filter`) | comparisons combined by AND, OR and NOT (§36), built with a builder (§35); a full scan, one secondary index's range (§28.4), a union of ranges for an OR (§36.3), or an index read in sort order up to the limit (§34.2), by a fixed rule | No — deliberately minimal |
| Public API | `Collection<T>`, `Database` | Both `Collection<Document>` (§12) and typed `Collection<T>` (§13.3) real | Yes |
