use crate::batch::Batch;
use crate::catalog::Catalog;
use crate::collection::{Collection, apply_write_op};
use crate::data;
use crate::durability::{Durability, WalDurability};
use crate::id::UuidV7Generator;
use crate::index::{BTreeIndex, Index};
use crate::storage::{Commit, FileStore, PageId, Pages, SnapshotStore};
use crate::txn::{GlobalLockTxnManager, TransactionManager, WriteOp};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, Weak};

/// Opens the file and owns the whole stack — as a cheap, cloneable,
/// thread-safe handle (SPEC §27): every clone refers to the same open
/// database, and `Database`, `Collection` and `Batch` are all `Send +
/// Sync`, so they can go into an app's shared state, a thread, or a
/// `static`. The file stays open (and locked, §21.1) until the last clone
/// — including those inside `Collection`s and `Batch`es — is dropped.
///
/// Reads (`get`, `find`) run together, each on the commit that was the
/// last when it began, to its end (SPEC §80). Write batches run one at a
/// time, beside the reads, and wait for none of them: a batch is staged,
/// logged and flushed where no reader sees it, and published at once,
/// so a read sees it entirely or not at all (SPEC §19.3, §79).
///
/// # Limits
/// Each where it belongs (SPEC §60), all of them listed here:
/// - [`Database::MAX_COLLECTION_NAME_LEN`]: a collection's name, in bytes.
/// - [`Database::MAX_FIELD_NAME_LEN`]: an indexed field's name, in bytes.
/// - [`Database::MAX_COMPOUND_FIELDS`]: fields in one compound index.
/// - [`Document::MAX_NESTING`](crate::Document::MAX_NESTING): how deeply a
///   document nests.
/// - A document's encoded size: `u32::MAX` bytes (4 GiB).
#[derive(Clone)]
pub struct Database {
    inner: Arc<Shared>,
}

/// What every clone shares (SPEC §79). Readers and the writer meet in
/// two places only: `pages`, which locks itself, and `current`.
struct Shared {
    /// The committed pages: read by everyone, changed by the writer at
    /// commit and at a checkpoint.
    pages: Arc<Pages>,
    /// The last commit, as readers see it (SPEC §78): its pages and its
    /// catalog. Replaced by every commit, never changed. The lock is held
    /// only to take it, by a read, or to replace it, by a commit: a read
    /// keeps the snapshot, not the lock, and reads it whatever is
    /// committed meanwhile (SPEC §80).
    current: RwLock<Snapshot>,
    /// Held shared by every read for as long as it reads, and alone by a
    /// batch that replaces the whole file (`compact`), from before its
    /// commit to after it: the one kind of batch that can't keep the
    /// pages as they were for the reads under way, so it waits for them
    /// (rule 6 of SPEC §57.3, §80.5). No other batch takes it.
    gate: RwLock<()>,
    /// The writer's own, and the lock that makes it one writer at a
    /// time. Held for a whole batch, from staging to its checkpoint;
    /// readers never take it.
    writer: Mutex<Writer>,
    /// Set when a batch was durably logged but couldn't be written to the
    /// main file, even on retry — see `transact`. From then on every call
    /// fails with `Error::Poisoned` until the database is reopened.
    poisoned: AtomicBool,
    id_gen: UuidV7Generator,
    txn: GlobalLockTxnManager,
}

/// What a batch works with, and nobody else: the staged pages and the
/// WAL. Index and data-page code take `&mut dyn PageStore` / `&mut
/// Catalog` as plain parameters rather than holding handles of their own,
/// so the writer's lock is the one place they come from.
pub(crate) struct Writer {
    pub(crate) store: FileStore,
    durability: WalDurability,
    /// `OpenOptions::checkpoint_pages`, as of `open_with`: how many
    /// committed pages may wait before a commit writes them back.
    checkpoint_pages: usize,
    /// `OpenOptions::checkpoint_wal_bytes`, with its default worked out:
    /// how long the WAL may grow before a commit writes back.
    checkpoint_wal_bytes: u64,
    /// The snapshots before the last commit that were still held when
    /// they were replaced, oldest first: the ones a reader may still be
    /// reading (SPEC §80). Weak, so a snapshot ends when its last reader
    /// lets go, and is found gone here.
    past: Vec<Weak<SnapshotInner>>,
    /// Test-only: called once, by the next batch, when it is logged and
    /// flushed and not yet published — to see what goes on beside it.
    #[cfg(test)]
    after_log: Option<Box<dyn FnOnce() + Send>>,
}

/// One committed state of the database (SPEC §78): a commit of the
/// pages, and the catalog as that commit left it. Shared, not copied: a
/// clone is the same snapshot.
#[derive(Clone)]
pub(crate) struct Snapshot(Arc<SnapshotInner>);

struct SnapshotInner {
    commit: Commit,
    catalog: Catalog,
}

impl Snapshot {
    fn new(commit: Commit, catalog: Catalog) -> Self {
        Snapshot(Arc::new(SnapshotInner { commit, catalog }))
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        &self.0.catalog
    }

    /// The snapshot, to read from `pages`: its catalog, and the pages as
    /// of its commit.
    fn reading<'a>(&'a self, pages: &'a Pages) -> Reading<'a> {
        Reading {
            catalog: self.catalog(),
            store: pages.at(self.0.commit),
        }
    }
}

/// What a read works with (SPEC §78): a snapshot's catalog, and the
/// pages as of its commit. Everything a reader sees comes through these
/// two (rule 4 of §57.3).
pub(crate) struct Reading<'a> {
    pub(crate) catalog: &'a Catalog,
    pub(crate) store: SnapshotStore<'a>,
}

/// A read under way (SPEC §80): the snapshot it took when it began,
/// which it reads to its end whatever is committed meanwhile. The
/// writer's store isn't reachable from here, so no read path can go past
/// the snapshot by mistake (SPEC §78).
pub(crate) struct ReadGuard<'a> {
    pages: &'a Pages,
    snapshot: Snapshot,
    /// Keeps a batch that replaces the whole file from committing under
    /// this read (`Shared::gate`).
    _gate: RwLockReadGuard<'a, ()>,
}

impl ReadGuard<'_> {
    /// The snapshot, to read: its catalog and its pages.
    pub(crate) fn snapshot(&self) -> Reading<'_> {
        self.snapshot.reading(self.pages)
    }

    /// The snapshot's catalog, for a read that needs no pages.
    pub(crate) fn catalog(&self) -> &Catalog {
        self.snapshot.catalog()
    }
}

/// How `Database::open_with` opens a database. Made with
/// `OpenOptions::default()` and its setters; the fields are private and the
/// type `#[non_exhaustive]`, so a new option breaks nobody.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenOptions {
    cache_size: usize,
    checkpoint_pages: usize,
    checkpoint_wal_bytes: Option<u64>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            cache_size: crate::storage::DEFAULT_CACHE_SIZE,
            checkpoint_pages: DEFAULT_CHECKPOINT_PAGES,
            checkpoint_wal_bytes: None,
        }
    }
}

impl OpenOptions {
    /// At most this many bytes of the file's pages kept in memory (SPEC
    /// §50); 0 keeps none. Default: `storage::DEFAULT_CACHE_SIZE`, 256 MiB.
    /// It fills only as pages are read.
    pub fn cache_size(mut self, bytes: usize) -> Self {
        self.cache_size = bytes;
        self
    }

    /// Once this many committed pages wait, a commit writes them back to
    /// the file (SPEC §51, §53); 0 or 1 writes them back after every
    /// commit. More: fewer writes, but more memory (8 KB a page) and a
    /// WAL that grows with every commit, not with this number (§53.3);
    /// `checkpoint_wal_bytes` bounds that. Default: 1,000.
    pub fn checkpoint_pages(mut self, pages: usize) -> Self {
        self.checkpoint_pages = pages;
        self
    }

    /// Once the WAL is this many bytes long, a commit writes the waiting
    /// pages back and empties it, even if fewer than `checkpoint_pages`
    /// wait (SPEC §76): commits that change the same few pages again and
    /// again add a page image to the WAL each time, and the next open
    /// after a crash reads all of it back. Checked after a commit, so one
    /// large commit can take the WAL past it. Default: twice the bytes of
    /// `checkpoint_pages` pages, 16 MB at 1,000; raise both together.
    pub fn checkpoint_wal_bytes(mut self, bytes: u64) -> Self {
        self.checkpoint_wal_bytes = Some(bytes);
        self
    }
}

/// How many committed pages may wait in memory, logged but not written
/// back, before a commit writes them back (SPEC §51): 1,000 pages of 8 KB each —
/// SQLite's default for its WAL mode too.
const DEFAULT_CHECKPOINT_PAGES: usize = 1000;

impl Writer {
    /// `Database::checkpoint`: the pages to the file, then the WAL
    /// emptied — only after they're durably in the file. If the WAL can't
    /// be emptied, its records are written back once more at the next
    /// open: harmless, page images are idempotent. Beside the readers
    /// (`Pages::checkpoint`).
    fn checkpoint(&mut self) -> std::io::Result<()> {
        let live = self.live();
        self.store.checkpoint_beside(&live)?;
        let _ = self.durability.checkpoint();
        Ok(())
    }

    /// The commits of the snapshots still open among those replaced,
    /// ascending: what the pages have to keep older versions for (SPEC
    /// §80). Those that have ended are forgotten here. One that ends
    /// just now may still be counted, which keeps a version a little
    /// longer; none can open that isn't counted, since a snapshot is
    /// only ever taken from `Shared::current`.
    fn live(&mut self) -> Vec<u64> {
        self.past.retain(|snapshot| snapshot.strong_count() > 0);
        let open = self.past.iter().filter_map(Weak::upgrade);
        open.map(|snapshot| snapshot.commit.seq()).collect()
    }
}

/// The last handle gone: what's committed goes to the main file, so it's
/// complete without its WAL. Best effort — if it fails, the next open
/// recovers the same pages from the WAL.
impl Drop for Shared {
    fn drop(&mut self) {
        if !*self.poisoned.get_mut()
            && let Ok(writer) = self.writer.get_mut()
        {
            let _ = writer.checkpoint();
        }
    }
}

impl Database {
    /// The longest collection name, in UTF-8 bytes (SPEC §22.3).
    pub const MAX_COLLECTION_NAME_LEN: usize = crate::catalog::MAX_COLLECTION_NAME_LEN;

    /// The longest indexed field name, in UTF-8 bytes (SPEC §28).
    pub const MAX_FIELD_NAME_LEN: usize = crate::catalog::MAX_FIELD_NAME_LEN;

    /// The most fields one compound index may have (SPEC §43).
    pub const MAX_COMPOUND_FIELDS: usize = crate::index::key::MAX_COMPOUND_FIELDS;

