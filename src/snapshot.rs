//! `Database::snapshot` (SPEC §82): the database at one moment, kept for
//! as long as the caller wants to read it.

use crate::catalog::IndexInfo;
use crate::check::{CheckReport, FileInfo};
use crate::collection::Collection;
use crate::cursor::Cursor;
use crate::database::{Committed, Database, ReadGuard};
use crate::document::{DocId, Document};
use crate::export::Summary;
use crate::query::{Filter, QueryPlan};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::Write;

/// The database as it was at one moment: every read through a snapshot
/// sees the same documents, in every collection, whatever is written
/// after it was taken (SPEC §82). For reads that have to agree with each
/// other — a count and the list it counts, an order and its customer —
/// and for a long read beside writers.
///
/// ```
/// # let dir = tempfile::tempdir().unwrap();
/// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb"))?;
/// use trunkdb::query::Filter;
///
/// let numbers = db.collection::<i64>("numbers");
/// numbers.insert(1)?;
///
/// let snapshot = db.snapshot()?;
/// numbers.insert(2)?; // not in the snapshot
///
/// let seen = snapshot.collection::<i64>("numbers");
/// assert_eq!(seen.count(Filter::new())?, 1);
/// assert_eq!(seen.find(Filter::new())?, [1]);
/// assert_eq!(numbers.count(Filter::new())?, 2);
/// # Ok::<(), trunkdb::Error>(())
/// ```
///
/// Taking one costs nothing to speak of, and it holds no lock: reads and
/// writes go on beside it. It is cheap to clone and `Send + Sync`; every
/// clone is the same moment. It can only read: a [`View`] has no
/// `insert`. To write what a snapshot's reads led to, use the
/// [`Database`] as usual; that write is not in the snapshot.
///
/// # What it costs while it is open
/// - **Memory.** A page changed after the snapshot was taken stays in
///   memory as it was, once per page however often it changes, until the
///   snapshot and its clones, views and cursors are dropped.
///   `Database::snapshot_info` says how much that is.
/// - **It can get too old.** That memory is limited
///   (`OpenOptions::snapshot_memory`, 256 MiB unless set). Past the
///   limit the oldest snapshot is ended: every read through it fails
///   with `Error::SnapshotTooOld` from then on. Writers are never held
///   up for a snapshot.
/// - **`Database::compact` is refused**, with `Error::SnapshotOpen`.
/// - **The file stays open**, as with any other handle (SPEC §27).
///
/// So drop a snapshot when its reads are done, and take a new one for
/// the next.
#[derive(Clone)]
pub struct Snapshot {
    db: Database,
    at: Committed,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot").finish_non_exhaustive()
    }
}

/// What open snapshots cost right now — see
/// [`Database::snapshot_info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SnapshotInfo {
    /// How many pages are kept in memory as they were, for the snapshots
    /// open: a snapshot, a view, a cursor, or a read under way.
    pub kept_pages: usize,
    /// Those pages, in bytes.
    pub kept_bytes: u64,
    /// At most how many bytes of them are kept:
    /// `OpenOptions::snapshot_memory`.
    pub limit_bytes: u64,
    /// How many times the limit has ended snapshots since the database
    /// was opened. Not 0: some reader met `Error::SnapshotTooOld`, or
    /// will on its next read.
    pub ended: u64,
}

impl Database {
    /// How much memory open snapshots take right now, and whether the
    /// limit on it has ended any (SPEC §83). What is kept for a snapshot
    /// that has been dropped is let go at the next write of each page, or
    /// the next checkpoint, so this can lag behind.
    pub fn snapshot_info(&self) -> SnapshotInfo {
        let (kept_pages, limit_pages, ended) = self.versions_kept();
        let bytes = |pages: usize| pages as u64 * crate::storage::PAGE_SIZE as u64;
        SnapshotInfo {
            kept_pages,
            kept_bytes: bytes(kept_pages),
            limit_bytes: bytes(limit_pages),
            ended,
        }
    }

    /// The database as it is now, to keep reading as it is now — see
    /// [`Snapshot`]. It has every batch committed before this call, and
    /// none committed after it.
    pub fn snapshot(&self) -> crate::Result<Snapshot> {
        Ok(Snapshot {
            db: self.clone(),
            at: self.pin()?,
        })
    }
}

impl Snapshot {
    /// The collection `name` as the snapshot has it: empty if it didn't
    /// exist then.
    pub fn collection<T>(&self, name: &str) -> View<T> {
        View {
            collection: Collection::pinned(self.db.clone(), name, self.at.clone()),
        }
    }

    fn read(&self) -> crate::Result<ReadGuard<'_>> {
        self.db.read_at(&self.at)
    }

    /// The names of all collections, sorted — see
    /// [`Database::collections`].
    pub fn collections(&self) -> crate::Result<Vec<String>> {
        Database::collections_of(&self.read()?)
    }

    /// Writes every collection to `out` as JSON Lines — see
    /// [`Database::export`], which does this with a snapshot of its own.
    pub fn export(&self, out: impl Write) -> crate::Result<Summary> {
        Database::export_of(&self.read()?, out)
    }

    /// Checks that the snapshot is consistent — see [`Database::check`].
    /// The scan for damaged pages is of the file as it is now: a page's
    /// older bytes aren't on disk any more.
    pub fn check(&self) -> crate::Result<CheckReport> {
        Database::check_of(&self.read()?)
    }

    /// The header's facts as of the snapshot — see
    /// [`Database::file_info`].
    pub fn file_info(&self) -> crate::Result<FileInfo> {
        Database::file_info_of(&self.read()?)
    }
}

/// One collection of a [`Snapshot`]: what a [`Collection`] reads, as it
/// was when the snapshot was taken, and nothing that writes. `T` is as
/// for `Collection<T>`: any serde type, or [`Document`].
///
/// Cheap to clone; it keeps its snapshot, with all that says about
/// memory and `compact`.
///
/// There is nothing to write with, so this doesn't compile:
///
/// ```compile_fail
/// # let dir = tempfile::tempdir().unwrap();
/// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb")).unwrap();
/// let snapshot = db.snapshot().unwrap();
/// snapshot.collection::<i64>("numbers").insert(1);
/// ```
pub struct View<T> {
    /// A handle that reads the snapshot's commit. Only its read methods
    /// are passed on.
    collection: Collection<T>,
}

/// By hand, as for `Collection`: only the handle is cloned, never a `T`.
impl<T> Clone for View<T> {
    fn clone(&self) -> Self {
        View {
            collection: self.collection.clone(),
        }
    }
}

