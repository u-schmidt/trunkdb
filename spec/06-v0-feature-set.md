# 6. v0 feature set

**In scope, real (no faking):**
- Single-file database, page-based storage
- `Document` value type with typed (`Collection<T>`) and dynamic
  (`Collection<Document>`) access via one serde-based mechanism
- UUIDv7 ids, pluggable generator
- CRUD: insert (auto-id), get-by-id, update-by-id (full replace),
  delete-by-id, iterate-all — later joined by `find_one`, `count`,
  `upsert` and a streaming `cursor` (§29)
- Filter: comparison operators ANDed together, sort-by-field, limit
- Atomic multi-document, multi-collection batch write (§17) — all ops
  land or none do: a failing op rolls the whole batch back, and a crash
  leaves either the pre- or the post-batch state (§19)
- Crash survival via a page-image write-ahead log (§19)
- A persisted B-tree primary (`_id`) index (§10)
- Documents larger than a page, via overflow pages (§26)

Originally planned as fakes behind a trait (`Index` as linear scan,
`Durability` as no-op, `TransactionManager` unwired) — all three have
since been replaced by real implementations; §3's table is the current
state. The fakes (`InMemoryIndex`, `NoopDurability`) remain, for tests
only.

**Explicitly out of v0:**
- A real (cost-based) query planner; secondary indexes were added
  later (§28), with a fixed rule for when to use one
- Joins, aggregates (GROUP BY/COUNT-style — left to application code,
  per §5.1); OR/nested conditions were added later (§36)
- Concurrent multi-process access; concurrent writers (batches are
  serialized, §27)
- Compaction/vacuum (added later, §41), encryption, schema
  validation; online backups (export, §30, is a snapshot, but writers
  wait for it)
