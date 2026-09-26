use crate::batch::Batch;
use crate::catalog::Catalog;
use crate::collection::{Collection, apply_write_op};
use crate::data;
use crate::durability::{Durability, WalDurability};
use crate::id::UuidV7Generator;
use crate::index::{BTreeIndex, Index};
use crate::storage::{FileStore, PageId};
use crate::txn::{GlobalLockTxnManager, TransactionManager, WriteOp};
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Opens the file and owns the whole stack — as a cheap, cloneable,
/// thread-safe handle (SPEC §27): every clone refers to the same open
/// database, and `Database`, `Collection` and `Batch` are all `Send +
/// Sync`, so they can go into an app's shared state, a thread, or a
/// `static`. The file stays open (and locked, §21.1) until the last clone
/// — including those inside `Collection`s and `Batch`es — is dropped.
///
/// Reads (`get`, `find`) share a read lock and run in parallel; a write
/// batch takes the write lock for its whole commit (stage to checkpoint,
/// SPEC §19.3), so readers see a batch entirely or not at all.
#[derive(Clone)]
pub struct Database {
    inner: Arc<Shared>,
}

/// What every clone shares. Only `state` changes after `open`, so only it
/// sits behind the lock.
struct Shared {
    state: RwLock<State>,
    id_gen: UuidV7Generator,
    txn: GlobalLockTxnManager,
}

/// The mutable part of an open database. Index and data-page code take
/// `&mut dyn PageStore` / `&mut Catalog` as plain parameters rather than
/// holding handles of their own, so the lock guard is the one place they
/// come from.
pub(crate) struct State {
    pub(crate) store: FileStore,
    pub(crate) catalog: Catalog,
    durability: WalDurability,
    /// Set when a batch was durably logged but couldn't be written to the
    /// main file, even on retry — see `write_batch`. From then on every
    /// call fails with `Error::Poisoned` until the database is reopened.
    poisoned: bool,
    /// `OpenOptions::checkpoint_pages`, as of `open_with`: how many
    /// committed pages may wait before a commit writes them back.
    checkpoint_pages: usize,
}

/// How `Database::open_with` opens a database. Made with
/// `OpenOptions::default()` and its setters; the fields are private and the
/// type `#[non_exhaustive]`, so a new option breaks nobody.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenOptions {
    cache_size: usize,
    checkpoint_pages: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            cache_size: crate::storage::DEFAULT_CACHE_SIZE,
            checkpoint_pages: DEFAULT_CHECKPOINT_PAGES,
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
    /// WAL that grows with every commit, not with this number, which the
    /// next open after a crash reads back whole (§53.3). Default: 1,000.
    pub fn checkpoint_pages(mut self, pages: usize) -> Self {
        self.checkpoint_pages = pages;
        self
    }
}

/// How many committed pages may wait in memory, logged but not written
/// back, before a commit writes them back (SPEC §51): 1,000 pages of 8 KB each —
/// SQLite's default for its WAL mode too.
const DEFAULT_CHECKPOINT_PAGES: usize = 1000;

impl State {
    /// `Database::checkpoint`: the pages to the file, then the WAL
    /// emptied — only after they're durably in the file. If the WAL can't
    /// be emptied, its records are written back once more at the next
    /// open: harmless, page images are idempotent.
    fn checkpoint(&mut self) -> std::io::Result<()> {
        self.store.checkpoint()?;
        let _ = self.durability.checkpoint();
        Ok(())
    }
}

/// The last handle gone: what's committed goes to the main file, so it's
/// complete without its WAL. Best effort — if it fails, the next open
/// recovers the same pages from the WAL.
impl Drop for Shared {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut()
            && !state.poisoned
        {
            let _ = state.checkpoint();
        }
    }
}