impl<T> View<T> {
    pub fn name(&self) -> &str {
        self.collection.name()
    }

    /// The collection's secondary indexes as of the snapshot — see
    /// [`Collection::indexes`].
    pub fn indexes(&self) -> crate::Result<Vec<IndexInfo>> {
        self.collection.indexes()
    }

    /// How `find` would run `filter` — see [`Collection::explain`]. With
    /// the indexes the snapshot has: one made since isn't used.
    pub fn explain(&self, filter: &Filter) -> crate::Result<QueryPlan> {
        self.collection.explain(filter)
    }
}

impl<T: Serialize + DeserializeOwned> View<T> {
    /// The document `id`, if the snapshot has it — see
    /// [`Collection::get`].
    pub fn get(&self, id: &DocId) -> crate::Result<Option<T>> {
        self.collection.get(id)
    }

    /// Every match — see [`Collection::find`].
    pub fn find(&self, filter: Filter) -> crate::Result<Vec<T>> {
        self.collection.find(filter)
    }

    /// The first match — see [`Collection::find_one`].
    pub fn find_one(&self, filter: Filter) -> crate::Result<Option<T>> {
        self.collection.find_one(filter)
    }

    /// How many documents match — see [`Collection::count`]. Always what
    /// `find` with the same filter returns, on the same snapshot.
    pub fn count(&self, filter: Filter) -> crate::Result<usize> {
        self.collection.count(filter)
    }

    /// A streaming `find` — see [`Cursor`].
    pub fn cursor(&self, filter: Filter) -> crate::Result<Cursor<T>> {
        self.collection.cursor(filter)
    }
}

impl View<Document> {
    /// The document `id`, if the snapshot has it — see
    /// [`Collection::get`].
    pub fn get(&self, id: &DocId) -> crate::Result<Option<Document>> {
        self.collection.get(id)
    }

    /// Every match — see [`Collection::find`].
    pub fn find(&self, filter: Filter) -> crate::Result<Vec<Document>> {
        self.collection.find(filter)
    }

    /// The first match — see [`Collection::find_one`].
    pub fn find_one(&self, filter: Filter) -> crate::Result<Option<Document>> {
        self.collection.find_one(filter)
    }

    /// How many documents match — see [`Collection::count`]. Always what
    /// `find` with the same filter returns, on the same snapshot.
    pub fn count(&self, filter: Filter) -> crate::Result<usize> {
        self.collection.count(filter)
    }

    /// A streaming `find` — see [`Cursor`].
    pub fn cursor(&self, filter: Filter) -> crate::Result<Cursor<Document>> {
        self.collection.cursor(filter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IndexOptions;
    use serde::Deserialize;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Order {
        customer: String,
        total: i64,
    }

    fn order(customer: &str, total: i64) -> Order {
        Order {
            customer: customer.to_string(),
            total,
        }
    }

    fn open() -> (tempfile::TempDir, std::path::PathBuf, Database) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        (dir, path, db)
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn snapshots_and_views_are_send_and_sync() {
        assert_send_sync::<Snapshot>();
        assert_send_sync::<View<Order>>();
        // Whatever `T` is: the handle holds none.
        assert_send_sync::<View<std::rc::Rc<u8>>>();
    }

    /// Every read through a snapshot is of the moment it was taken:
    /// counts and lists agree, in every collection, whatever is inserted,
    /// updated, deleted, indexed, created or dropped afterwards.
    #[test]
    fn a_snapshot_is_one_moment_across_calls_and_collections() {
        let (_dir, _path, db) = open();
        let orders = db.collection::<Order>("orders");
        let customers = db.collection::<String>("customers");
        let mut batch = db.batch();
        let ann = batch.insert(&customers, "Ann".to_string()).unwrap();
        let first = batch.insert(&orders, order("Ann", 10)).unwrap();
        let second = batch.insert(&orders, order("Ann", 20)).unwrap();
        batch.commit().unwrap();
        orders.ensure_index("total").unwrap();

        let snapshot = db.snapshot().unwrap();
        let info_then = db.file_info().unwrap();

        assert!(orders.update(&first, order("Ann", 11)).unwrap());
        assert!(orders.delete(&second).unwrap());
        let third = orders.insert(order("Bob", 30)).unwrap();
        assert!(customers.delete(&ann).unwrap());
        orders.ensure_index("customer").unwrap();
        orders.drop_index("total").unwrap();
        db.collection::<i64>("later").insert(1).unwrap();
        assert!(db.drop_collection("customers").unwrap());
        db.checkpoint().unwrap();

        let then = snapshot.collection::<Order>("orders");
        assert_eq!(then.name(), "orders");
        let all = Filter::new();
        assert_eq!(then.count(all.clone()).unwrap(), 2);
        assert_eq!(
            then.find(all.clone()).unwrap(),
            [order("Ann", 10), order("Ann", 20)]
        );
        assert_eq!(then.get(&first).unwrap(), Some(order("Ann", 10)));
        assert_eq!(then.get(&second).unwrap(), Some(order("Ann", 20)));
        assert_eq!(then.get(&third).unwrap(), None);
        let over_15 = Filter::new().gt("total", 15);
        assert_eq!(
            then.find_one(over_15.clone()).unwrap(),
            Some(order("Ann", 20))
        );
        let streamed: Vec<Order> = then
            .cursor(all.clone())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(streamed, [order("Ann", 10), order("Ann", 20)]);
        // The indexes it had, and the plans they give.
        let indexes = then.indexes().unwrap();
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].fields(), ["total"]);
        assert!(matches!(
            then.explain(&over_15).unwrap(),
            QueryPlan::Index { .. }
        ));
        let by_customer = Filter::new().eq("customer", "Ann");
        assert_eq!(then.explain(&by_customer).unwrap(), QueryPlan::Scan);
        assert_eq!(then.count(by_customer).unwrap(), 2);
        // The other collections, as they were.
        assert_eq!(snapshot.collections().unwrap(), ["customers", "orders"]);
        let customers_then = snapshot.collection::<String>("customers");
        assert_eq!(customers_then.find(all.clone()).unwrap(), ["Ann"]);
        assert_eq!(
            snapshot
                .collection::<i64>("later")
                .count(all.clone())
                .unwrap(),
            0
        );
        let report = snapshot.check().unwrap();
        assert!(report.is_ok(), "{report:#?}");
        assert_eq!((report.collections, report.documents), (2, 3));
        assert_eq!(snapshot.file_info().unwrap(), info_then);
        assert_ne!(db.file_info().unwrap(), info_then);

        // Untyped, the same documents.
        let untyped = snapshot.collection::<Document>("orders");
        assert_eq!(untyped.count(all.clone()).unwrap(), 2);
        let docs = untyped.find(all.clone()).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(untyped.get(&first).unwrap().as_ref(), Some(&docs[0]));
        assert_eq!(
            untyped.find_one(all.clone()).unwrap().as_ref(),
            Some(&docs[0])
        );
        assert_eq!(untyped.cursor(all.clone()).unwrap().count(), 2);

        // The database itself has moved on.
        assert_eq!(
            orders.find(all.clone()).unwrap(),
            [order("Ann", 11), order("Bob", 30)]
        );
        assert_eq!(db.collections().unwrap(), ["later", "orders"]);
        // And a snapshot taken now is of now.
        let now = db.snapshot().unwrap().collection::<Order>("orders");
        assert_eq!(now.count(all).unwrap(), 2);
        assert_eq!(now.get(&third).unwrap(), Some(order("Bob", 30)));
    }

