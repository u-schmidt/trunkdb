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
///   snapshot and its clones, views and cursors are dropped. A snapshot
///   forgotten beside a busy writer keeps growing; there is no limit yet.
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

impl Database {
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
    /// two always agree; and the writer isn't held up by it.
    #[test]
    fn a_count_and_a_list_agree_beside_a_writer() {
        let (_dir, _path, db) = open();
        let orders = db.collection::<Order>("orders");
        orders
            .ensure_index_with("total", IndexOptions::new())
            .unwrap();
        let done = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut written = 0;
                while !done.load(Ordering::Acquire) {
                    orders.insert(order("Ann", written)).unwrap();
                    written += 1;
                    if written % 5 == 0 {
                        orders
                            .delete_many(Filter::new().eq("total", written - 3))
                            .unwrap();
                    }
                }
                written
            });
            let mut grew = 0;
            let mut last = 0;
            for _ in 0..40 {
                let snapshot = db.snapshot().unwrap();
                let then = snapshot.collection::<Order>("orders");
                let count = then.count(Filter::new()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(3));
                let listed = then.find(Filter::new()).unwrap();
                assert_eq!(listed.len(), count);
                let through_the_index = then.count(Filter::new().gte("total", 0)).unwrap();
                assert_eq!(through_the_index, count);
                grew += usize::from(count > last);
                last = count;
            }
            done.store(true, Ordering::Release);
            let written = writer.join().unwrap();
            assert!(written > 40, "the writer was held up: {written} inserts");
            assert!(grew > 5, "snapshots didn't move on: {grew}");
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
}