impl Database {
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
        let catalog = match Catalog::load(&mut store) {
            Ok(catalog) => catalog,
            Err(e) => {
                store.rollback();
                return Err(e.into());
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

        Ok(Self {
            inner: Arc::new(Shared {
                state: RwLock::new(State {
                    store,
                    catalog,
                    durability,
                    poisoned: false,
                    checkpoint_pages: options.checkpoint_pages,
                }),
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

    /// Shared access for a read. `Err(Error::Poisoned)` once a failed
    /// write has left the main file possibly half-written (see
    /// `write_batch`), or a thread panicked in the middle of one — which
    /// poisons the lock and may have left the store staging (SPEC §27.3).
    /// Every public entry point goes through this or `write`.
    pub(crate) fn read(&self) -> crate::Result<RwLockReadGuard<'_, State>> {
        let state = self
            .inner
            .state
            .read()
            .map_err(|_| crate::Error::Poisoned)?;
        if state.poisoned {
            return Err(crate::Error::Poisoned);
        }
        Ok(state)
    }

    /// Exclusive access for a write batch; poisoned as for `read`.
    fn write(&self) -> crate::Result<RwLockWriteGuard<'_, State>> {
        let state = self
            .inner
            .state
            .write()
            .map_err(|_| crate::Error::Poisoned)?;
        if state.poisoned {
            return Err(crate::Error::Poisoned);
        }
        Ok(state)
    }

    /// Applies every op in `ops` as one atomic, durable unit. Ops may name
    /// different collections (each `WriteOp` carries its own) — that's the
    /// actual point: a multi-entity update (SPEC §4.4) needs exactly
    /// this, which a sequence of separate `Collection::insert`/`update`/
    /// `delete` calls can't give you, since each of those is its own batch.
    pub fn write_batch(&self, ops: Vec<WriteOp>) -> crate::Result<()> {
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
    /// atomic, durable unit, under the write lock. `write_batch` is one
    /// use; building or dropping an index (SPEC §28) is another.
    ///
    /// The protocol (SPEC §19.3):
    /// 1. **stage** — `FileStore::begin`; every page write from here on
    ///    stays in memory.
    /// 2. **apply**. On error: roll back the staged pages *and* the
    ///    catalog cache, and return the error — nothing of it ever
    ///    reached the file, so there's nothing else to undo.
    /// 3. **log** every changed page to the WAL as one record, `fsync` —
    ///    the one flush a commit waits for (SPEC §51).
    /// 4. **commit** — the pages become the newest committed ones, read
    ///    from memory; the main file isn't touched.
    /// 5. **checkpoint**, once `OpenOptions::checkpoint_pages` or more are
    ///    waiting (default 1,000): write them back to the main file,
    ///    `fsync`, truncate the WAL.
    ///
    /// A crash before 3 completes leaves the state before; a crash after
    /// it leaves complete WAL records that `open` writes back, giving the
    /// state after. Never anything in between.
    pub(crate) fn transact<R>(
        &self,
        apply: impl FnOnce(&mut Catalog, &mut FileStore) -> crate::Result<R>,
    ) -> crate::Result<R> {
        let mut guard = self.write()?;
        // Reborrow the guard as a plain `&mut State` once, so the borrow
        // checker can see that `store`, `catalog` and `durability` below
        // are separate fields, each borrowable on its own.
        let state = &mut *guard;
        let catalog_before = state.catalog.clone();

        state.store.begin();
        let result = match apply(&mut state.catalog, &mut state.store) {
            Ok(result) => result,
            Err(e) => {
                state.store.rollback();
                state.catalog = catalog_before;
                return Err(e);
            }
        };

        let pages: Vec<(PageId, &[u8])> = state.store.dirty_pages().collect();
        if pages.is_empty() {
            // Nothing changed (e.g. an index that already existed):
            // nothing to log or write, just end staging.
            drop(pages);
            state.store.rollback();
            return Ok(result);
        }
        let logged = state.durability.log(&pages);
        drop(pages);
        if let Err(e) = logged {
            state.store.rollback();
            state.catalog = catalog_before;
            // A failed `log` may still have left a complete record behind
            // (say the write landed but the fsync failed), which the next
            // `open` would restore — for a batch this call reports as
            // failed. Truncating the log rules that out; if even that
            // fails, the batch's fate is genuinely unknown.
            if state.durability.checkpoint().is_err() {
                state.poisoned = true;
            }
            return Err(e.into());
        }

        // The batch is durable from here on: the WAL holds all its pages.
        state.store.commit();
        if state.store.unwritten_pages() >= state.checkpoint_pages {
            // Not an error for this batch if it fails: it's durable, and
            // reads find its pages in memory. The next one tries again.
            let _ = state.checkpoint();
        }
        Ok(result)
    }

    /// Writes every committed page back to the main file and empties the
    /// WAL (SPEC §51) — which a commit does by itself once enough pages
    /// wait, and dropping the last handle does too. For a file that is
    /// complete on its own: before copying it, say.
    pub fn checkpoint(&self) -> crate::Result<()> {
        Ok(self.write()?.checkpoint()?)
    }

    /// Direct access to the state for tests, bypassing the poisoned
    /// checks — e.g. to inject write-back faults, or inspect the catalog.
    #[cfg(test)]
    pub(crate) fn state(&self) -> RwLockWriteGuard<'_, State> {
        self.inner.state.write().unwrap()
    }
}

#[cfg(test)]
// `find_with_ids` and `find_one_with_id` are deprecated (SPEC §59) but
// work until they're removed before 1.0; these tests keep them covered.
#[allow(deprecated)]
mod tests {
    use super::*;
    use crate::document::{DocId, Document};
    use crate::query::Filter;
    use crate::storage::PageType;
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
        docs.ensure_unique_index("u").unwrap();
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
            assert_eq!(db.read().unwrap().store.cache_size(), size / 8192);
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
        assert_eq!(again.indexes().unwrap(), ["n", "u"]);
        assert_eq!(again.unique_indexes().unwrap(), ["u"]);
        let n_is_3 = Filter::new().eq("n", 3);
        assert_eq!(again.find(n_is_3).unwrap().len(), 28);

        assert!(db.check().unwrap().is_ok(), "{:#?}", db.check().unwrap());
        // A handle to a dropped collection still works: it starts over.
        let gone = db.collection::<Document>("gone");
        assert_eq!(gone.count(Filter::new()).unwrap(), 0);
        gone.insert(Document::Int(1)).unwrap();
        assert_eq!(gone.count(Filter::new()).unwrap(), 1);
        assert!(gone.indexes().unwrap().is_empty());
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
                store.failing_write_backs = 1;
                store.write_back_fails_after = pages_written;
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
        use crate::storage::PAGE_SIZE;
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
        assert!(db.state().catalog.get("posts").is_none());

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
        db.state().store.failing_write_backs = 1;
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
                let mut state = db.state();
                assert!(state.store.unwritten_pages() > fails_after);
                state.store.failing_write_backs = 2;
                state.store.write_back_fails_after = fails_after;
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
        db.state().store.failing_write_backs = 2;
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
        assert!(db.state().catalog.get("posts").is_none());
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
                    store.failing_write_backs = 1;
                    store.write_back_fails_after = pages_written;
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
            // two separate `state.catalog`/`state.store` borrows through
            // the guard couldn't.
            let State { catalog, store, .. } = &mut *state;
            let snapshot = catalog.clone();
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
                    *catalog = snapshot;
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
}