    /// Opens the file, then recovers: if a prior run logged a batch and
    /// crashed before checkpointing it (see `durability::WalDurability`),
    /// its page images are still in the WAL. They're written back to the
    /// main file before anything — including `Catalog::load` — reads it,
    /// which is what makes the durability promise real rather than just
    /// "nothing crashes while it's running."
    ///
    /// On a fresh file, `Catalog::load` bootstraps the catalog page. That
    /// runs through the same stage/log/write-back path as any batch, so a
    /// crash mid-bootstrap leaves either an empty file (fresh again next
    /// time) or a WAL record that completes it — never a file with a
    /// header but no catalog page.
    ///
    /// The store is opened first: it takes the file's exclusive lock
    /// (`FileStore::open`), so a second `open` of a database in use fails
    /// before it reads anything — not even the WAL, which the running
    /// instance may be halfway through writing. To share one database
    /// within a process, clone the handle instead of opening it again.
    pub fn open(path: impl AsRef<Path>) -> crate::Result<Self> {
        Self::open_with(path, OpenOptions::default())
    }

    /// `open`, with `options`: how much of the file to keep in memory,
    /// for now (SPEC §50).
    ///
    /// ```
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let path = dir.path().join("app.trunkdb");
    /// use trunkdb::{Database, OpenOptions};
    ///
    /// let db = Database::open_with(&path, OpenOptions::default().cache_size(256 << 20))?;
    /// # Ok::<(), trunkdb::Error>(())
    /// ```
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> crate::Result<Self> {
        let path = path.as_ref();
        let mut store = FileStore::open_before_recovery(path)?;
        store.set_cache_size(options.cache_size);
        let (mut durability, pending) = WalDurability::open(path)?;
        if !pending.is_empty() {
            store.restore_pages(&pending)?;
        }
        // After recovery, which rewrites a header a crash tore (SPEC
        // §40.4); a damaged one without a batch to restore is real damage.
        store.check_header()?;
        // Always, not just after a restore: a WAL whose only content is a
        // torn tail (a crash mid-`log`) yields nothing pending, but the
        // next batch must not be appended after that garbage.
        durability.checkpoint()?;

        store.begin();
        let catalog = match Catalog::load(&mut store)
            .map_err(crate::Error::from)
            .and_then(|catalog| {
                crate::collection::key_ids_in_old_indexes(&catalog, &mut store)?;
                Ok(catalog)
            }) {
            Ok(catalog) => catalog,
            Err(e) => {
                store.rollback();
                return Err(e);
            }
        };
        let pages: Vec<(PageId, &[u8])> = store.dirty_pages().collect();
        if pages.is_empty() {
            drop(pages);
            store.rollback(); // an existing file: nothing was bootstrapped
        } else {
            // No retry or poisoning here, unlike `write_batch`: a failure
            // fails `open` itself, and the next `open` recovers from the WAL.
            durability.log(&pages)?;
            drop(pages);
            store.write_back()?;
            durability.checkpoint()?;
        }
        let current = Snapshot::new(store.last_commit(), catalog);

        Ok(Self {
            inner: Arc::new(Shared {
                pages: store.pages.clone(),
                current: RwLock::new(current),
                gate: RwLock::new(()),
                writer: Mutex::new(Writer {
                    store,
                    durability,
                    checkpoint_pages: options.checkpoint_pages,
                    checkpoint_wal_bytes: options.checkpoint_wal_bytes.unwrap_or(
                        2 * options.checkpoint_pages as u64 * crate::storage::PAGE_SIZE as u64,
                    ),
                    past: Vec::new(),
                    #[cfg(test)]
                    after_log: None,
                }),
                poisoned: AtomicBool::new(false),
                id_gen: UuidV7Generator,
                txn: GlobalLockTxnManager::default(),
            }),
        })
    }

    pub fn collection<T>(&self, name: &str) -> Collection<T> {
        Collection::new(self.clone(), name)
    }

    /// Deletes the collection `name` — its documents, its indexes, its
    /// catalog entry — and frees all of their pages for reuse, in one
    /// atomic batch (SPEC §37). `false` if there was no such collection.
    /// `Collection` handles for it stay usable: the next write creates
    /// it again, empty.
    pub fn drop_collection(&self, name: &str) -> crate::Result<bool> {
        self.transact(|catalog, store| {
            let Some(meta) = catalog.get(name).copied() else {
                return Ok(false);
            };
            let primary = BTreeIndex::new(meta.index_root);
            let locs: Vec<_> = primary
                .scan(store)?
                .into_iter()
                .map(|(_key, loc)| loc)
                .collect();
            data::free_collection_pages(store, meta.current_data_page, locs)?;
            let (_meta, indexes) = catalog
                .drop_collection(store, name)?
                .expect("it was just there");
            primary.free_all(store)?;
            for index in indexes {
                BTreeIndex::new(index.root).free_all(store)?;
            }
            Ok(true)
        })
    }

    /// Starts a typed, atomic multi-op write — see `Batch`.
    pub fn batch(&self) -> Batch {
        Batch::new(self.clone())
    }

    pub(crate) fn id_gen(&self) -> &UuidV7Generator {
        &self.inner.id_gen
    }

    /// Whether `self` and `other` are handles to the same open database.
    pub(crate) fn same_as(&self, other: &Database) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Whether the database refuses every call until it's reopened:
    /// a failed write has left a batch's fate unknown (see `transact`),
    /// or a thread panicked in the middle of a batch, which poisons the
    /// writer's lock and may have left the store staging (SPEC §27.3).
    /// The committed state a reader sees is whole even then; reads are
    /// refused all the same, as they always were.
    fn is_poisoned(&self) -> bool {
        self.inner.poisoned.load(Ordering::Relaxed) || self.inner.writer.is_poisoned()
    }

    /// Access for a read: the last commit, taken now and kept for as
    /// long as the guard lives (SPEC §80). It waits for nothing but a
    /// `compact`: commits go on beside it, and it goes on reading its own
    /// commit. `Err(Error::Poisoned)` as `is_poisoned` says. Every public
    /// entry point goes through this or `write`.
    ///
    /// What comes back is that commit and nothing else (SPEC §78): a
    /// read can't reach the store, where a batch may be staged.
    pub(crate) fn read(&self) -> crate::Result<ReadGuard<'_>> {
        if self.is_poisoned() {
            return Err(crate::Error::Poisoned);
        }
        let gate = self.inner.gate.read().map_err(|_| crate::Error::Poisoned)?;
        Ok(ReadGuard {
            pages: &self.inner.pages,
            snapshot: self.current(),
            _gate: gate,
        })
    }

    /// Access for a write batch, one at a time; poisoned as for `read`.
    /// Readers go on.
    fn write(&self) -> crate::Result<MutexGuard<'_, Writer>> {
        let writer = self
            .inner
            .writer
            .lock()
            .map_err(|_| crate::Error::Poisoned)?;
        if self.inner.poisoned.load(Ordering::Relaxed) {
            return Err(crate::Error::Poisoned);
        }
        Ok(writer)
    }

    /// Applies every op in `ops` as one atomic, durable unit. Ops may name
    /// different collections (each `WriteOp` carries its own) — that's the
    /// actual point: a multi-entity update (SPEC §4.4) needs exactly
    /// this, which a sequence of separate `Collection::insert`/`update`/
    /// `delete` calls can't give you, since each of those is its own batch.
    /// The engine under `Batch`, `upsert` and import; outside the crate,
    /// `Batch` is the way in (SPEC §60).
    pub(crate) fn write_batch(&self, ops: Vec<WriteOp>) -> crate::Result<()> {
        if ops.is_empty() {
            drop(self.write()?); // still reports a poisoned database
            return Ok(());
        }
        self.transact(|catalog, store| {
            self.inner
                .txn
                .apply_batch(ops, &mut |op| apply_write_op(catalog, store, &op))
        })
    }

    /// Runs `apply` — any change to the catalog and the pages — as one
    /// atomic, durable unit, one batch at a time. `write_batch` is one
    /// use; building or dropping an index (SPEC §28) is another.
    ///
    /// The protocol (SPEC §19.3, §79, §80), all of it under the writer's
    /// lock, with the reads going on beside it:
    /// 1. **stage** — `FileStore::begin`; every page write from here on
    ///    stays in memory, the writer's alone.
    /// 2. **apply**, to the staged pages and a catalog of the batch's
    ///    own. On error: drop both, and return the error — nothing of it
    ///    ever reached the file or a reader, so there's nothing to undo.
    /// 3. **log** every changed page to the WAL as one record, `fsync` —
    ///    the one flush a commit waits for (SPEC §51).
    /// 4. **publish**: the pages become the newest committed ones, read
    ///    from memory, and with the batch's catalog the snapshot new
    ///    reads get (SPEC §78). It takes as long as putting the pages
    ///    into a map, and waits for no read: the reads under way keep
    ///    their snapshot, and the pages as they were stay in memory for
    ///    them. The main file isn't touched.
    /// 5. **checkpoint**, once `OpenOptions::checkpoint_pages` or more are
    ///    waiting (default 1,000) or the WAL has reached
    ///    `OpenOptions::checkpoint_wal_bytes` (SPEC §76): write them back
    ///    to the main file, `fsync`, truncate the WAL.
    ///
    /// A batch that replaces the whole file (`compact`) is the exception:
    /// it waits for the reads under way before 3, lets none begin until
    /// after 4, and fails with `Error::SnapshotOpen` if a snapshot is
    /// kept open beyond a read (SPEC §80.5).
    ///
    /// A crash before 3 completes leaves the state before; a crash after
    /// it leaves complete WAL records that `open` writes back, giving the
    /// state after. Never anything in between.
    pub(crate) fn transact<R>(
        &self,
        apply: impl FnOnce(&mut Catalog, &mut FileStore) -> crate::Result<R>,
    ) -> crate::Result<R> {
        let mut guard = self.write()?;
        // Reborrow the guard as a plain `&mut Writer` once, so the borrow
        // checker can see that `store` and `durability` below are
        // separate fields, each borrowable on its own.
        let writer = &mut *guard;
        // The batch's own catalog, as the staged pages are its own: the
        // snapshot's becomes this one at commit, and stays as it is if
        // the batch fails (SPEC §19.5, §78). No commit can come between
        // this and ours: we hold the writer's lock.
        let mut catalog = self.current().catalog().clone();

        writer.store.begin();
        let result = match apply(&mut catalog, &mut writer.store) {
            Ok(result) => result,
            Err(e) => {
                writer.store.rollback();
                return Err(e);
            }
        };

        let pages: Vec<(PageId, &[u8])> = writer.store.dirty_pages().collect();
        if pages.is_empty() {
            // Nothing changed (e.g. an index that already existed):
            // nothing to log or write, just end staging.
            drop(pages);
            writer.store.rollback();
            return Ok(result);
        }
        drop(pages);

        // Alone, for a batch that replaces the whole file: the reads
        // under way are waited for, and none begins before the commit.
        // Before the log, so a batch refused here was never durable.
        let alone = if writer.store.replaces_all() {
            let gate = self
                .inner
                .gate
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // No read is under way now. A snapshot may still be open:
            // one that somebody keeps.
            let kept = Arc::strong_count(&self.current_locked().0) > 1;
            if kept || !writer.live().is_empty() {
                writer.store.rollback();
                return Err(crate::Error::SnapshotOpen);
            }
            Some(gate)
        } else {
            None
        };

        let pages: Vec<(PageId, &[u8])> = writer.store.dirty_pages().collect();
        let logged = writer.durability.log(&pages);
        drop(pages);
        if let Err(e) = logged {
            writer.store.rollback();
            // A failed `log` may still have left a complete record behind
            // (say the write landed but the fsync failed), which the next
            // `open` would restore — for a batch this call reports as
            // failed. Truncating the log rules that out; if even that
            // fails, the batch's fate is genuinely unknown.
            if writer.durability.checkpoint().is_err() {
                self.inner.poisoned.store(true, Ordering::Relaxed);
            }
            return Err(e.into());
        }

        // The batch is durable from here on: the WAL holds all its pages.
        #[cfg(test)]
        if let Some(hook) = writer.after_log.take() {
            hook();
        }
        // Before the lock, what needs none: the pages' layout checked.
        writer.store.check_staged();
        let before = {
            // New reads wait here, for as long as it takes to put the
            // pages in. A panic while it was held was a panic here, which
            // poisoned the writer's lock too: nobody gets this far.
            let mut current = self.current_locked();
            // The snapshot about to be replaced stays open if a read
            // holds it: no read can take it once it's replaced, so who
            // holds it now is everyone who ever will.
            if Arc::strong_count(&current.0) > 1 {
                writer.past.push(Arc::downgrade(&current.0));
            }
            let live = writer.live();
            writer.store.commit_beside(&live);
            let snapshot = Snapshot::new(writer.store.last_commit(), catalog);
            std::mem::replace(&mut *current, snapshot)
        };
        drop(alone);
        // The catalog before, if no read holds it, freed outside the lock.
        drop(before);

        if writer.store.unwritten_pages() >= writer.checkpoint_pages
            || writer.durability.len() >= writer.checkpoint_wal_bytes
        {
            // Not an error for this batch if it fails: it's durable, and
            // reads find its pages in memory. The next one tries again.
            let _ = writer.checkpoint();
        }
        Ok(result)
    }

    /// The last commit, shared: the lock is held only to take it.
    fn current(&self) -> Snapshot {
        self.inner
            .current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The last commit, to replace, with no read taking it meanwhile.
    fn current_locked(&self) -> std::sync::RwLockWriteGuard<'_, Snapshot> {
        self.inner
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes every committed page back to the main file and empties the
    /// WAL (SPEC §51) — which a commit does by itself once enough pages
    /// wait, and dropping the last handle does too. For a file that is
    /// complete on its own: before copying it, say.
    pub fn checkpoint(&self) -> crate::Result<()> {
        Ok(self.write()?.checkpoint()?)
    }

    /// Direct access to the writer for tests, bypassing the poisoned
    /// checks — e.g. to inject write-back faults, or inspect the catalog.
    /// With the last commit as of this call.
    #[cfg(test)]
    pub(crate) fn state(&self) -> TestState<'_> {
        TestState {
            writer: self.inner.writer.lock().unwrap(),
            current: self.current(),
            pages: &self.inner.pages,
        }
    }
}

