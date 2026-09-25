# 5. Reference workloads

Three workload shapes, taken from real applications, calibrate what
trunkdb has to do. v0 was built standalone against synthetic data shaped
like these. The sync workload (§5.3) is the first planned live use, the
large-document workload (§5.2) the second; see [ROADMAP.md](../ROADMAP.md) for the roadmap.

## 5.1 Time-series workload
A data-shape and query-pattern reference, not a planned integration:
- A mix of high-frequency, append-only data (location pings, event logs)
  and small reference tables (geofences, a singleton state row).
- Query patterns: time-range filters (`tst >= ? AND tst <= ?`) combined
  with sort + limit; several optional filters ANDed together dynamically;
  "most recent row" (`ORDER BY id DESC LIMIT 1`); one unique constraint
  (`zone_key`); one aggregate (`LEFT JOIN` + `GROUP BY` + `COUNT`) that a
  document DB should *not* try to replicate — that becomes application
  code instead.
- In a relational store, such data ends up as JSON blobs in TEXT
  columns — exactly the pattern a document store removes.

## 5.2 Large-document workload (a desktop app)
A Rust desktop app that keeps its data as a directory of files, one per
entity. Integration plan: trunkdb becomes an *alternative* `Storage`
backend behind a trait, selectable against the file-based one.

What its storage code showed:
- About ten entity kinds sharing common fields — a document-shaped data
  model, arguably a better fit than a relational one. Some entities hold
  long-form text: tens to hundreds of KB per document.
- Every lookup rescans and re-parses the whole directory, including from
  inside write operations — get-by-id is effectively O(all files).
- Relationships between entities are hand-maintained linked lists of
  string ids, with manual re-pointing and cycle detection — a real index
  would replace much of that logic.
- Multi-entity writes get best-effort atomicity by snapshotting and
  restoring file contents on failure; there is no real transaction
  protocol for loose files. This directly motivated making atomic batch
  writes a v0 feature (§4.4).

**Planned integration sequencing**: build trunkdb standalone and solid
first → refactor the app to define a `Storage` trait that its existing
file-based writer implements unchanged (pure refactor, no behavior
change) → only then write a second `Storage` impl backed by trunkdb.
Two moving targets (a new storage engine and a live app's integration)
are deliberately not built at the same time.

## 5.3 Sync workload (a small app replacing MongoDB)
A small Rust desktop app, low thousands of documents in one main
collection, currently on MongoDB — genuine overkill here, and the app's
only reason to need a running database server. Its whole database
surface is three operations:
- **Sync**: a periodic run looks every record up by the app's own
  business key (independent of `_id`), inserts new ones and updates the
  rest (at least a `last_seen` field) — ideally as one atomic batch.
- **Query**: a dynamic AND of optional exact-match filters, a
  case-insensitive substring search on two text fields, and a dynamic
  sort field/direction plus limit.
- **Partial update** of a few fields, which becomes read-modify-write
  against `Collection::update`'s whole-document replace — an
  application-level detail, not a gap.

A prototype ran this workload against trunkdb 0.1.0, on a few hundred
real documents:
- **Document size**: ~500 B min / ~900 B avg / 3 KB max — well within
  one page (no overflow involved, §26.2).
- **Space**: one 8 KB page per document (§11.4), ~9× overhead. Harmless
  at this scale; still the reason to fix the data-page format before
  real data lands — since done (§20).
- **`find()` didn't return `DocId`s** (typed path): the prototype kept a
  business-key → `DocId` map from `insert`, which is gone after a
  restart. The sync looks records up by key and then updates them, so
  this was the one real functional blocker — resolved by
  `find_with_ids` (§23).
- **Substring search** was done app-side after a full scan — works at
  this scale, but belongs in `Filter` (since §25: `Op::Contains`).
- **Every write was its own batch** (two `fsync`s each), while a sync
  touches every record — it wants one typed, atomic batch per run
  (since §24).
- **Threading**: `Database` was `Send`, so `Mutex<Database>` in the
  app's shared state worked; since §27 `Database` itself goes there.
- Secondary indexes aren't needed: at low thousands of documents a full
  scan takes milliseconds.
