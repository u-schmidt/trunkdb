use crate::collection::Collection;
use crate::database::Database;
use crate::document::DocId;
use crate::id::IdGenerator;
use crate::serde_bridge::to_document;
use crate::txn::WriteOp;
use serde::Serialize;

/// A typed, atomic multi-op write: collect inserts, updates and deletes
/// against any of this database's collections, then `commit` them as one
/// `Database::write_batch` — all of them, or (on any error) none.
///
/// Each op takes the `Collection` handle it targets, so `T` and the
/// collection name come from the handle rather than being repeated.
/// Values are converted to `Document`s as ops are added, so a value the
/// serde bridge can't represent fails right there, leaving the batch as
/// it was. Errors that depend on the stored state — an insert whose id
/// exists (`Error::DuplicateId`), an update or delete of a missing id
/// (`Error::NotFound`) — surface at `commit`, which then rolls back the
/// whole batch (SPEC §22.2).
///
/// Nothing is read or written before `commit`: reads in the meantime see
/// the state before the batch. Ops apply in the order they were added, so
/// a later op may update or delete a document an earlier one inserted.
/// Dropping a batch without committing discards it.
#[must_use = "a batch writes nothing until `commit`"]
pub struct Batch {
    db: Database,
    ops: Vec<WriteOp>,
}