/// `Database::state`: the writer, held, and the last commit.
#[cfg(test)]
pub(crate) struct TestState<'a> {
    writer: MutexGuard<'a, Writer>,
    current: Snapshot,
    pages: &'a Pages,
}

#[cfg(test)]
impl TestState<'_> {
    /// The last commit, to read: its catalog and its pages. A batch being
    /// staged is not in it.
    pub(crate) fn snapshot(&self) -> Reading<'_> {
        self.current.reading(self.pages)
    }

    /// The catalog as of the last commit.
    pub(crate) fn catalog(&self) -> &Catalog {
        self.current.catalog()
    }
}

#[cfg(test)]
impl std::ops::Deref for TestState<'_> {
    type Target = Writer;

    fn deref(&self) -> &Writer {
        &self.writer
    }
}

#[cfg(test)]
impl std::ops::DerefMut for TestState<'_> {
    fn deref_mut(&mut self) -> &mut Writer {
        &mut self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocId, Document};
    use crate::query::Filter;
    use crate::storage::{PAGE_SIZE, PageType};
    use crate::txn::WriteOp;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct Dummy;

    // --- Dropping a collection (SPEC §37) ---

    /// Fills `name` with the same documents every time — same ids, same
    /// sizes — so two collections filled this way take the same pages:
    /// 300 documents of very different sizes (so a few pages empty out
    /// when some are deleted, the current one among them), one in
    /// overflow pages, and a plain and a unique index.
    fn fill(db: &Database, name: &str) {
        let docs = db.collection::<Document>(name);
        docs.ensure_index("n").unwrap();
        docs.ensure_index_with("u", crate::IndexOptions::new().unique())
            .unwrap();
        let doc = |i: usize| {
            let pad = if i == 7 { 100_000 } else { i * 37 % 3000 };
            let fields = [
                ("n", Document::Int((i % 7) as i64)),
                ("u", Document::Int(i as i64)),
                ("pad", Document::String("p".repeat(pad))),
            ];
            Document::Object(
                fields
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            )
        };
        let id = |i: usize| DocId((i as u128).to_be_bytes());
        let ops = (0..300).map(|i| WriteOp::Insert(name.into(), id(i), doc(i)));
        db.write_batch(ops.collect()).unwrap();
        // The last 20 too: that empties the current data page, which
        // stays (inserts go there) though no document points to it.
        let deletes = (0..300).filter(|i| i % 5 == 0 || (40..80).contains(i) || *i >= 280);
        db.write_batch(
            deletes
                .map(|i| WriteOp::Delete(name.into(), id(i)))
                .collect(),
        )
        .unwrap();
    }

    fn contents(db: &Database, name: &str) -> Vec<(DocId, Document)> {
        let mut all = db
            .collection::<Document>(name)
            .find_with_ids(Filter::new())
            .unwrap();
        all.sort_by_key(|(id, _)| *id);
        all
    }

    /// The cache's size is an option; any size, none included, gives
    /// the same data (SPEC §50).
    #[test]
    fn open_with_any_cache_size_reads_the_same() {
        assert_eq!(OpenOptions::default().cache_size, 256 << 20);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let ids: Vec<_> = {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<crate::Document>("docs");
            use crate::id::IdGenerator;
            let ids: Vec<_> = (0..500).map(|_| db.id_gen().generate()).collect();
            let ops = ids
                .iter()
                .enumerate()
                .map(|(i, id)| WriteOp::Insert("docs".into(), *id, crate::Document::Int(i as i64)))
                .collect();
            db.write_batch(ops).unwrap();
            drop(docs);
            ids
        };
        for size in [0, 8192 * 3, 1 << 20] {
            let options = OpenOptions::default().cache_size(size);
            let db = Database::open_with(&path, options).unwrap();
            let docs = db.collection::<crate::Document>("docs");
            for _ in 0..2 {
                for (i, id) in ids.iter().enumerate() {
                    assert_eq!(docs.get(id).unwrap(), Some(crate::Document::Int(i as i64)));
                }
            }
            assert_eq!(db.state().store.cache_size(), size / 8192);
        }
    }

    #[test]
    fn dropping_a_collection_frees_every_page_it_had() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let file_len = || std::fs::metadata(&path).unwrap().len();
        let (keep1, keep2) = {
            let db = Database::open(&path).unwrap();
            // Interleaved in the file: the kept ones' pages sit among the
            // dropped one's.
            fill(&db, "keep1");
            fill(&db, "gone");
            fill(&db, "keep2");
            let kept = (contents(&db, "keep1"), contents(&db, "keep2"));
            let len = file_len();

            assert!(db.drop_collection("gone").unwrap());
            assert!(!db.drop_collection("gone").unwrap());
            assert_eq!(db.collections().unwrap(), ["keep1", "keep2"]);
            assert!(contents(&db, "gone").is_empty());

            // Everything it had comes back: the same collection again
            // takes no new page.
            fill(&db, "again");
            assert_eq!(file_len(), len);
            assert!(db.check().unwrap().is_ok(), "{:#?}", db.check().unwrap());
            assert_eq!(contents(&db, "again").len(), 192);
            assert_eq!((contents(&db, "keep1"), contents(&db, "keep2")), kept);
            kept
        };

        let db = Database::open(&path).unwrap();
        assert_eq!(db.collections().unwrap(), ["again", "keep1", "keep2"]);
        assert_eq!(
            (contents(&db, "keep1"), contents(&db, "keep2")),
            (keep1, keep2)
        );
        let again = db.collection::<Document>("again");
        assert_eq!(again.index_names().unwrap(), ["n", "u"]);
        assert_eq!(again.unique_indexes().unwrap(), ["u"]);
        let n_is_3 = Filter::new().eq("n", 3);
        assert_eq!(again.find(n_is_3).unwrap().len(), 28);

        assert!(db.check().unwrap().is_ok(), "{:#?}", db.check().unwrap());
        // A handle to a dropped collection still works: it starts over.
        let gone = db.collection::<Document>("gone");
        assert_eq!(gone.count(Filter::new()).unwrap(), 0);
        gone.insert(Document::Int(1)).unwrap();
        assert_eq!(gone.count(Filter::new()).unwrap(), 1);
        assert!(gone.index_names().unwrap().is_empty());
    }

    fn insert(collection: &str, id: u8, value: i64) -> WriteOp {
        WriteOp::Insert(
            collection.to_string(),
            DocId([id; 16]),
            Document::Int(value),
        )
    }

    fn get(db: &Database, collection: &str, id: u8) -> Option<Document> {
        db.collection::<Document>(collection)
            .get(&DocId([id; 16]))
            .unwrap()
    }

    /// Where `log_batch_then_crash` stops, mirroring `write_batch`'s steps.
    enum CrashPoint {
        /// Ops applied to staged pages, nothing logged yet.
        DuringApply,
        /// Logged, but no page written to the main file yet.
        AfterLog,
        /// Logged, and the write-back got this many pages (ascending id
        /// order) into the main file.
        MidWriteBack { pages_written: usize },
        /// Logged and fully written back, but never checkpointed.
        BeforeCheckpoint,
    }

    /// Runs `ops` through `write_batch`'s protocol by hand, against its
    /// own `FileStore`/`Catalog`/WAL on `path`, and stops at `crash` — as
    /// if the process died there. `path` must already be a database.
    /// Returns how many pages the batch changed.
    fn log_batch_then_crash(path: &Path, ops: &[WriteOp], crash: CrashPoint) -> usize {
        let (mut wal, pending) = WalDurability::open(path).unwrap();
        assert!(pending.is_empty());
        let mut store = FileStore::open(path).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();

        store.begin();
        for op in ops {
            apply_write_op(&mut catalog, &mut store, op).unwrap();
        }
        let pages: Vec<(PageId, &[u8])> = store.dirty_pages().collect();
        let page_count = pages.len();
        if matches!(crash, CrashPoint::DuringApply) {
            return page_count;
        }
        wal.log(&pages).unwrap();
        drop(pages);

        match crash {
            CrashPoint::DuringApply | CrashPoint::AfterLog => {}
            CrashPoint::MidWriteBack { pages_written } => {
                assert!(pages_written < page_count, "that's not mid-write-back");
                store.pages.fail_write_backs(1, pages_written);
                store.write_back().unwrap_err();
            }
            CrashPoint::BeforeCheckpoint => store.write_back().unwrap(),
        }
        page_count
    }

    fn two_collection_batch() -> Vec<WriteOp> {
        vec![insert("posts", 1, 10), insert("authors", 2, 20)]
    }

    fn assert_two_collection_batch_present(db: &Database) {
        assert_eq!(get(db, "posts", 1), Some(Document::Int(10)));
        assert_eq!(get(db, "authors", 2), Some(Document::Int(20)));
    }

    #[test]
    fn two_collections_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();

        let users = db.collection::<Dummy>("users");
        let posts = db.collection::<Dummy>("posts");

        // The point of this test: it wouldn't compile if `collection()`
        // required `&mut self` instead of `&self`.
        let _ = (&users, &posts);
    }

    /// A second `open` of a database in use fails, and leaves the first
    /// instance's WAL alone — here a logged, not yet checkpointed record,
    /// as if the first instance were between `log` and `checkpoint`.
    #[test]
    fn a_database_in_use_cannot_be_opened_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        let catalog_page = crate::storage::PageStore::read_page(&db.state().store, 1).unwrap();
        db.state().durability.log(&[(1, &catalog_page)]).unwrap();
        let wal_path = dir.path().join("test.trunkdb.wal");
        let wal_len = std::fs::metadata(&wal_path).unwrap().len();

        let Err(crate::Error::Io(err)) = Database::open(&path) else {
            panic!("a second open must fail with an I/O error");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), wal_len);

        drop(db);
        Database::open(&path).unwrap();
    }

    /// Pointing `open` at someone else's small file must neither
    /// overwrite it nor leave a WAL file behind.
    #[test]
    fn opening_a_foreign_file_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"my notes").unwrap();

        assert!(Database::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"my notes");
        assert!(!dir.path().join("notes.txt.wal").exists());
    }

    #[test]
    fn empty_batch_is_a_harmless_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        db.write_batch(Vec::new()).unwrap();
    }

    /// An op that can't apply as asked — inserting an id that exists,
    /// updating or deleting one that doesn't — fails the whole batch
    /// with a matchable error, instead of being silently skipped while
    /// the batch reports success.
    #[test]
    fn ops_on_duplicate_or_missing_ids_fail_the_whole_batch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        db.write_batch(vec![insert("users", 1, 1)]).unwrap();
        let missing = DocId([9; 16]);

        let cases = [
            (insert("users", 1, 2), "duplicate insert"),
            (
                WriteOp::Update("users".to_string(), missing, Document::Int(3)),
                "update of a missing id",
            ),
            (
                WriteOp::Delete("users".to_string(), missing),
                "delete of a missing id",
            ),
            (
                WriteOp::Update("nowhere".to_string(), missing, Document::Int(3)),
                "update in a missing collection",
            ),
        ];
        for (op, case) in cases {
            // A valid op first, so the test also proves it's rolled back.
            let result = db.write_batch(vec![insert("posts", 5, 5), op]);
            match (case, result) {
                ("duplicate insert", Err(crate::Error::DuplicateId { collection, id })) => {
                    assert_eq!((collection.as_str(), id), ("users", DocId([1; 16])));
                }
                (_, Err(crate::Error::NotFound { id, .. })) if case != "duplicate insert" => {
                    assert_eq!(id, missing);
                }
                (_, other) => panic!("{case}: unexpected {other:?}"),
            }
            assert_eq!(get(&db, "posts", 5), None, "{case}: batch not rolled back");
        }
        assert_eq!(get(&db, "users", 1), Some(Document::Int(1)));
    }

    /// The actual point of a batch: ops can target different collections
    /// and still land as one atomic, durable unit.
    #[test]
    fn write_batch_applies_ops_across_different_collections() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();

        db.write_batch(two_collection_batch()).unwrap();
        assert_two_collection_batch_present(&db);
    }

    #[test]
    fn recovers_a_batch_that_was_logged_but_never_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        drop(Database::open(&path).unwrap());

        log_batch_then_crash(&path, &two_collection_batch(), CrashPoint::AfterLog);

        assert_two_collection_batch_present(&Database::open(&path).unwrap());
    }

    /// The case the op-level WAL couldn't handle (SPEC §19.1): some of the
    /// batch's pages are in the main file, others aren't — a structurally
    /// half-written file. Writing back every logged page repairs it.
    #[test]
    fn recovers_a_batch_whose_write_back_was_cut_short() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        drop(Database::open(&path).unwrap());

        let pages = log_batch_then_crash(
            &path,
            &two_collection_batch(),
            CrashPoint::MidWriteBack { pages_written: 1 },
        );
        assert!(pages > 1);

        assert_two_collection_batch_present(&Database::open(&path).unwrap());
    }

    /// A crash mid-write of the header page can leave its first bytes new
    /// and its last ones old: fields and checksum disagree. The batch is
    /// in the WAL, so that's recovered, not reported as damage (SPEC
    /// §40.4). The same mismatch with nothing to recover is damage.
    #[test]
    fn a_header_torn_by_a_crash_is_recovered_from_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        drop(Database::open(&path).unwrap());
        let before = std::fs::read(&path).unwrap();

        log_batch_then_crash(
            &path,
            &two_collection_batch(),
            CrashPoint::MidWriteBack { pages_written: 1 },
        );
        let last_sector = PAGE_SIZE - 512..PAGE_SIZE;
        let mut bytes = std::fs::read(&path).unwrap();
        assert_ne!(bytes[last_sector.clone()], before[last_sector.clone()]);
        bytes[last_sector.clone()].copy_from_slice(&before[last_sector]);
        std::fs::write(&path, &bytes).unwrap();

        let db = Database::open(&path).unwrap();
        assert_two_collection_batch_present(&db);
        assert!(db.check().unwrap().is_ok());
        drop(db);

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[PAGE_SIZE - 100] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let err = Database::open(&path).err().expect("damage, not a crash");
        assert!(err.to_string().contains("page 0 is damaged"), "{err}");
    }

    /// Writing back pages that are already in the main file changes
    /// nothing — no duplicate documents, no error.
    #[test]
    fn recovering_an_already_written_back_batch_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        drop(Database::open(&path).unwrap());

        log_batch_then_crash(&path, &two_collection_batch(), CrashPoint::BeforeCheckpoint);

        let db = Database::open(&path).unwrap();
        assert_two_collection_batch_present(&db);
        assert_eq!(
            db.collection::<Document>("posts")
                .find(Filter::default())
                .unwrap()
                .len(),
            1
        );
    }

    /// A crash during `log` that got only part of a batch onto disk must
    /// leave the database showing none of it after recovery.
    #[test]
    fn a_batch_torn_while_being_logged_is_not_recovered_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        drop(Database::open(&path).unwrap());

        log_batch_then_crash(&path, &two_collection_batch(), CrashPoint::AfterLog);
        let wal_path = dir.path().join("test.trunkdb.wal");
        let len = std::fs::metadata(&wal_path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&wal_path)
            .unwrap()
            .set_len(len - 3)
            .unwrap();

        let db = Database::open(&path).unwrap();
        assert_eq!(get(&db, "posts", 1), None);
        assert_eq!(get(&db, "authors", 2), None);
    }

    /// `open` must clear a torn tail even when nothing was recovered from
    /// it — otherwise the next batch would be appended after the garbage,
    /// and the open after that would find a bad checksum mid-file.
    #[test]
    fn a_torn_wal_tail_does_not_break_later_batches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        drop(Database::open(&path).unwrap());

        log_batch_then_crash(&path, &two_collection_batch(), CrashPoint::AfterLog);
        let wal_path = dir.path().join("test.trunkdb.wal");
        let len = std::fs::metadata(&wal_path).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&wal_path)
            .unwrap()
            .set_len(len - 3)
            .unwrap();

        {
            let db = Database::open(&path).unwrap();
            db.write_batch(vec![insert("users", 3, 30)]).unwrap();
        }
        let db = Database::open(&path).unwrap();
        assert_eq!(get(&db, "users", 3), Some(Document::Int(30)));
    }

    /// Real rollback (formerly SPEC §17.3's known gap): op 1 succeeds and
    /// even creates a new collection, op 2 writes a document large enough
    /// for overflow pages; op 3 is a duplicate id. Nothing of the batch
    /// may remain — not the documents, not the collection op 1 created
    /// (neither in the file nor in the catalog cache), and not the pages
    /// they took.
    #[test]
    fn a_failed_batch_leaves_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        db.write_batch(vec![insert("users", 1, 1)]).unwrap();
        let file_len = std::fs::metadata(&path).unwrap().len();

        let large = Document::Binary(vec![0u8; 20_000]); // needs overflow pages
        let result = db.write_batch(vec![
            insert("posts", 2, 2),
            WriteOp::Insert("posts".to_string(), DocId([3; 16]), large),
            insert("users", 1, 5),
        ]);

        assert!(matches!(result, Err(crate::Error::DuplicateId { .. })));
        assert_eq!(get(&db, "posts", 3), None);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), file_len);
        assert!(result.is_err());
        assert_eq!(get(&db, "posts", 2), None);
        assert!(db.state().catalog().get("posts").is_none());

        // The database keeps working, and the next batch that does create
        // "posts" gets a consistent collection, also after a reopen.
        db.write_batch(vec![insert("posts", 4, 4)]).unwrap();
        drop(db);
        let db = Database::open(&path).unwrap();
        assert_eq!(get(&db, "users", 1), Some(Document::Int(1)));
        assert_eq!(get(&db, "posts", 4), Some(Document::Int(4)));
        assert_eq!(
            db.collection::<Document>("posts")
                .find(Filter::default())
                .unwrap()
                .len(),
            1
        );
    }

    fn wal_len(path: &Path) -> u64 {
        let mut wal = path.as_os_str().to_os_string();
        wal.push(".wal");
        std::fs::metadata(wal).unwrap().len()
    }

    /// A commit only logs (SPEC §51): the batch is in the WAL, not the
    /// main file, until a checkpoint — and readable all along.
    #[test]
    fn a_commit_logs_and_a_checkpoint_writes_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        // Through the store's handle: Windows won't let a second one read
        // a locked file.
        let file = || db.state().store.file_bytes();
        let empty = file();

        db.write_batch(two_collection_batch()).unwrap();
        assert_two_collection_batch_present(&db);
        assert_eq!(file(), empty, "the main file waits");
        assert!(wal_len(&path) > 0);

        db.checkpoint().unwrap();
        assert_ne!(file(), empty);
        assert_eq!(wal_len(&path), 0);
        assert_two_collection_batch_present(&db);
    }

    /// How many committed pages wait for a checkpoint after one small
    /// commit, in a database opened with `options`.
    fn pages_waiting_after_one_commit(options: OpenOptions) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(dir.path().join("test.trunkdb"), options).unwrap();
        let doc = Document::String("x".into());
        db.write_batch(vec![WriteOp::Insert("docs".into(), DocId([1; 16]), doc)])
            .unwrap();
        db.state().store.unwritten_pages()
    }

    /// `OpenOptions::checkpoint_pages` decides when a commit writes its
    /// pages back: at 0 or 1 right away, at the default only once 1,000
    /// wait — which one small commit doesn't reach — and exactly when
    /// as many as it says are waiting.
    #[test]
    fn checkpoint_pages_sets_when_a_commit_writes_back() {
        let options = OpenOptions::default();
        assert_eq!(
            pages_waiting_after_one_commit(options.checkpoint_pages(0)),
            0
        );
        assert_eq!(
            pages_waiting_after_one_commit(options.checkpoint_pages(1)),
            0
        );
        let waiting = pages_waiting_after_one_commit(options);
        assert!(waiting > 1);

        let just_enough = options.checkpoint_pages(waiting);
        assert_eq!(pages_waiting_after_one_commit(just_enough), 0);
        let one_short = options.checkpoint_pages(waiting + 1);
        assert_eq!(pages_waiting_after_one_commit(one_short), waiting);
    }

    /// Commits that keep changing the same few pages leave few distinct
    /// pages waiting but a WAL that grows with every commit. Returns the
    /// WAL's longest length over 200 such commits, and how many pages
    /// waited at the end.
    fn hot_page_wal_peak(options: OpenOptions) -> (u64, usize) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open_with(&path, options).unwrap();
        let first = Document::String("first".into());
        db.write_batch(vec![WriteOp::Insert("docs".into(), DocId([1; 16]), first)])
            .unwrap();
        let mut peak = 0;
        for i in 0..200u32 {
            let doc = Document::String(format!("value {i}"));
            db.write_batch(vec![WriteOp::Update("docs".into(), DocId([1; 16]), doc)])
                .unwrap();
            peak = peak.max(wal_len(&path));
        }
        let waiting = db.state().store.unwritten_pages();
        (peak, waiting)
    }

    /// `OpenOptions::checkpoint_wal_bytes` bounds the WAL when the page
    /// threshold never fires (SPEC §76).
    #[test]
    fn checkpoint_wal_bytes_bounds_the_wal_when_few_pages_change() {
        let many_pages = OpenOptions::default().checkpoint_pages(1_000_000);
        // Without the guard, 200 commits of the same page: 200 images.
        let (unbounded, waiting) = hot_page_wal_peak(many_pages.checkpoint_wal_bytes(u64::MAX));
        assert!(unbounded > 200 * PAGE_SIZE as u64, "{unbounded}");
        assert!(waiting > 0);

        // A limit of 20 page images: the commit that reaches it empties
        // the WAL, so it is seen only just under the limit.
        let limit = 20 * PAGE_SIZE as u64;
        let (peak, _) = hot_page_wal_peak(many_pages.checkpoint_wal_bytes(limit));
        assert!(peak < limit, "{peak}");
        assert!(peak > limit / 2, "{peak}");
    }

    /// Unset, the limit is twice the bytes of `checkpoint_pages` pages: at
    /// 5 pages, 10 page images, so 200 commits of one page checkpoint.
    #[test]
    fn checkpoint_wal_bytes_defaults_to_twice_the_page_threshold() {
        let options = OpenOptions::default().checkpoint_pages(5);
        let (peak, _) = hot_page_wal_peak(options);
        assert!(peak < 40 * PAGE_SIZE as u64, "{peak}");
        let explicit = options.checkpoint_wal_bytes(u64::MAX);
        assert!(hot_page_wal_peak(explicit).0 > 200 * PAGE_SIZE as u64);
    }

    /// Enough waiting pages, and the commit writes them back itself.
    #[test]
    fn a_commit_checkpoints_once_enough_pages_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        let big = Document::String("x".repeat(6000));
        let checkpoint_pages = DEFAULT_CHECKPOINT_PAGES;
        let mut waiting = 0;
        for commit in 0..100u8 {
            let ops = (0..50u8)
                .map(|i| {
                    let id = DocId([commit, i, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
                    WriteOp::Insert("big".into(), id, big.clone())
                })
                .collect();
            db.write_batch(ops).unwrap();
            let now = db.state().store.unwritten_pages();
            if now < waiting {
                // This commit's pages brought it over the line.
                assert_eq!(now, 0);
                assert!(waiting < checkpoint_pages, "{waiting}");
                assert!(commit > 5, "{commit}");
                assert_eq!(wal_len(&path), 0);
                return;
            }
            assert!(wal_len(&path) > 0);
            waiting = now;
        }
        panic!("no checkpoint after {waiting} pages");
    }

    /// Dropping the last handle writes back what's committed: the main
    /// file is complete without its WAL.
    #[test]
    fn dropping_the_last_handle_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        let other = db.clone();
        db.write_batch(two_collection_batch()).unwrap();
        drop(db);
        assert!(wal_len(&path) > 0, "a handle is left");
        drop(other);
        assert_eq!(wal_len(&path), 0);
        std::fs::remove_file(dir.path().join("test.trunkdb.wal")).unwrap();
        assert_two_collection_batch_present(&Database::open(&path).unwrap());
    }

    /// A checkpoint that fails leaves the batch committed and readable;
    /// the next one writes it back.
    #[test]
    fn a_failed_checkpoint_is_retried_by_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();

        db.write_batch(two_collection_batch()).unwrap();
        db.state().store.pages.fail_write_backs(1, 1);
        assert!(db.checkpoint().is_err());
        assert_two_collection_batch_present(&db);
        assert!(wal_len(&path) > 0, "the WAL still holds the batch");
        db.checkpoint().unwrap();
        assert_eq!(wal_len(&path), 0);
        assert_two_collection_batch_present(&db);
    }

    /// Several committed batches wait, each rewriting the same documents;
    /// the checkpoint writing them back is cut short after any number of
    /// pages, and so is the one at drop — as if the process died there.
    /// The next open restores all of them in order: the last batch's
    /// values everywhere (SPEC §51).
    #[test]
    fn batches_cut_short_mid_checkpoint_are_recovered_in_order() {
        // A page each: the batches span dozens of pages.
        let big =
            |round: i64, id: u8| Document::String(format!("{round}-{id}-{}", "x".repeat(5000)));
        for fails_after in [0, 1, 3, 7, 20] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("test.trunkdb");
            let db = Database::open(&path).unwrap();
            for round in 0..3i64 {
                let ops = (1..=30u8)
                    .map(|id| {
                        let (id, value) = (DocId([id; 16]), big(round, id));
                        match round {
                            0 => WriteOp::Insert("docs".into(), id, value),
                            _ => WriteOp::Update("docs".into(), id, value),
                        }
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            {
                let state = db.state();
                assert!(state.store.unwritten_pages() > fails_after);
                state.store.pages.fail_write_backs(2, fails_after);
            }
            assert!(db.checkpoint().is_err());
            drop(db);

            let db = Database::open(&path).unwrap();
            for id in 1..=30u8 {
                assert_eq!(
                    get(&db, "docs", id),
                    Some(big(2, id)),
                    "{fails_after}: {id}"
                );
            }
            assert!(db.check().unwrap().is_ok(), "{fails_after}");
        }
    }

    /// Checkpoints that keep failing, the one at drop included, lose
    /// nothing: the next open writes the batch back from the WAL.
    #[test]
    fn checkpoints_that_keep_failing_leave_it_to_the_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();

        db.write_batch(two_collection_batch()).unwrap();
        db.state().store.pages.fail_write_backs(2, 1);
        assert!(db.checkpoint().is_err());
        assert_two_collection_batch_present(&db);
        drop(db);
        assert!(wal_len(&path) > 0);

        assert_two_collection_batch_present(&Database::open(&path).unwrap());
    }

    #[test]
    fn a_crash_during_apply_leaves_the_pre_batch_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        drop(Database::open(&path).unwrap());

        log_batch_then_crash(&path, &two_collection_batch(), CrashPoint::DuringApply);

        let db = Database::open(&path).unwrap();
        assert_eq!(get(&db, "posts", 1), None);
        assert!(db.state().catalog().get("posts").is_none());
    }

    /// A fresh file's first write is the catalog bootstrap. A crash at any
    /// point of it must leave a file that opens cleanly — either fresh
    /// again, or completed from the WAL — never "header but no catalog".
    #[test]
    fn a_crash_at_any_point_of_the_catalog_bootstrap_leaves_an_openable_file() {
        let dir = tempfile::tempdir().unwrap();

        // Crash before anything was logged: the file stays empty.
        let path = dir.path().join("unlogged.trunkdb");
        {
            let mut store = FileStore::open(&path).unwrap();
            store.begin();
            Catalog::load(&mut store).unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        Database::open(&path).unwrap();

        // Crash after the log, with 0..all of its pages written back.
        let page_count = 2; // header + catalog page
        for pages_written in 0..=page_count {
            let path = dir.path().join(format!("cut{pages_written}.trunkdb"));
            {
                let (mut wal, _) = WalDurability::open(&path).unwrap();
                let mut store = FileStore::open(&path).unwrap();
                store.begin();
                Catalog::load(&mut store).unwrap();
                let pages: Vec<(PageId, &[u8])> = store.dirty_pages().collect();
                assert_eq!(pages.len(), page_count);
                wal.log(&pages).unwrap();
                drop(pages);
                if pages_written < page_count {
                    store.pages.fail_write_backs(1, pages_written);
                    store.write_back().unwrap_err();
                } else {
                    store.write_back().unwrap();
                }
            }

            let db = Database::open(&path).unwrap();
            db.write_batch(vec![insert("users", 1, 1)]).unwrap();
            drop(db);
            assert_eq!(
                get(&Database::open(&path).unwrap(), "users", 1),
                Some(Document::Int(1)),
                "cut after {pages_written} pages"
            );
        }
    }

    /// The regression test for the bug that motivated the page-image WAL
    /// (SPEC §19.1, §19.8): with the op-level WAL, a crash between a B-tree
    /// split's page writes permanently lost committed entries — replaying
    /// the op couldn't repair the broken structure. Here the splitting
    /// insert is cut off after every possible number of written pages;
    /// every committed document must survive every cut, and at least one
    /// cut must actually damage the tree when recovery is skipped (proof
    /// the test reproduces the original failure, not just a no-op).
    #[test]
    fn a_crash_mid_b_tree_split_loses_nothing() {
        use crate::index::{BTreeIndex, Index};

        // Ascending ids, so every insert lands in the rightmost leaf.
        fn key(i: u32) -> DocId {
            let mut bytes = [0u8; 16];
            bytes[12..].copy_from_slice(&i.to_be_bytes());
            DocId(bytes)
        }
        fn op(i: u32) -> WriteOp {
            WriteOp::Insert("pings".to_string(), key(i), Document::Int(i as i64))
        }

        /// Stages inserts from `from` on, one by one, until one splits a
        /// leaf — recognizable as a new `IndexLeaf` page in the dirty set
        /// (ascending ids only ever touch the rightmost leaf otherwise).
        /// Rolls everything back — the pages and, like `write_batch`, the
        /// catalog cache (the inserts may have moved `current_data_page`)
        /// — and returns that insert's `i`.
        fn first_splitting_insert(db: &Database, from: u32) -> u32 {
            let mut state = db.state();
            // Destructuring borrows both fields mutably at once, which
            // two separate `state.catalog()`/`state.store` borrows through
            // the guard couldn't.
            let mut catalog = state.catalog().clone();
            let (catalog, store) = (&mut catalog, &mut state.store);
            store.begin();
            let mut leaves = None;
            for i in from..from + 1000 {
                apply_write_op(catalog, store, &op(i)).unwrap();
                let now = store
                    .dirty_pages()
                    .filter(|(_id, page)| page[0] == PageType::IndexLeaf as u8)
                    .count();
                if leaves.is_some_and(|before| now > before) {
                    store.rollback();
                    return i;
                }
                leaves = Some(now);
            }
            panic!("no split within 1000 inserts");
        }

        /// Whether every one of `count` entries is reachable in the
        /// `pings` index, reading the file raw — no WAL recovery.
        fn intact_without_recovery(path: &Path, count: u32) -> bool {
            let check = || -> crate::Result<bool> {
                let mut store = FileStore::open(path)?;
                let catalog = Catalog::load(&mut store)?;
                let Some(meta) = catalog.get("pings") else {
                    return Ok(false);
                };
                let index = BTreeIndex::new(meta.index_root);
                if index.scan(&store)?.len() != count as usize {
                    return Ok(false);
                }
                for i in 0..count {
                    if index.lookup(&store, &key(i).0)?.is_none() {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
            check().unwrap_or(false)
        }

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.trunkdb");
        let committed = {
            let db = Database::open(&base).unwrap();
            db.write_batch((0..1000).map(op).collect()).unwrap();
            let split_at = first_splitting_insert(&db, 1000);
            db.write_batch((1000..split_at).map(op).collect()).unwrap();
            split_at // documents 0..split_at are committed
        };
        let splitting = op(committed);

        // How many pages the splitting insert changes — more than the
        // two of a plain insert (data page, leaf), or three when the data
        // page is new (plus the header).
        let probe = dir.path().join("probe.trunkdb");
        std::fs::copy(&base, &probe).unwrap();
        let page_count = log_batch_then_crash(
            &probe,
            std::slice::from_ref(&splitting),
            CrashPoint::DuringApply,
        );
        assert!(page_count > 3, "expected a split, got {page_count} pages");

        let mut damaged_cuts = 0;
        for pages_written in 1..page_count {
            let path = dir.path().join(format!("cut{pages_written}.trunkdb"));
            std::fs::copy(&base, &path).unwrap();
            log_batch_then_crash(
                &path,
                std::slice::from_ref(&splitting),
                CrashPoint::MidWriteBack { pages_written },
            );
            if !intact_without_recovery(&path, committed) {
                damaged_cuts += 1;
            }

            let db = Database::open(&path).unwrap();
            let pings = db.collection::<Document>("pings");
            for i in 0..=committed {
                assert_eq!(
                    pings.get(&key(i)).unwrap(),
                    Some(Document::Int(i as i64)),
                    "document {i} after a cut at {pages_written} of {page_count} pages"
                );
            }
            assert_eq!(
                pings.find(Filter::default()).unwrap().len(),
                committed as usize + 1
            );
        }
        assert!(
            damaged_cuts > 0,
            "no cut damaged the tree without recovery — the test doesn't reproduce the bug"
        );
    }

    // --- The thread-safe handle (SPEC §27) ---

    fn open_temp() -> (tempfile::TempDir, std::path::PathBuf, Database) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        (dir, path, db)
    }

    /// Compile-time: if any of these weren't `Send + Sync + 'static`, this
    /// test wouldn't build. `Rc` is neither `Send` nor `Sync`, yet a
    /// collection *of* it still is — the handle holds no `Rc`.
    #[test]
    fn handles_are_send_sync_and_static() {
        fn thread_safe<T: Send + Sync + 'static>() {}
        thread_safe::<Database>();
        thread_safe::<Collection<Document>>();
        thread_safe::<Collection<std::rc::Rc<i64>>>();
        thread_safe::<Batch>();
    }

    #[test]
    fn a_collection_handle_keeps_the_database_open() {
        let (_dir, _path, db) = open_temp();
        let numbers = db.collection::<i64>("numbers");
        drop(db);

        let id = numbers.insert(7).unwrap();
        assert_eq!(numbers.get(&id).unwrap(), Some(7));
    }

    /// Clones are one open database: the file stays locked until the last
    /// handle, wherever it is, is gone.
    #[test]
    fn the_file_is_released_when_the_last_handle_drops() {
        let (_dir, path, db) = open_temp();
        let clone = db.clone();
        let users = db.collection::<i64>("users");
        let id = clone.collection::<i64>("users").insert(1).unwrap();
        assert_eq!(users.get(&id).unwrap(), Some(1), "one database, not two");

        drop(db);
        drop(clone);
        assert!(Database::open(&path).is_err(), "`users` still holds it");
        drop(users);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.collection::<i64>("users").get(&id).unwrap(), Some(1));
    }

    #[test]
    fn writers_on_several_threads_lose_nothing() {
        let (_dir, path, db) = open_temp();
        let threads: Vec<_> = (0..4)
            .map(|t| {
                // Moved into the thread — possible only because the handle
                // borrows nothing (`thread::spawn` needs `'static`).
                let numbers = db.collection::<i64>("numbers");
                std::thread::spawn(move || {
                    for i in 0..10 {
                        numbers.insert(t * 100 + i).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let mut all = db
            .collection::<i64>("numbers")
            .find(Filter::default())
            .unwrap();
        all.sort();
        let expected: Vec<i64> = (0..4)
            .flat_map(|t| (0..10).map(move |i| t * 100 + i))
            .collect();
        assert_eq!(all, expected);
        drop(db);
        let db = Database::open(&path).unwrap();
        assert_eq!(
            db.collection::<i64>("numbers")
                .find(Filter::default())
                .unwrap()
                .len(),
            40
        );
    }

    /// Each batch inserts two documents; a reader running alongside must
    /// never count an odd number — a batch holds the write lock from
    /// staging to checkpoint.
    #[test]
    fn readers_never_see_half_a_batch() {
        let (_dir, _path, db) = open_temp();
        let pairs = db.collection::<i64>("pairs");
        let done = std::sync::atomic::AtomicBool::new(false);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..30 {
                    let mut batch = db.batch();
                    batch.insert(&pairs, i).unwrap();
                    batch.insert(&pairs, -i).unwrap();
                    batch.commit().unwrap();
                }
                done.store(true, std::sync::atomic::Ordering::Release);
            });
            loop {
                let finished = done.load(std::sync::atomic::Ordering::Acquire);
                let count = pairs.find(Filter::default()).unwrap().len();
                assert_eq!(count % 2, 0, "saw {count} documents");
                if finished {
                    break;
                }
            }
        });
        assert_eq!(pairs.find(Filter::default()).unwrap().len(), 60);
    }

    /// A thread that panics mid-write may leave the store staging (SPEC
    /// §22.4); the lock it held is poisoned, and every handle reports
    /// `Error::Poisoned` instead of reading or writing half a batch.
    /// Reopening — once all handles are gone — gets the committed state.
    #[test]
    fn a_panic_mid_write_poisons_every_handle() {
        let (_dir, path, db) = open_temp();
        let numbers = db.collection::<i64>("numbers");
        let id = numbers.insert(1).unwrap();

        let panicked = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let mut state = db.state();
                    state.store.begin();
                    panic!("simulated bug mid-batch");
                })
                .join()
        });
        assert!(panicked.is_err());

        assert!(matches!(numbers.get(&id), Err(crate::Error::Poisoned)));
        assert!(matches!(numbers.insert(2), Err(crate::Error::Poisoned)));
        drop((db, numbers));
        let db = Database::open(&path).unwrap();
        let numbers = db.collection::<i64>("numbers");
        assert_eq!(numbers.find(Filter::default()).unwrap(), vec![1]);
    }

    // --- Reads through a snapshot (SPEC §78) ---

    /// While a batch is staged, the snapshot is the commit before it:
    /// a read finds none of the batch, not its documents and not the
    /// collection it made. What the write lock hides today (§27), and
    /// what §79 relies on.
    #[test]
    fn a_snapshot_shows_nothing_of_a_staged_batch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        db.write_batch(vec![insert("posts", 1, 10)]).unwrap();

        let mut state = db.state();
        let mut catalog = state.catalog().clone();
        state.store.begin();
        for op in [insert("posts", 2, 20), insert("authors", 3, 30)] {
            apply_write_op(&mut catalog, &mut state.store, &op).unwrap();
        }
        assert!(catalog.get("authors").is_some(), "the batch's own catalog");

        let at = state.snapshot();
        assert!(at.catalog.get("authors").is_none());
        let posts = at.catalog.get("posts").unwrap();
        let entries = BTreeIndex::new(posts.index_root).scan(&at.store).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, DocId([1; 16]).0);
        // The writer's own view has both.
        let staged = catalog.get("posts").unwrap();
        let entries = BTreeIndex::new(staged.index_root).scan(&state.store);
        assert_eq!(entries.unwrap().len(), 2);
        state.store.rollback();
    }

    /// A commit replaces the snapshot; a batch that fails, or changes
    /// nothing, leaves the one there was.
    #[test]
    fn only_a_commit_replaces_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let current = |db: &Database| db.state().current.clone();
        let same = |a: &Snapshot, b: &Snapshot| Arc::ptr_eq(&a.0, &b.0);
        let opened = current(&db);

        db.write_batch(vec![insert("posts", 1, 10)]).unwrap();
        let first = current(&db);
        assert!(!same(&opened, &first));
        assert_eq!(first.0.commit.seq(), opened.0.commit.seq() + 1);
        // The one from before is as it was: no such collection.
        assert!(opened.catalog().get("posts").is_none());
        assert!(first.catalog().get("posts").is_some());

        db.write_batch(vec![insert("authors", 2, 20), insert("posts", 1, 11)])
            .unwrap_err();
        assert!(same(&first, &current(&db)), "a failed batch");

        let docs = db.collection::<Document>("posts");
        docs.ensure_index("n").unwrap();
        let indexed = current(&db);
        assert!(!same(&first, &indexed));
        assert!(!docs.ensure_index("n").unwrap(), "it exists");
        assert!(same(&indexed, &current(&db)), "a batch changing nothing");
    }

    // --- Writers beside the readers (SPEC §79) ---

    /// How long a test waits for something that must happen, before it
    /// calls it a deadlock.
    const SOON: std::time::Duration = std::time::Duration::from_secs(10);
    /// How long a test waits to see that something doesn't happen.
    const A_WHILE: std::time::Duration = std::time::Duration::from_millis(200);

    /// A batch being staged, and then logged and flushed, holds the
    /// writer's lock, which no reader takes: reads go on, on the commit
    /// before it. Until §79 they waited for all of it.
    #[test]
    fn reads_go_on_while_a_batch_is_staged_and_flushed() {
        let (_dir, _path, db) = open_temp();
        db.write_batch(vec![insert("posts", 1, 10)]).unwrap();

        /// What a reader on another thread gets, or an error if it
        /// isn't back soon.
        fn read_beside(db: &Database) -> Result<(usize, Option<Document>), String> {
            let (sent, received) = std::sync::mpsc::channel();
            let db = db.clone();
            std::thread::spawn(move || {
                let posts = db.collection::<Document>("posts");
                let all = posts.find(Filter::new()).unwrap().len();
                let _ = sent.send((all, get(&db, "posts", 2)));
            });
            received
                .recv_timeout(SOON)
                .map_err(|_| "the read waited for the writer".to_string())
        }

        let (sent, flushed) = std::sync::mpsc::channel();
        let beside = db.clone();
        db.state().after_log = Some(Box::new(move || {
            let _ = sent.send(read_beside(&beside));
        }));
        let staged = db
            .transact(|catalog, store| {
                apply_write_op(catalog, store, &insert("posts", 2, 20))?;
                Ok(read_beside(&db))
            })
            .unwrap();

        assert_eq!(staged, Ok((1, None)), "while staged");
        assert_eq!(flushed.try_recv().unwrap(), Ok((1, None)), "while flushed");
        assert_eq!(get(&db, "posts", 2), Some(Document::Int(20)));
    }

    /// Every document of `collection` as the reading has it, by id:
    /// through the primary index, each read from its data page.
    fn contents_at(at: &Reading<'_>, collection: &str) -> Vec<(DocId, Document)> {
        let Some(meta) = at.catalog.get(collection) else {
            return Vec::new();
        };
        let entries = BTreeIndex::new(meta.index_root).scan(&at.store).unwrap();
        let read = |(_key, loc)| data::get_record(&at.store, loc).unwrap();
        entries.into_iter().map(read).collect()
    }

    /// A commit waits for no read, and a read under way goes on reading
    /// its own commit: through commits that change the very pages it
    /// reads, make a collection and drop one, and through a checkpoint
    /// that puts all of it into the file. Until §80 the commit waited.
    #[test]
    fn a_read_keeps_its_commit_while_commits_go_on() {
        let (_dir, _path, db) = open_temp();
        db.write_batch(vec![insert("posts", 1, 10), insert("drafts", 5, 50)])
            .unwrap();
        let before = [(DocId([1; 16]), Document::Int(10))];

        let reading = db.read().unwrap();
        assert_eq!(contents_at(&reading.snapshot(), "posts"), before);

        // On this very thread, with the read still open.
        let update = WriteOp::Update("posts".into(), DocId([1; 16]), Document::Int(11));
        db.write_batch(vec![update, insert("posts", 2, 20)])
            .unwrap();
        db.write_batch(vec![insert("authors", 3, 30)]).unwrap();
        assert!(db.drop_collection("drafts").unwrap());
        db.collection::<Document>("posts")
            .ensure_index("n")
            .unwrap();
        db.checkpoint().unwrap();
        let big = Document::Binary(vec![7; 30_000]);
        db.collection::<Document>("authors").insert(big).unwrap();

        let at = reading.snapshot();
        assert_eq!(contents_at(&at, "posts"), before);
        assert!(at.catalog.get("authors").is_none());
        assert!(at.catalog.indexes("posts").is_empty());
        let drafts = contents_at(&at, "drafts");
        assert_eq!(drafts, [(DocId([5; 16]), Document::Int(50))]);
        // The pages as they were, their count and the free list too.
        let pages = at.store.page_count();
        assert!(pages < db.file_info().unwrap().pages);
        assert_eq!(at.store.free_pages().unwrap().len(), 0);

        // A read begun now has all of it.
        assert_eq!(get(&db, "posts", 1), Some(Document::Int(11)));
        assert_eq!(get(&db, "posts", 2), Some(Document::Int(20)));
        assert_eq!(get(&db, "drafts", 5), None);
        assert!(db.check().unwrap().is_ok());
    }

    /// The versions kept for a read go once it has ended: the next
    /// checkpoint leaves no version in memory, and a commit after it
    /// keeps only its own pages, as before §80.
    #[test]
    fn versions_kept_for_a_read_go_when_it_ends() {
        let (_dir, _path, db) = open_temp();
        let numbers = db.collection::<i64>("numbers");
        let id = numbers.insert(0).unwrap();
        db.checkpoint().unwrap();
        let held = |db: &Database| db.inner.pages.versions_held();
        assert_eq!(held(&db), 0);

        let reading = db.read().unwrap();
        for n in 1..=20 {
            assert!(numbers.update(&id, n).unwrap());
        }
        let waiting = db.state().store.unwritten_pages();
        // One older version of each page changed, however many commits.
        assert_eq!(held(&db), 2 * waiting);
        db.checkpoint().unwrap();
        assert_eq!(held(&db), 2 * waiting, "the read is still open");
        let at = reading.snapshot();
        assert_eq!(contents_at(&at, "numbers"), [(id, Document::Int(0))]);

        drop(reading);
        assert!(numbers.update(&id, 21).unwrap());
        // The commit drops them from the pages it changes.
        assert_eq!(held(&db), db.state().store.unwritten_pages());
        db.checkpoint().unwrap();
        assert_eq!(held(&db), 0);
        assert_eq!(numbers.get(&id).unwrap(), Some(21));
    }

    /// `compact` rewrites every page, so it can't keep them as they were
    /// for a snapshot: one that is kept open refuses it, at once, with
    /// nothing changed; and it waits for a read under way, as every
    /// commit did until §80.
    #[test]
    fn compact_is_refused_by_a_kept_snapshot_and_waits_for_a_read() {
        let (_dir, _path, db) = open_temp();
        let docs = db.collection::<Document>("docs");
        let ids: Vec<DocId> = (0..300)
            .map(|i| {
                docs.insert(Document::String("x".repeat(i * 13 % 900)))
                    .unwrap()
            })
            .collect();
        for id in &ids[..250] {
            assert!(docs.delete(id).unwrap());
        }
        let pages = db.file_info().unwrap().pages;

        // Kept from before a later commit, and kept at the last one.
        for commits_after in [1, 0] {
            let kept = db.current();
            let count = docs.count(Filter::new()).unwrap();
            for _ in 0..commits_after {
                docs.insert(Document::Int(1)).unwrap();
            }
            assert!(matches!(db.compact(), Err(crate::Error::SnapshotOpen)));
            assert_eq!(db.file_info().unwrap().pages, pages);
            let at = kept.reading(&db.inner.pages);
            assert_eq!(contents_at(&at, "docs").len(), count);
            // Refused, not poisoned: writes go on.
            docs.insert(Document::Int(2)).unwrap();
        }

        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reading = db.read().unwrap();
            let compacting = scope.spawn(|| {
                let compacted = db.compact().unwrap();
                done.store(true, std::sync::atomic::Ordering::Release);
                compacted
            });
            std::thread::sleep(A_WHILE);
            assert!(
                !done.load(std::sync::atomic::Ordering::Acquire),
                "compacted under a read"
            );
            assert_eq!(contents_at(&reading.snapshot(), "docs").len(), 53);
            drop(reading);
            let compacted = compacting.join().unwrap();
            assert!(compacted.pages_after < compacted.pages_before);
        });
        assert_eq!(docs.count(Filter::new()).unwrap(), 53);
        assert!(db.check().unwrap().is_ok());
    }

    /// Once a batch's fate is unknown (its log failed, and so did taking
    /// the log back), reads and writes are refused alike; reopening
    /// recovers what the WAL holds.
    #[test]
    fn a_batch_of_unknown_fate_refuses_every_call() {
        let (_dir, path, db) = open_temp();
        db.write_batch(vec![insert("posts", 1, 10)]).unwrap();
        db.inner.poisoned.store(true, Ordering::Relaxed);

        let posts = db.collection::<Document>("posts");
        assert!(matches!(
            posts.get(&DocId([1; 16])),
            Err(crate::Error::Poisoned)
        ));
        assert!(matches!(
            db.write_batch(vec![insert("posts", 2, 20)]),
            Err(crate::Error::Poisoned)
        ));
        assert!(matches!(
            db.write_batch(vec![]),
            Err(crate::Error::Poisoned)
        ));
        assert!(matches!(db.checkpoint(), Err(crate::Error::Poisoned)));
        drop((db, posts));
        assert_eq!(
            get(&Database::open(&path).unwrap(), "posts", 1),
            Some(Document::Int(10))
        );
    }

    /// Two threads writing at once take turns: every batch lands whole.
    #[test]
    fn writers_take_turns() {
        let (_dir, _path, db) = open_temp();
        let numbers = db.collection::<i64>("numbers");
        std::thread::scope(|scope| {
            for thread in 0..4i64 {
                let (db, numbers) = (&db, &numbers);
                scope.spawn(move || {
                    for i in 0..25 {
                        let mut batch = db.batch();
                        batch.insert(numbers, thread * 100 + i).unwrap();
                        batch.insert(numbers, -(thread * 100 + i)).unwrap();
                        batch.commit().unwrap();
                    }
                });
            }
        });
        let mut all = numbers.find(Filter::new()).unwrap();
        assert_eq!(all.len(), 200);
        assert_eq!(all.iter().sum::<i64>(), 0);
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 199, "each number once, 0 twice");
        assert!(db.check().unwrap().is_ok());
    }

    /// Readers beside a writer that commits back to back and checkpoints
    /// after every commit, with a cache too small to keep a page: every
    /// read meets a checkpoint writing the file, and reads the file
    /// itself. Each batch moves an amount between two documents, updates
    /// an indexed field and now and then deletes, inserts or compacts,
    /// so a reader that saw half a batch, or a page half written, would
    /// see a sum that's off, an index that disagrees, or a damaged page.
    #[test]
    fn readers_see_whole_commits_beside_checkpoints() {
        #[derive(Serialize, Deserialize, Clone)]
        struct Account {
            n: i64,
            amount: i64,
            pad: String,
        }
        const ACCOUNTS: i64 = 40;
        const TOTAL: i64 = ACCOUNTS * 100;

        for cache_pages in [0, 3, 1 << 15] {
            let dir = tempfile::tempdir().unwrap();
            let options = OpenOptions::default()
                .checkpoint_pages(1)
                .cache_size(cache_pages * PAGE_SIZE);
            let db = Database::open_with(dir.path().join("test.trunkdb"), options).unwrap();
            let accounts = db.collection::<Account>("accounts");
            accounts.ensure_index("n").unwrap();
            let mut batch = db.batch();
            let ids: Vec<DocId> = (0..ACCOUNTS)
                .map(|n| {
                    // Of different sizes, so pages fill unevenly.
                    let pad = "x".repeat(n as usize * 97 % 1500);
                    let account = Account {
                        n,
                        amount: 100,
                        pad,
                    };
                    batch.insert(&accounts, account).unwrap()
                })
                .collect();
            batch.commit().unwrap();
            let done = std::sync::atomic::AtomicBool::new(false);

            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    let mut rng = crate::testing::XorShift(7 + cache_pages as u64);
                    let extras = db.collection::<Document>("extras");
                    let mut extra_ids = Vec::new();
                    for round in 0..150 {
                        let (from, to) = (rng.below(ids.len()), rng.below(ids.len()));
                        if from != to {
                            let mut a = accounts.get(&ids[from]).unwrap().unwrap();
                            let mut b = accounts.get(&ids[to]).unwrap().unwrap();
                            let amount = rng.below(50) as i64;
                            a.amount -= amount;
                            b.amount += amount;
                            // The pad changes size, so documents move.
                            b.pad = "y".repeat(rng.below(2000));
                            let mut batch = db.batch();
                            batch.update(&accounts, &ids[from], a).unwrap();
                            batch.update(&accounts, &ids[to], b).unwrap();
                            batch.commit().unwrap();
                        }
                        // Pages freed and taken again, and a file that
                        // shrinks under the readers.
                        match round % 10 {
                            3 => {
                                let big = Document::String("z".repeat(20_000));
                                extra_ids.push(extras.insert(big).unwrap());
                            }
                            7 => {
                                for id in extra_ids.drain(..) {
                                    assert!(extras.delete(&id).unwrap());
                                }
                            }
                            9 => {
                                db.compact().unwrap();
                            }
                            _ => {}
                        }
                    }
                    done.store(true, std::sync::atomic::Ordering::Release);
                });

                let mut readers = Vec::new();
                for reader in 0..3 {
                    let (db, accounts, ids, done) = (&db, &accounts, &ids, &done);
                    readers.push(scope.spawn(move || {
                        let mut reads = 0;
                        while !done.load(std::sync::atomic::Ordering::Acquire) {
                            let all = accounts.find(Filter::new()).unwrap();
                            assert_eq!(all.len(), ACCOUNTS as usize);
                            assert_eq!(all.iter().map(|a| a.amount).sum::<i64>(), TOTAL);
                            // Through the index, and by id.
                            let n = (reads + reader) % ACCOUNTS;
                            let by_index = accounts.find(Filter::new().eq("n", n)).unwrap();
                            assert_eq!(by_index.len(), 1, "n = {n}");
                            let by_id = accounts.get(&ids[n as usize]).unwrap().unwrap();
                            assert_eq!(by_id.n, n);
                            if reads % 7 == 0 {
                                let report = db.check().unwrap();
                                assert!(report.is_ok(), "{report:#?}");
                            }
                            // One read kept open over many commits: the
                            // same documents at its end as at its start
                            // (SPEC §80).
                            if reads % 3 == 0 {
                                let reading = db.read().unwrap();
                                let first = contents_at(&reading.snapshot(), "accounts");
                                std::thread::sleep(std::time::Duration::from_millis(2));
                                let again = contents_at(&reading.snapshot(), "accounts");
                                assert!(first == again, "a kept read changed");
                                let amounts = first.iter().map(|(_id, doc)| match doc {
                                    Document::Object(fields) => match fields["amount"] {
                                        Document::Int(amount) => amount,
                                        _ => panic!("not an amount"),
                                    },
                                    _ => panic!("not an account"),
                                });
                                assert_eq!(amounts.sum::<i64>(), TOTAL);
                            }
                            reads += 1;
                        }
                        reads
                    }));
                }
                writer.join().unwrap();
                for reader in readers {
                    assert!(reader.join().unwrap() > 0);
                }
            });
            let all = accounts.find(Filter::new()).unwrap();
            assert_eq!(all.iter().map(|a| a.amount).sum::<i64>(), TOTAL);
            assert!(db.check().unwrap().is_ok());
        }
    }

    /// Random batches, snapshots taken and let go, checkpoints and
    /// compactions, in any order, against a model: a copy of the whole
    /// database's contents per snapshot taken. Every open snapshot reads
    /// exactly its copy, after every step, through the primary index and
    /// a secondary one; a read begun now reads the newest. With a cache
    /// of no pages, a few, and many, and checkpoints after every commit,
    /// after a few, and at the default.
    #[test]
    fn every_open_snapshot_reads_its_commit_through_random_writes() {
        type Model = std::collections::BTreeMap<&'static str, Vec<(DocId, Document)>>;
        const NAMES: [&str; 2] = ["a", "b"];

        fn doc(n: i64, pad: usize) -> Document {
            let fields = [
                ("n", Document::Int(n)),
                ("pad", Document::String("p".repeat(pad))),
            ];
            Document::Object(
                fields
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            )
        }

        /// What the reading has, against the model's copy.
        fn assert_reads(at: &Reading<'_>, expected: &Model, what: &str) {
            for name in NAMES {
                let mut docs = contents_at(at, name);
                // The stored document has its id in it (SPEC §59).
                for (id, doc) in &mut docs {
                    if let Document::Object(fields) = doc {
                        assert_eq!(fields.shift_remove("_id"), Some(Document::Id(*id)));
                    }
                }
                assert!(docs == expected[name], "{what}: {name}");
                // The index on `n` has an entry for each, no more.
                if let Some(index) = at.catalog.indexes(name).first() {
                    let entries = BTreeIndex::new(index.root).scan(&at.store).unwrap();
                    assert_eq!(entries.len(), docs.len(), "{what}: {name}'s index");
                }
            }
        }

        let settings = [(0, 1), (3, 1), (3, 8), (1 << 15, 1000), (8, 3)];
        for (seed, (cache_pages, checkpoint_pages)) in settings.into_iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let options = OpenOptions::default()
                .checkpoint_pages(checkpoint_pages)
                .cache_size(cache_pages * PAGE_SIZE);
            let db = Database::open_with(dir.path().join("test.trunkdb"), options).unwrap();
            for name in NAMES {
                db.collection::<Document>(name).ensure_index("n").unwrap();
            }
            let mut rng = crate::testing::XorShift(0x9E3779B97F4A7C15 ^ (seed as u64 + 1));
            let mut model: Model = NAMES.into_iter().map(|name| (name, Vec::new())).collect();
            let mut open: Vec<(Snapshot, Model)> = Vec::new();
            let mut next_id = 0u128;
            let (mut refused, mut compacted, mut most_open) = (0, 0, 0);

            for step in 0..400 {
                let what = format!("seed {seed}, step {step}");
                match rng.below(12) {
                    0..=6 => {
                        let mut ops = Vec::new();
                        for _ in 0..1 + rng.below(6) {
                            let name = NAMES[rng.below(2)];
                            let docs = model.get_mut(name).unwrap();
                            // Sizes from a few bytes to several pages.
                            let pad = match rng.below(12) {
                                0 => 9_000 + rng.below(20_000),
                                1..=4 => rng.below(3_000),
                                _ => rng.below(200),
                            };
                            let new = doc(rng.below(50) as i64, pad);
                            let at = rng.below(docs.len().max(1));
                            match rng.below(4) {
                                0 if !docs.is_empty() => {
                                    let (id, _doc) = docs.remove(at);
                                    ops.push(WriteOp::Delete(name.into(), id));
                                }
                                1 if !docs.is_empty() => {
                                    docs[at].1 = new.clone();
                                    ops.push(WriteOp::Update(name.into(), docs[at].0, new));
                                }
                                _ => {
                                    next_id += 1;
                                    let id = DocId(next_id.to_be_bytes());
                                    // Ids ascend, so the model stays in id order.
                                    docs.push((id, new.clone()));
                                    ops.push(WriteOp::Insert(name.into(), id, new));
                                }
                            }
                        }
                        db.write_batch(ops).expect(&what);
                    }
                    7 | 8 if open.len() < 4 => open.push((db.current(), model.clone())),
                    7..=9 if !open.is_empty() => {
                        open.swap_remove(rng.below(open.len()));
                    }
                    10 => db.checkpoint().expect(&what),
                    _ => match db.compact() {
                        Ok(_) => compacted += 1,
                        Err(crate::Error::SnapshotOpen) => {
                            assert!(!open.is_empty(), "{what}: refused with none open");
                            refused += 1;
                        }
                        Err(e) => panic!("{what}: {e}"),
                    },
                }
                most_open = most_open.max(open.len());
                for (at, (snapshot, expected)) in open.iter().enumerate() {
                    let reading = snapshot.reading(&db.inner.pages);
                    assert_reads(&reading, expected, &format!("{what}, snapshot {at}"));
                }
                let now = db.current();
                assert_reads(&now.reading(&db.inner.pages), &model, &what);
                if step % 40 == 0 {
                    assert!(db.check().unwrap().is_ok(), "{what}");
                }
            }
            assert!(most_open >= 3 && refused > 0 && compacted > 0, "{seed}");

            // With all of them closed, nothing older stays.
            open.clear();
            db.checkpoint().unwrap();
            assert_eq!(db.inner.pages.versions_held(), 0, "seed {seed}");
            assert!(db.check().unwrap().is_ok());
            drop(db);
            let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
            let now = db.current();
            assert_reads(&now.reading(&db.inner.pages), &model, "reopened");
        }
    }
}