    /// A clone of a snapshot is the same moment, and so is a view of it
    /// and a clone of that; each keeps it open on its own.
    #[test]
    fn clones_and_views_keep_the_same_moment() {
        let (_dir, _path, db) = open();
        let numbers = db.collection::<i64>("numbers");
        numbers.insert(1).unwrap();
        let snapshot = db.snapshot().unwrap();
        let clone = snapshot.clone();
        let view = snapshot.collection::<i64>("numbers");
        numbers.insert(2).unwrap();
        drop(snapshot);
        numbers.insert(3).unwrap();
        db.checkpoint().unwrap();

        let all = Filter::new();
        assert_eq!(
            clone
                .collection::<i64>("numbers")
                .find(all.clone())
                .unwrap(),
            [1]
        );
        drop(clone);
        numbers.insert(4).unwrap();
        db.checkpoint().unwrap();
        let cloned_view = view.clone();
        drop(view);
        assert_eq!(cloned_view.find(all.clone()).unwrap(), [1]);
        assert_eq!(numbers.count(all).unwrap(), 4);
        assert_eq!(format!("{:?}", db.snapshot().unwrap()), "Snapshot { .. }");
    }

    /// An export of a snapshot is the export that would have been made
    /// when it was taken, byte for byte.
    #[test]
    fn a_snapshots_export_is_the_export_of_its_moment() {
        let (_dir, _path, db) = open();
        let orders = db.collection::<Order>("orders");
        for total in 0..200 {
            orders.insert(order("Ann", total)).unwrap();
        }
        let mut then = Vec::new();
        db.export(&mut then).unwrap();
        let snapshot = db.snapshot().unwrap();

        orders.delete_many(Filter::new().lt("total", 100)).unwrap();
        orders.insert(order("Bob", 1)).unwrap();
        db.collection::<i64>("more").insert(7).unwrap();
        db.checkpoint().unwrap();

        let mut from_snapshot = Vec::new();
        let summary = snapshot.export(&mut from_snapshot).unwrap();
        assert_eq!((summary.collections, summary.documents), (1, 200));
        assert!(from_snapshot == then, "the snapshot's export differs");
        let mut now = Vec::new();
        assert_eq!(db.export(&mut now).unwrap().documents, 102);
    }

    /// `compact` can't keep the pages as they were, so anything that
    /// keeps a snapshot refuses it: the snapshot, a clone, a view, a
    /// cursor. With the last of them dropped it goes through.
    #[test]
    fn whatever_keeps_a_snapshot_refuses_compaction() {
        let (_dir, _path, db) = open();
        let docs = db.collection::<String>("docs");
        let ids: Vec<DocId> = (0..300)
            .map(|i| docs.insert("x".repeat(i * 13 % 900)).unwrap())
            .collect();
        for id in &ids[..290] {
            assert!(docs.delete(id).unwrap());
        }
        let refused = |db: &Database| matches!(db.compact(), Err(crate::Error::SnapshotOpen));

        let snapshot = db.snapshot().unwrap();
        assert!(refused(&db));
        let clone = snapshot.clone();
        drop(snapshot);
        assert!(refused(&db));
        let view = clone.collection::<String>("docs");
        drop(clone);
        assert!(refused(&db));
        let cursor = view.cursor(Filter::new()).unwrap();
        drop(view);
        assert!(refused(&db));
        // Refused, not broken: the database writes, and the cursor reads.
        docs.insert("more".to_string()).unwrap();
        assert_eq!(cursor.count(), 10);

        let compacted = db.compact().unwrap();
        assert!(compacted.pages_after < compacted.pages_before);
        assert_eq!(docs.count(Filter::new()).unwrap(), 11);
    }

    /// A snapshot is a handle like any other (SPEC §27): the file stays
    /// open, and locked, until it's dropped too.
    #[test]
    fn a_snapshot_keeps_the_database_open() {
        let (_dir, path, db) = open();
        db.collection::<i64>("numbers").insert(1).unwrap();
        let snapshot = db.snapshot().unwrap();
        let view = snapshot.collection::<i64>("numbers");
        drop((db, snapshot));

        assert!(Database::open(&path).is_err(), "still open");
        assert_eq!(view.find(Filter::new()).unwrap(), [1]);
        drop(view);
        let db = Database::open(&path).unwrap();
        assert_eq!(
            db.collection::<i64>("numbers")
                .count(Filter::new())
                .unwrap(),
            1
        );
    }