impl Batch {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            ops: Vec::new(),
        }
    }

    /// Adds an insert and returns the new document's id right away — it's
    /// generated here, not at `commit`, so later ops in the same batch can
    /// refer to it. It only names a stored document once `commit`
    /// succeeds.
    pub fn insert<T: Serialize>(
        &mut self,
        collection: &Collection<T>,
        doc: T,
    ) -> crate::Result<DocId> {
        let name = self.name_of(collection);
        let document = to_document(&doc)?;
        let id = self.db.id_gen().generate();
        self.ops.push(WriteOp::Insert(name, id, document));
        Ok(id)
    }

    /// Adds an update. Unlike `Collection::update`, a missing id isn't an
    /// `Ok(false)` here: it fails the whole batch at `commit`.
    pub fn update<T: Serialize>(
        &mut self,
        collection: &Collection<T>,
        id: &DocId,
        doc: T,
    ) -> crate::Result<()> {
        let name = self.name_of(collection);
        let document = to_document(&doc)?;
        self.ops.push(WriteOp::Update(name, *id, document));
        Ok(())
    }

    /// Adds a delete. A missing id fails the whole batch at `commit`, as
    /// for `update`.
    pub fn delete<T>(&mut self, collection: &Collection<T>, id: &DocId) {
        let name = self.name_of(collection);
        self.ops.push(WriteOp::Delete(name, *id));
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Applies every op as one atomic, durable unit (SPEC §19.3).
    pub fn commit(self) -> crate::Result<()> {
        self.db.write_batch(self.ops)
    }

    /// The collection's name — after checking the handle belongs to this
    /// batch's database. A handle from another `Database` would silently
    /// write to a same-named collection here instead; that's a bug in the
    /// caller, so it panics rather than returning an error.
    fn name_of<T>(&self, collection: &Collection<T>) -> String {
        assert!(
            collection.db().same_as(&self.db),
            "Batch op on a collection of a different Database"
        );
        collection.name().to_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::query::{Condition, Filter, Op};
    use crate::{Database, DocId, Error};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Entry {
        key: String,
        price: i64,
    }

    fn entry(key: &str, price: i64) -> Entry {
        Entry {
            key: key.to_string(),
            price,
        }
    }

    fn by_key(key: &str) -> Filter {
        Filter {
            conditions: vec![Condition {
                field: "key".to_string(),
                op: Op::Eq,
                value: crate::Document::String(key.to_string()),
            }],
            ..Filter::default()
        }
    }

    /// The sync workload's shape (SPEC §5.3): look entries up by
    /// business key, then update the known ones, insert the new ones and
    /// drop the vanished ones — as one batch, surviving a reopen.
    #[test]
    fn a_sync_run_is_one_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let new_id = {
            let db = Database::open(&path).unwrap();
            let entries = db.collection::<Entry>("entries");
            entries.insert(entry("a", 10)).unwrap();
            entries.insert(entry("b", 20)).unwrap();

            let (a_id, _) = entries.find_with_ids(by_key("a")).unwrap().remove(0);
            let (b_id, _) = entries.find_with_ids(by_key("b")).unwrap().remove(0);
            let mut batch = db.batch();
            batch.update(&entries, &a_id, entry("a", 11)).unwrap();
            batch.delete(&entries, &b_id);
            let new_id = batch.insert(&entries, entry("c", 30)).unwrap();
            assert_eq!(batch.len(), 3);
            batch.commit().unwrap();
            new_id
        };

        let db = Database::open(&path).unwrap();
        let entries = db.collection::<Entry>("entries");
        let mut all = entries.find(Filter::default()).unwrap();
        all.sort_by(|x, y| x.key.cmp(&y.key));
        assert_eq!(all, [entry("a", 11), entry("c", 30)]);
        assert_eq!(entries.get(&new_id).unwrap(), Some(entry("c", 30)));
    }

    #[test]
    fn later_ops_can_target_a_document_inserted_earlier_in_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let entries = db.collection::<Entry>("entries");

        let mut batch = db.batch();
        let kept = batch.insert(&entries, entry("kept", 1)).unwrap();
        batch.update(&entries, &kept, entry("kept", 2)).unwrap();
        let gone = batch.insert(&entries, entry("gone", 1)).unwrap();
        batch.delete(&entries, &gone);
        batch.commit().unwrap();

        assert_eq!(entries.get(&kept).unwrap(), Some(entry("kept", 2)));
        assert_eq!(entries.get(&gone).unwrap(), None);
    }

    /// One failing op — here an update of a missing id, the last of three
    /// ops across two collections — and nothing of the batch is stored,
    /// not even the insert whose id was already handed out.
    #[test]
    fn a_failing_op_rolls_back_the_whole_batch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let entries = db.collection::<Entry>("entries");
        let log = db.collection::<String>("log");
        let existing = entries.insert(entry("a", 10)).unwrap();

        let mut batch = db.batch();
        batch.update(&entries, &existing, entry("a", 99)).unwrap();
        let logged = batch.insert(&log, "synced".to_string()).unwrap();
        let missing = DocId([7; 16]);
        batch.update(&entries, &missing, entry("x", 1)).unwrap();

        let Err(Error::NotFound { collection, id }) = batch.commit() else {
            panic!("expected NotFound");
        };
        assert_eq!((collection.as_str(), id), ("entries", missing));
        assert_eq!(entries.get(&existing).unwrap(), Some(entry("a", 10)));
        assert_eq!(log.get(&logged).unwrap(), None);
    }

    #[test]
    fn a_value_the_bridge_cant_represent_fails_when_added() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let counters = db.collection::<u64>("counters");

        let mut batch = db.batch();
        batch.insert(&counters, 1).unwrap();
        assert!(matches!(
            batch.insert(&counters, u64::MAX),
            Err(Error::Document(_))
        ));
        assert_eq!(batch.len(), 1, "the failed op wasn't added");
        batch.commit().unwrap();
        assert_eq!(counters.find(Filter::default()).unwrap(), [1]);
    }

    #[test]
    fn a_dropped_batch_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let entries = db.collection::<Entry>("entries");

        let mut batch = db.batch();
        let id = batch.insert(&entries, entry("a", 1)).unwrap();
        drop(batch);

        assert_eq!(entries.get(&id).unwrap(), None);
    }

    #[test]
    #[should_panic(expected = "different Database")]
    fn ops_on_another_databases_collection_panic() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("one.trunkdb")).unwrap();
        let other = Database::open(dir.path().join("two.trunkdb")).unwrap();
        let foreign = other.collection::<Entry>("entries");

        let mut batch = db.batch();
        batch.delete(&foreign, &DocId([1; 16]));
    }
}
