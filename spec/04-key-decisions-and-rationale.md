# 4. Key decisions and rationale

## 4.1 Document model
Own `Document` enum (`Null`, `Bool`, `Int`, `Float`, `String`, `Binary`,
`Array`, `Object`, `Id`) — the schema-less value type everything else is
expressed in. Equivalent in spirit to LiteDB's `BsonValue` / MongoDB's BSON
/ `serde_json::Value`.

`Collection<T>` converts `T: Serialize + DeserializeOwned` to/from
`Document` by implementing serde's `Serializer`/`Deserializer` traits
*against* `Document` (the same technique the `bson` and `serde_json` crates
use) — real and tested, see §13. Built in the opposite order from how this
section originally imagined it: `Collection<Document>` (the untyped path)
was wired to real storage first (§12), and typed `Collection<T>` second,
as a thin layer converting to/from `Document` and then delegating to
`Collection<Document>` for the actual storage work (§13.3) — not two
separate code paths, matching the original intent here, just arrived at
from the other direction.

## 4.2 Id generation: UUIDv7, not UUIDv4
Two reasons this isn't just cosmetic:
- **Size**: 16 bytes either way (ids duplicate into every index entry and
  every reference, so size matters), but the sortability point below is the
  bigger one.
- **B-tree insert locality**: UUIDv7 embeds a timestamp prefix, so ids
  created in sequence sort close together. Now that `BTreeIndex` (§10) is
  the real index, sequential-ish ids insert near the "end" of the tree;
  fully random UUIDv4 ids would scatter inserts randomly, causing more
  page splits and worse locality as data grows. UUIDv7 (RFC 9562) gets
  this property while staying a standard UUID, avoiding a hand-rolled id
  format like LiteDB/Mongo's ObjectId.

Id generation is pluggable via `IdGenerator`, in case a caller-supplied
scheme is ever wanted.

## 4.3 Query capability: richer than "equality only"
Originally scoped as single-field equality only. Widened after checking
real reference workloads (§5): v0 supports comparison operators (`=`, `!=`,
`<`, `<=`, `>`, `>=`) and a case-insensitive substring match
(`Op::Contains`, §25) ANDed together, plus sort-by-field and limit (real
and tested, §14) — still scan-based, still no
OR/nesting/planner/joins/aggregates.

## 4.4 Transactions: atomic batch writes are v0, not a stretch goal
Originally scoped as "single global lock, no multi-op atomicity." Widened
after reading a desktop app's storage code (§5.2): **atomic multi-document
batch write** (all-or-nothing across several documents) is a first-class
v0 requirement, because that app's file-based writer only has
best-effort rollback (re-writing snapshotted file contents on failure) and
its own code says so directly — a real transaction primitive is a concrete
improvement, not a nice-to-have.

`GlobalLockTxnManager` gives atomicity (all ops in a batch succeed or none
do, enforced by the caller's `apply` closure returning early on error) but
no isolation. Now wired to a real, public entry point —
`Database::write_batch` — and durable, via the same WAL as every other
write. See §17. A batch that fails partway is rolled back completely:
every page write is staged in memory until the batch commits, so nothing
of a failed batch ever reaches the file (§19.5).

## 4.5 Durability: no longer a no-op
Originally `NoopDurability` (a crash mid-write could corrupt the file —
documented, deliberate, not an oversight). Now real: `WalDurability`, a
write-ahead log that makes every batch crash-safe. First built as an
op-level log (§16), which turned out not to protect writes spanning
several pages; replaced by a page-image log (§19).

## 4.6 Concurrency
v0 is single-process only: a database file is exclusively locked by
whoever opens it (§21.1), so a second process gets a clear error instead
of corrupting the file. Within the process, `Database` is a cloneable,
thread-safe handle (§27): readers run in parallel, write batches one at
a time, and a reader waits while a batch commits.
