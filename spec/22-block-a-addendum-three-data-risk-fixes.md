# 22. Block A addendum: three data-risk fixes (`storage/file.rs`, `collection.rs`, `catalog.rs`, `txn/`)

After Block A, a review of the code for anything that could damage or
silently lose data turned up three problems — each confirmed with a
throwaway test before fixing, each now covered by a real one.

## 22.1 `open` no longer overwrites small foreign files
`FileStore::open` treated every file shorter than one page as fresh
(§19.7), so `Database::open("notes.txt")` on a 2 KB text file bootstrapped
a database *over it* — the one way trunkdb could destroy data that
wasn't its own. A non-empty short file is now fresh only if it starts
with the magic (or a prefix of it, for a cut inside the first 8 bytes):
a first write-back cut short always does, because the header page is
written first. Anything else is "not a trunkdb file (bad magic)",
left untouched. Since §21.1 opens the store before the WAL, no stray
`<path>.wal` is created either (`opening_a_foreign_file_changes_nothing`).

## 22.2 Duplicate and missing ids fail the batch
Inside `write_batch`, an `Insert` of an id that already existed, and an
`Update`/`Delete` of one that didn't (or of a collection that didn't),
were skipped — and the batch returned `Ok`. A leftover of op replay
(§16.1), which page-image recovery (§19.4) no longer needs; for a
caller, "saved" when nothing was saved. Block B's typed batch API (§24,
the sync workload's batch) would have inherited it.

Now `apply_write_op` returns `Error::DuplicateId { collection, id }` or
`Error::NotFound { collection, id }`, and the whole batch rolls back.
For those to be matchable, `TransactionManager::apply_batch` passes
the op's `crate::Error` through instead of flattening it into
`TxnError::Failed(String)` — which also keeps every other op error
(e.g. `InvalidInput` for a too-large document) intact. `TxnError`
remains for the manager's own failures (a poisoned lock).

The single-op API keeps its shape: `Collection::update`/`delete` still
return `Ok(false)` for a missing id — for one op that's an ordinary
answer, not an error. They now map `Error::NotFound` to `false` instead
of checking existence first, which saves an index lookup.

## 22.3 Collection names are limited to 255 bytes
A catalog cell must fit in one page. For a name that didn't fit even
an empty catalog page, `create_collection` chained new catalog pages
forever — staged, so the file was safe, but `insert` hung while memory
grew. Names over `MAX_COLLECTION_NAME_LEN` (255 UTF-8 bytes) are now an
`InvalidInput` error; 255 is generous for a name and keeps catalog
pages holding dozens of entries.

## 22.4 Reviewed, deliberately not changed
- **A panic mid-batch** (only a bug or a corrupt file can cause one)
  leaves `FileStore` staging: the file stays safe, but a caller that
  catches the panic would read uncommitted pages in-process. Poisoning
  on unwind would close it; not worth it before there's a known path.
  *Closed by §27.3, for free: the panic poisons the lock.*
- **The header is decoded before WAL recovery** (§19.4). It would only
  matter if a power loss tore the header page, and all of the header's
  fields sit in its first 512 bytes, which disks write atomically.
  *Since §40 the header's checksum sits in its last bytes, so it's
  checked only after recovery (§40.4).*
- **`u16` lengths** in the document encoding can't wrap today, since a
  whole document must fit in one page; overflow pages (§26) had to widen
  them, as already planned. *Done in §26.1.*
- **Bit rot** stays undetected until per-page checksums. *Closed by
  §40.*