    /// The workload of §77.1: a reader that counts, takes its time, and
    /// lists, beside a writer inserting back to back. On one snapshot the
    /// two always agree, though the writer commits in between every
    /// time: the reader waits for that, with its snapshot open, so a
    /// writer held up by a snapshot would stop the test.
    #[test]
    fn a_count_and_a_list_agree_beside_a_writer() {
        let (_dir, _path, db) = open();
        let orders = db.collection::<Order>("orders");
        orders
            .ensure_index_with("total", IndexOptions::new())
            .unwrap();
        let done = AtomicBool::new(false);
        let written = std::sync::atomic::AtomicI64::new(0);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    let n = written.load(Ordering::Acquire);
                    orders.insert(order("Ann", n)).unwrap();
                    if n % 5 == 4 {
                        let earlier = Filter::new().eq("total", n - 3);
                        assert_eq!(orders.delete_many(earlier).unwrap(), 1);
                    }
                    written.store(n + 1, Ordering::Release);
                }
            });
            let mut counts = Vec::new();
            for _ in 0..40 {
                let snapshot = db.snapshot().unwrap();
                let then = snapshot.collection::<Order>("orders");
                let count = then.count(Filter::new()).unwrap();

                // Until the writer has committed again, twice over.
                let before = written.load(Ordering::Acquire);
                let waiting = std::time::Instant::now();
                while written.load(Ordering::Acquire) < before + 2 {
                    assert!(
                        waiting.elapsed() < std::time::Duration::from_secs(20),
                        "the writer is held up"
                    );
                    std::thread::yield_now();
                }

                let listed = then.find(Filter::new()).unwrap();
                assert_eq!(listed.len(), count);
                let through_the_index = then.count(Filter::new().gte("total", 0)).unwrap();
                assert_eq!(through_the_index, count);
                counts.push(count);
            }
            done.store(true, Ordering::Release);
            // Each snapshot was taken after two more commits of the
            // writer's, of which a delete takes back one insert at most.
            let never_back = counts.windows(2).all(|pair| pair[0] <= pair[1]);
            assert!(never_back && counts[39] > counts[0] + 20, "{counts:?}");
        });
        assert!(db.check().unwrap().is_ok());
    }

    /// A poisoned database refuses a snapshot's reads like any other
    /// (SPEC §27.3).
    #[test]
    fn a_poisoned_database_refuses_snapshots_too() {
        let (_dir, _path, db) = open();
        db.collection::<i64>("numbers").insert(1).unwrap();
        let snapshot = db.snapshot().unwrap();
        let view = snapshot.collection::<i64>("numbers");
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
        assert!(matches!(
            view.find(Filter::new()),
            Err(crate::Error::Poisoned)
        ));
        assert!(matches!(
            snapshot.collections(),
            Err(crate::Error::Poisoned)
        ));
        assert!(matches!(db.snapshot(), Err(crate::Error::Poisoned)));
    }

    // --- A limit on what snapshots keep (SPEC §83) ---

    fn open_with_limit(pages: usize) -> (tempfile::TempDir, std::path::PathBuf, Database) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let limit = pages * crate::storage::PAGE_SIZE;
        let options = crate::OpenOptions::default().snapshot_memory(limit);
        let db = Database::open_with(&path, options).unwrap();
        (dir, path, db)
    }

    /// 200 documents of about a kilobyte each: some thirty pages.
    fn fill(db: &Database) -> Vec<DocId> {
        let docs = db.collection::<String>("docs");
        let mut batch = db.batch();
        let ids = (0..200)
            .map(|i| batch.insert(&docs, format!("{i:01000}")).unwrap())
            .collect();
        batch.commit().unwrap();
        ids
    }

    #[test]
    fn the_limit_is_256_mib_unless_set() {
        let (_dir, _path, db) = open();
        let info = db.snapshot_info();
        assert_eq!(
            (info.limit_bytes, info.kept_pages, info.ended),
            (256 << 20, 0, 0)
        );
        let (_dir, _path, db) = open_with_limit(10);
        assert_eq!(db.snapshot_info().limit_bytes, 10 * 8192);
    }

    /// A snapshot kept while more is rewritten than snapshots may keep is
    /// ended: every read through it, of any kind, fails with
    /// `SnapshotTooOld`, and stays failed. The writer never fails, a
    /// snapshot taken afterwards reads, and what was kept is let go.
    #[test]
    fn a_snapshot_kept_past_the_limit_is_too_old() {
        let (_dir, _path, db) = open_with_limit(10);
        let ids = fill(&db);
        let docs = db.collection::<String>("docs");
        let snapshot = db.snapshot().unwrap();
        let then = snapshot.collection::<String>("docs");
        let mut cursor = then.cursor(Filter::new()).unwrap();
        assert!(cursor.next().unwrap().is_ok());

        // A few pages rewritten: kept, and within the limit.
        for id in &ids[..20] {
            assert!(docs.update(id, "changed".to_string()).unwrap());
        }
        let info = db.snapshot_info();
        assert!(
            info.kept_pages > 0 && info.kept_bytes <= info.limit_bytes,
            "{info:?}"
        );
        assert_eq!(info.ended, 0);
        assert_eq!(then.count(Filter::new()).unwrap(), 200);
        assert_eq!(then.get(&ids[0]).unwrap(), Some(format!("{:01000}", 0)));

        // All of them: more than ten pages' worth.
        for id in &ids {
            assert!(docs.update(id, "changed again".to_string()).unwrap());
        }
        let info = db.snapshot_info();
        assert!(
            info.ended > 0 && info.kept_bytes <= info.limit_bytes,
            "{info:?}"
        );

        let too_old = |result: crate::Result<()>| {
            assert!(
                matches!(result, Err(crate::Error::SnapshotTooOld)),
                "{result:?}"
            );
        };
        too_old(then.find(Filter::new()).map(drop));
        too_old(then.get(&ids[150]).map(drop));
        too_old(then.count(Filter::new()).map(drop));
        too_old(then.find_one(Filter::new()).map(drop));
        too_old(then.cursor(Filter::new()).map(drop));
        too_old(cursor.next().unwrap().map(drop));
        too_old(then.indexes().map(drop));
        too_old(snapshot.collections().map(drop));
        too_old(snapshot.export(std::io::sink()).map(drop));
        too_old(snapshot.check().map(drop));
        too_old(snapshot.file_info().map(drop));
        too_old(
            snapshot
                .collection::<Document>("docs")
                .find(Filter::new())
                .map(drop),
        );
        // For good, also once the file has everything.
        db.checkpoint().unwrap();
        too_old(then.get(&ids[0]).map(drop));

        // The database is as well as ever, and a new snapshot reads.
        assert_eq!(
            docs.get(&ids[0]).unwrap(),
            Some("changed again".to_string())
        );
        let now = db.snapshot().unwrap().collection::<String>("docs");
        assert_eq!(now.count(Filter::new()).unwrap(), 200);
        assert!(db.check().unwrap().is_ok());

        // With the old ones dropped, nothing is kept.
        drop((snapshot, then, cursor, now));
        db.checkpoint().unwrap();
        assert_eq!(db.snapshot_info().kept_pages, 0);
    }

    /// The limit takes the oldest snapshot first, and leaves a newer one
    /// that needs less.
    #[test]
    fn the_oldest_snapshot_goes_first() {
        let (_dir, _path, db) = open_with_limit(12);
        let ids = fill(&db);
        let docs = db.collection::<String>("docs");
        let older = db.snapshot().unwrap();
        for id in &ids[..60] {
            assert!(docs.update(id, "x".to_string()).unwrap());
        }
        let newer = db.snapshot().unwrap();
        for id in &ids[60..110] {
            assert!(docs.update(id, "y".to_string()).unwrap());
        }
        assert_eq!(db.snapshot_info().ended, 1);

        let all = Filter::new();
        let older = older.collection::<String>("docs");
        assert!(matches!(
            older.count(all.clone()),
            Err(crate::Error::SnapshotTooOld)
        ));
        let newer = newer.collection::<String>("docs");
        assert_eq!(newer.count(all.clone()).unwrap(), 200);
        let changed = newer
            .find(all)
            .unwrap()
            .iter()
            .filter(|doc| *doc == "x")
            .count();
        assert_eq!(changed, 60, "the newer one's moment");
    }

    /// One long read is a snapshot too: an export beside which more is
    /// rewritten than snapshots may keep fails, and the writer doesn't.
    #[test]
    fn a_long_read_past_the_limit_fails_and_the_writer_does_not() {
        let (_dir, _path, db) = open_with_limit(4);
        let ids = fill(&db);

        /// Rewrites every document at the export's first write.
        struct Rewriter {
            db: Database,
            ids: Vec<DocId>,
            done: bool,
        }
        impl Write for Rewriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if !self.done {
                    let docs = self.db.collection::<String>("docs");
                    for id in &self.ids {
                        assert!(docs.update(id, "rewritten".to_string()).unwrap());
                    }
                    self.done = true;
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut out = Rewriter {
            db: db.clone(),
            ids,
            done: false,
        };
        let result = db.export(&mut out);
        assert!(out.done);
        assert!(
            matches!(result, Err(crate::Error::SnapshotTooOld)),
            "{result:?}"
        );
        // Again, with nobody writing beside it.
        assert_eq!(db.export(std::io::sink()).unwrap().documents, 200);
    }

    /// A snapshot the limit has ended keeps nothing, so it no longer
    /// stands in the way of a compaction, though it is still held.
    #[test]
    fn an_ended_snapshot_does_not_refuse_compaction() {
        let (_dir, _path, db) = open_with_limit(4);
        let ids = fill(&db);
        let docs = db.collection::<String>("docs");
        let snapshot = db.snapshot().unwrap();
        for id in &ids[..190] {
            assert!(docs.delete(id).unwrap());
        }
        assert!(db.snapshot_info().ended > 0);
        // One that is still good does.
        let good = db.snapshot().unwrap();
        assert!(matches!(db.compact(), Err(crate::Error::SnapshotOpen)));
        drop(good);

        let compacted = db.compact().unwrap();
        assert!(compacted.pages_after < compacted.pages_before);
        let then = snapshot.collection::<String>("docs");
        assert!(matches!(
            then.count(Filter::new()),
            Err(crate::Error::SnapshotTooOld)
        ));
        assert_eq!(docs.count(Filter::new()).unwrap(), 10);
    }

    /// Readers beside a writer that rewrites more than snapshots may
    /// keep, so their snapshots are ended under them again and again, in
    /// the middle of a read too. A read then either is whole and of one
    /// moment — every transfer in it entirely or not at all — or fails
    /// with `SnapshotTooOld`. Never anything else, and never a mix.
    #[test]
    fn a_read_ended_by_the_limit_fails_and_never_reads_a_mix() {
        #[derive(Clone, Serialize, Deserialize)]
        struct Account {
            amount: i64,
            pad: String,
        }
        const ACCOUNTS: usize = 60;
        const TOTAL: i64 = ACCOUNTS as i64 * 100;

        /// Whether the read was whole, and of one moment; `false` if it
        /// was refused as too old. Anything else panics.
        fn whole(listed: crate::Result<Vec<Account>>) -> bool {
            match listed {
                Ok(listed) => {
                    assert_eq!(listed.len(), ACCOUNTS);
                    assert_eq!(listed.iter().map(|a| a.amount).sum::<i64>(), TOTAL);
                    true
                }
                Err(crate::Error::SnapshotTooOld) => false,
                Err(e) => panic!("{e}"),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let options = crate::OpenOptions::default()
            .snapshot_memory(2 * crate::storage::PAGE_SIZE)
            .checkpoint_pages(1)
            .cache_size(3 * crate::storage::PAGE_SIZE);
        let db = Database::open_with(dir.path().join("test.trunkdb"), options).unwrap();
        let accounts = db.collection::<Account>("accounts");
        let mut batch = db.batch();
        let ids: Vec<DocId> = (0..ACCOUNTS)
            .map(|_| {
                let account = Account {
                    amount: 100,
                    pad: "x".repeat(700),
                };
                batch.insert(&accounts, account).unwrap()
            })
            .collect();
        batch.commit().unwrap();
        let done = AtomicBool::new(false);
        // One kept from before the first transfer to after the last: it
        // is ended for certain, however the threads fall.
        let kept = db.snapshot().unwrap();

        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut rng = crate::testing::XorShift(11);
                for _ in 0..400 {
                    let (from, to) = (rng.below(ACCOUNTS), rng.below(ACCOUNTS));
                    if from == to {
                        continue;
                    }
                    let mut a = accounts.get(&ids[from]).unwrap().unwrap();
                    let mut b = accounts.get(&ids[to]).unwrap().unwrap();
                    a.amount -= 7;
                    b.amount += 7;
                    let mut batch = db.batch();
                    batch.update(&accounts, &ids[from], a).unwrap();
                    batch.update(&accounts, &ids[to], b).unwrap();
                    // The writer is never the one to fail.
                    batch.commit().unwrap();
                }
                done.store(true, Ordering::Release);
            });

            let readers: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(|| {
                        let (mut read, mut too_old) = (0, 0);
                        while !done.load(Ordering::Acquire) {
                            // One call, and a snapshot kept over two.
                            let outcomes = [whole(accounts.find(Filter::new())), {
                                let snapshot = db.snapshot().unwrap();
                                let then = snapshot.collection::<Account>("accounts");
                                let first = whole(then.find(Filter::new()));
                                std::thread::yield_now();
                                // Ended in between is fine; back again is not.
                                let again = whole(then.find(Filter::new()));
                                assert!(first || !again, "an ended snapshot read again");
                                first && again
                            }];
                            for ok in outcomes {
                                read += usize::from(ok);
                                too_old += usize::from(!ok);
                            }
                        }
                        (read, too_old)
                    })
                })
                .collect();
            writer.join().unwrap();
            let (mut read, mut too_old) = (0, 0);
            for reader in readers {
                let (r, t) = reader.join().unwrap();
                read += r;
                too_old += t;
            }
            // How many of each is up to the machine; that there were
            // reads isn't.
            assert!(read + too_old > 0, "{read} read, {too_old} too old");
        });
        assert!(db.snapshot_info().ended > 0);
        assert!(!whole(
            kept.collection::<Account>("accounts").find(Filter::new())
        ));
        let listed = accounts.find(Filter::new()).unwrap();
        assert_eq!(listed.iter().map(|a| a.amount).sum::<i64>(), TOTAL);
        assert!(db.check().unwrap().is_ok());
    }

    // --- Page reuse beside an open snapshot: the free-space map (§75) and
    // the free list (§80) ---

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Padded {
        n: i64,
        pad: String,
    }

    fn padded(n: i64, len: usize) -> Padded {
        Padded {
            n,
            pad: format!("{n}:").repeat(len / 2 + 1)[..len].to_string(),
        }
    }

    /// `count` documents of `len` bytes numbered from `from`, in batches
    /// of 100 (one commit each), with the ids they got.
    fn insert_padded(
        db: &Database,
        docs: &Collection<Padded>,
        from: i64,
        count: i64,
        len: usize,
    ) -> Vec<(DocId, Padded)> {
        let mut made = Vec::new();
        for chunk in (from..from + count).collect::<Vec<i64>>().chunks(100) {
            let mut batch = db.batch();
            for &n in chunk {
                let doc = padded(n, len);
                made.push((batch.insert(docs, doc.clone()).unwrap(), doc));
            }
            batch.commit().unwrap();
        }
        made
    }

    /// Every way to read `view` gives exactly `expected` (sorted by `n`).
    fn assert_view_is(view: &View<Padded>, expected: &[(DocId, Padded)]) {
        assert_eq!(view.count(Filter::new()).unwrap(), expected.len());
        for (id, doc) in expected {
            assert_eq!(view.get(id).unwrap().as_ref(), Some(doc));
        }
        let want: Vec<Padded> = expected.iter().map(|(_, d)| d.clone()).collect();
        let mut listed = view.find(Filter::new()).unwrap();
        listed.sort_by_key(|d| d.n);
        assert_eq!(listed, want);
    }

    /// Deletes leave room in the collection's older data pages and the
    /// free-space map sends the next inserts there, into pages a snapshot
    /// is reading. It reads its documents as they were.
    #[test]
    fn a_snapshot_keeps_its_documents_when_inserts_refill_their_pages() {
        let (_dir, _path, db) = open();
        let docs = db.collection::<Padded>("docs");
        let mut original = insert_padded(&db, &docs, 0, 2000, 400);
        original.sort_by_key(|(_, d)| d.n);
        let snapshot = db.snapshot().unwrap();
        let cursor = snapshot
            .collection::<Padded>("docs")
            .cursor(Filter::new())
            .unwrap();

        let mut batch = db.batch();
        for (id, _) in original.iter().step_by(2) {
            batch.delete(&docs, id);
        }
        batch.commit().unwrap();
        let pages_before = db.file_info().unwrap().pages;
        let fresh = insert_padded(&db, &docs, 10_000, 1000, 400);
        db.checkpoint().unwrap();
        let grown = db.file_info().unwrap().pages - pages_before;
        // The premise: 1,000 new documents of 400 bytes need about 50
        // pages of 8 KB; they took the room the deletes left, so the file
        // grew by index pages only.
        assert!(
            grown < 25,
            "the file grew by {grown} pages: nothing refilled"
        );

        assert_view_is(&snapshot.collection::<Padded>("docs"), &original);
        let mut streamed: Vec<Padded> = cursor.map(Result::unwrap).collect();
        streamed.sort_by_key(|d| d.n);
        let want: Vec<Padded> = original.iter().map(|(_, d)| d.clone()).collect();
        assert_eq!(streamed, want);
        assert!(snapshot.check().unwrap().is_ok());

        // And the present is the present.
        assert_eq!(docs.count(Filter::new()).unwrap(), 2000);
        for (id, doc) in &fresh {
            assert_eq!(docs.get(id).unwrap().as_ref(), Some(doc));
        }
        for (id, _) in original.iter().step_by(2) {
            assert_eq!(docs.get(id).unwrap(), None);
        }
        assert!(db.check().unwrap().is_ok());
    }

    /// The same when the room comes from updates that shrink documents
    /// in place, and the new documents take it.
    #[test]
    fn a_snapshot_keeps_its_documents_when_shrinking_updates_leave_room() {
        let (_dir, _path, db) = open();
        let docs = db.collection::<Padded>("docs");
        let mut original = insert_padded(&db, &docs, 0, 1000, 600);
        original.sort_by_key(|(_, d)| d.n);
        let snapshot = db.snapshot().unwrap();

        let mut batch = db.batch();
        for (id, doc) in &original {
            batch.update(&docs, id, padded(doc.n, 20)).unwrap();
        }
        batch.commit().unwrap();
        let fresh = insert_padded(&db, &docs, 5_000, 600, 500);
        db.checkpoint().unwrap();

        assert_view_is(&snapshot.collection::<Padded>("docs"), &original);
        assert!(snapshot.check().unwrap().is_ok());
        for (id, doc) in &original {
            assert_eq!(docs.get(id).unwrap(), Some(padded(doc.n, 20)));
        }
        for (id, doc) in &fresh {
            assert_eq!(docs.get(id).unwrap().as_ref(), Some(doc));
        }
        assert!(db.check().unwrap().is_ok());
    }

    /// A dropped collection's pages go on the free list and a new
    /// collection takes them. A snapshot from before reads the dropped one
    /// whole, its index included.
    #[test]
    fn a_snapshot_reads_a_dropped_collection_whose_pages_are_taken_again() {
        let (_dir, _path, db) = open();
        let docs = db.collection::<Padded>("docs");
        docs.ensure_index("n").unwrap();
        let mut original = insert_padded(&db, &docs, 0, 800, 400);
        original.sort_by_key(|(_, d)| d.n);
        let snapshot = db.snapshot().unwrap();
        let pages_before = db.file_info().unwrap().pages;

        assert!(db.drop_collection("docs").unwrap());
        let freed = db.file_info().unwrap().free_pages;
        assert!(freed > 50, "only {freed} pages freed");
        let other = db.collection::<Padded>("other");
        other.ensure_index("n").unwrap();
        let fresh = insert_padded(&db, &other, 100_000, 800, 400);
        db.checkpoint().unwrap();
        // The premise: the new collection took the freed pages.
        let info = db.file_info().unwrap();
        assert!(
            info.pages < pages_before + 20 && info.free_pages < freed / 4,
            "{} pages, was {pages_before}; {} free, was {freed}",
            info.pages,
            info.free_pages
        );

        let then = snapshot.collection::<Padded>("docs");
        assert_view_is(&then, &original);
        let range = Filter::new().gte("n", 100).lt("n", 200);
        assert!(matches!(
            then.explain(&range).unwrap(),
            QueryPlan::Index { .. }
        ));
        assert_eq!(then.count(range.clone()).unwrap(), 100);
        let found = then.find(range).unwrap();
        assert!(found.iter().all(|d| (100..200).contains(&d.n)));
        assert!(snapshot.check().unwrap().is_ok());
        assert_eq!(snapshot.collections().unwrap(), ["docs"]);

        assert_view_is(&db.snapshot().unwrap().collection::<Padded>("other"), &{
            let mut f = fresh.clone();
            f.sort_by_key(|(_, d)| d.n);
            f
        });
        assert!(db.check().unwrap().is_ok());
    }

    /// Random inserts, deletes and updates of every size, a checkpoint now
    /// and then, and snapshots of several ages open throughout: every one
    /// reads, every round, exactly what the collection held when it was
    /// taken.
    #[test]
    fn snapshots_of_several_ages_survive_random_churn_and_page_reuse() {
        use std::collections::HashMap;
        let (_dir, _path, db) = open();
        let docs = db.collection::<Padded>("docs");
        docs.ensure_index("n").unwrap();
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut below = |m: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % m as u64) as usize
        };
        let mut model: HashMap<DocId, Padded> = HashMap::new();
        let mut next = 0i64;
        let mut open_snapshots: Vec<(Snapshot, Vec<(DocId, Padded)>)> = Vec::new();

        for round in 0..40 {
            let mut batch = db.batch();
            // Deletes, updates (to any size) and inserts (of any size).
            let live: Vec<DocId> = model.keys().copied().collect();
            for _ in 0..below(40) {
                if live.is_empty() {
                    break;
                }
                let id = live[below(live.len())];
                if !model.contains_key(&id) {
                    continue;
                }
                if below(2) == 0 {
                    batch.delete(&docs, &id);
                    model.remove(&id);
                } else {
                    let doc = padded(model[&id].n, 10 + below(1500));
                    batch.update(&docs, &id, doc.clone()).unwrap();
                    model.insert(id, doc);
                }
            }
            for _ in 0..(20 + below(80)) {
                let doc = padded(next, 10 + below(1500));
                next += 1;
                let id = batch.insert(&docs, doc.clone()).unwrap();
                model.insert(id, doc);
            }
            batch.commit().unwrap();
            if round % 3 == 0 {
                db.checkpoint().unwrap();
            }
            if round % 5 == 0 {
                let mut state: Vec<(DocId, Padded)> =
                    model.iter().map(|(i, d)| (*i, d.clone())).collect();
                state.sort_by_key(|(_, d)| d.n);
                open_snapshots.push((db.snapshot().unwrap(), state));
            }
            for (snapshot, state) in &open_snapshots {
                assert_view_is(&snapshot.collection::<Padded>("docs"), state);
            }
            if round % 10 == 9 {
                assert!(open_snapshots[0].0.check().unwrap().is_ok());
                assert!(db.check().unwrap().is_ok());
            }
        }
        assert_eq!(docs.count(Filter::new()).unwrap(), model.len());
    }

    // --- `compact` beside everything else (SPEC §80.5) ---

    /// A compaction rewrites every page and cuts the file, and takes the
    /// database alone: it waits for the reads under way, lets none begin
    /// until it has published, and is refused while a snapshot or a cursor
    /// is kept. Here it runs again and again beside writers (one moving
    /// money between accounts, one filling and emptying a collection so
    /// there is something to cut), kept snapshots, a slow cursor, exports
    /// and plain finds. Every read sees every account and exactly the
    /// total, a snapshot reads the same moment twice, nothing deadlocks
    /// (a watchdog fails the test instead of hanging), and the database
    /// checks out at the end.
    #[test]
    fn compaction_beside_snapshots_cursors_exports_and_writers_loses_nothing() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
        use std::time::{Duration, Instant};

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        struct Account {
            amount: i64,
            pad: String,
        }
        const ACCOUNTS: usize = 80;
        const TOTAL: i64 = ACCOUNTS as i64 * 100;

        #[derive(Default)]
        struct Counts {
            transfers: AtomicUsize,
            churn: AtomicUsize,
            compacted: AtomicUsize,
            refused: AtomicUsize,
            snapshots: AtomicUsize,
            cursors: AtomicUsize,
            exports: AtomicUsize,
            finds: AtomicUsize,
        }

        fn whole(accounts: &[Account]) {
            assert_eq!(accounts.len(), ACCOUNTS);
            assert_eq!(accounts.iter().map(|a| a.amount).sum::<i64>(), TOTAL);
        }

        fn rng(seed: u64) -> impl FnMut(usize) -> usize {
            let mut state = seed;
            move |below| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % below as u64) as usize
            }
        }

        let (_dir, _path, db) = open();
        let pad = "x".repeat(300);
        let accounts = db.collection::<Account>("accounts");
        let ids: Vec<DocId> = {
            let mut batch = db.batch();
            let ids = (0..ACCOUNTS)
                .map(|_| {
                    let account = Account {
                        amount: 100,
                        pad: pad.clone(),
                    };
                    batch.insert(&accounts, account).unwrap()
                })
                .collect();
            batch.commit().unwrap();
            ids
        };
        let stop = Arc::new(AtomicBool::new(false));
        let counts = Arc::new(Counts::default());
        let mut threads = Vec::new();
        let mut spawn = |work: Box<dyn FnOnce() + Send>| threads.push(std::thread::spawn(work));

        // The one writer of the accounts: transfers, so the total never moves.
        {
            let (db, stop, counts, ids, pad) = (
                db.clone(),
                stop.clone(),
                counts.clone(),
                ids.clone(),
                pad.clone(),
            );
            spawn(Box::new(move || {
                let accounts = db.collection::<Account>("accounts");
                let mut amounts = vec![100i64; ACCOUNTS];
                let mut below = rng(0x9E37_79B9_7F4A_7C15);
                while !stop.load(Relaxed) {
                    let from = below(ACCOUNTS);
                    let to = (from + 1 + below(ACCOUNTS - 1)) % ACCOUNTS;
                    let sum = 1 + below(20) as i64;
                    amounts[from] -= sum;
                    amounts[to] += sum;
                    let mut batch = db.batch();
                    for &i in &[from, to] {
                        let account = Account {
                            amount: amounts[i],
                            pad: pad.clone(),
                        };
                        batch.update(&accounts, &ids[i], account).unwrap();
                    }
                    batch.commit().unwrap();
                    counts.transfers.fetch_add(1, Relaxed);
                    std::thread::sleep(Duration::from_micros(300));
                }
            }));
        }

        // Another writer: bulky documents in and out, so pages are freed.
        {
            let (db, stop, counts) = (db.clone(), stop.clone(), counts.clone());
            spawn(Box::new(move || {
                let churn = db.collection::<Padded>("churn");
                let mut live = std::collections::VecDeque::new();
                let mut next = 0;
                while !stop.load(Relaxed) {
                    let mut batch = db.batch();
                    for _ in 0..25 {
                        live.push_back(batch.insert(&churn, padded(next, 1200)).unwrap());
                        next += 1;
                    }
                    if live.len() > 150 {
                        for _ in 0..50 {
                            batch.delete(&churn, &live.pop_front().unwrap());
                        }
                    }
                    batch.commit().unwrap();
                    counts.churn.fetch_add(1, Relaxed);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
        }

        // The compactor: done, or refused while something is kept.
        {
            let (db, stop, counts) = (db.clone(), stop.clone(), counts.clone());
            spawn(Box::new(move || {
                while !stop.load(Relaxed) {
                    match db.compact() {
                        Ok(_) => counts.compacted.fetch_add(1, Relaxed),
                        Err(crate::Error::SnapshotOpen) => counts.refused.fetch_add(1, Relaxed),
                        Err(e) => panic!("compact: {e}"),
                    };
                    std::thread::sleep(Duration::from_millis(4));
                }
            }));
        }

        // Kept snapshots: the same moment twice, a moment apart, and whole.
        for seed in [1u64, 2] {
            let (db, stop, counts) = (db.clone(), stop.clone(), counts.clone());
            spawn(Box::new(move || {
                let mut below = rng(seed * 0x2545_F491_4F6C_DD1D);
                while !stop.load(Relaxed) {
                    let snapshot = db.snapshot().unwrap();
                    let view = snapshot.collection::<Account>("accounts");
                    let first = view.find(Filter::new()).unwrap();
                    whole(&first);
                    std::thread::sleep(Duration::from_millis(1 + below(3) as u64));
                    assert_eq!(view.find(Filter::new()).unwrap(), first, "one moment");
                    assert_eq!(view.count(Filter::new()).unwrap(), ACCOUNTS);
                    assert!(snapshot.check().unwrap().is_ok());
                    counts.snapshots.fetch_add(1, Relaxed);
                    drop(snapshot);
                    std::thread::sleep(Duration::from_millis(1 + below(4) as u64));
                }
            }));
        }

        // A slow cursor: kept for as long as it is iterated.
        {
            let (db, stop, counts) = (db.clone(), stop.clone(), counts.clone());
            spawn(Box::new(move || {
                let accounts = db.collection::<Account>("accounts");
                while !stop.load(Relaxed) {
                    let mut seen = Vec::new();
                    for (n, item) in accounts.cursor(Filter::new()).unwrap().enumerate() {
                        seen.push(item.unwrap());
                        if n % 10 == 9 {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                    whole(&seen);
                    counts.cursors.fetch_add(1, Relaxed);
                    std::thread::sleep(Duration::from_millis(3));
                }
            }));
        }

        // Exports: a long read that holds up a compaction waiting to begin.
        {
            let (db, stop, counts) = (db.clone(), stop.clone(), counts.clone());
            spawn(Box::new(move || {
                while !stop.load(Relaxed) {
                    let mut out = Vec::new();
                    db.export(&mut out).unwrap();
                    assert!(!out.is_empty());
                    counts.exports.fetch_add(1, Relaxed);
                    std::thread::sleep(Duration::from_millis(2));
                }
            }));
        }

        // Plain finds, each its own short read.
        {
            let (db, stop, counts) = (db.clone(), stop.clone(), counts.clone());
            spawn(Box::new(move || {
                let accounts = db.collection::<Account>("accounts");
                while !stop.load(Relaxed) {
                    whole(&accounts.find(Filter::new()).unwrap());
                    counts.finds.fetch_add(1, Relaxed);
                }
            }));
        }

        std::thread::sleep(Duration::from_secs(4));
        stop.store(true, Relaxed);
        // Every thread must finish: one that doesn't is a deadlock. A
        // thread that panicked has finished too, and `join` says why.
        let deadline = Instant::now() + Duration::from_secs(30);
        for thread in threads {
            while !thread.is_finished() {
                assert!(Instant::now() < deadline, "a thread is stuck: deadlock");
                std::thread::sleep(Duration::from_millis(5));
            }
            thread.join().unwrap();
        }

        // Nothing is kept now: one more compaction must go through.
        db.compact().unwrap();
        let ended = accounts.find(Filter::new()).unwrap();
        whole(&ended);
        assert!(db.check().unwrap().is_ok());
        assert_eq!(db.snapshot_info().ended, 0);

        let c = &counts;
        let (transfers, compacted, refused) = (
            c.transfers.load(Relaxed),
            c.compacted.load(Relaxed),
            c.refused.load(Relaxed),
        );
        eprintln!(
            "transfers {transfers}, churn {}, compacted {compacted}, refused {refused}, \
             snapshots {}, cursors {}, exports {}, finds {}",
            c.churn.load(Relaxed),
            c.snapshots.load(Relaxed),
            c.cursors.load(Relaxed),
            c.exports.load(Relaxed),
            c.finds.load(Relaxed),
        );
        // That it ran beside all of it, and met both outcomes.
        assert!(transfers > 0 && c.churn.load(Relaxed) > 0);
        assert!(c.snapshots.load(Relaxed) > 0 && c.cursors.load(Relaxed) > 0);
        assert!(c.exports.load(Relaxed) > 0 && c.finds.load(Relaxed) > 0);
        assert!(compacted > 0, "no compaction ever went through");
        assert!(refused > 0, "no compaction was ever refused");
    }
}
