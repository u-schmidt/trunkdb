use crate::catalog::Catalog;
use crate::collection::{
    Collection, Upserted, any_match_in, apply_write_op, delete_matching_in, update_by_id,
    update_matching_in, upsert_fields_in,
};
use crate::database::Database;
use crate::document::{DocId, Document};
use crate::id::IdGenerator;
use crate::query::{Filter, Update};
use crate::storage::PageStore;
use crate::txn::WriteOp;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// What a batch stores (SPEC §60): any serde type, as `Collection<T>`
/// converts it, or a `Document` as it is, for a `Collection<Document>`.
/// Sealed: only these two.
pub trait IntoDocument: sealed::Sealed {
    #[doc(hidden)]
    fn into_document(self) -> crate::Result<Document>;
}

/// What a batch's `update_fields` reads back (SPEC §144.1): any serde
/// type, which the changed document must still convert to, or a
/// `Document`, which takes any. Sealed: only these two.
pub trait FromDocument: sealed::SealedFrom {
    #[doc(hidden)]
    fn accept(doc: &Document) -> crate::Result<()>;
}

mod sealed {
    /// Outside code can't name this, so it can't implement `IntoDocument`.
    pub trait Sealed {}
    impl<T: serde::Serialize> Sealed for T {}
    impl Sealed for crate::Document {}

    /// The same for `FromDocument`.
    pub trait SealedFrom {}
    impl<T: serde::de::DeserializeOwned> SealedFrom for T {}
    impl SealedFrom for crate::Document {}
}

/// As for `IntoDocument`: `Document` isn't `Deserialize`.
impl<T: DeserializeOwned> FromDocument for T {
    fn accept(doc: &Document) -> crate::Result<()> {
        // `T`'s conversions are the application's code (SPEC §137).
        crate::query::caught(|| crate::serde_bridge::from_document::<T>(doc.clone()).map(drop))?
            .map_err(Into::into)
    }
}

impl FromDocument for Document {
    fn accept(_doc: &Document) -> crate::Result<()> {
        Ok(())
    }
}

/// One op of a batch: a write as it is, or operators to make to the
/// document as the ops before it leave it.
enum Op {
    Write(WriteOp),
    Fields {
        collection: String,
        id: DocId,
        update: Update,
        accept: fn(&Document) -> crate::Result<()>,
    },
    /// A precondition (SPEC §162): `filter` must match a document of
    /// `collection`, or with `none`, no document, as the ops before
    /// leave it.
    Require {
        collection: String,
        filter: Filter,
        none: bool,
    },
    /// `update`'s operators made to what `filter` finds (SPEC §168), as
    /// the ops before leave it.
    UpdateMany {
        collection: String,
        filter: Filter,
        update: Update,
        accept: fn(&Document) -> crate::Result<()>,
    },
    /// What `filter` finds deleted (SPEC §168), as the ops before leave
    /// it.
    DeleteMany {
        collection: String,
        filter: Filter,
    },
    /// `update`'s operators made to the one document `filter` finds, or
    /// `seed` and them inserted as `id` where it finds none (SPEC §175).
    Upsert {
        collection: String,
        filter: Filter,
        id: DocId,
        seed: Document,
        update: Update,
        accept: fn(&Document) -> crate::Result<()>,
    },
}

/// What a batch did (SPEC §168): `Batch::commit` returns it, and
/// `Batch::dry_run`, which writes nothing, says what it would.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Written {
    counts: Vec<usize>,
    /// For each op, what it upserted: `Some` for `upsert_fields` only.
    upserted: Vec<Option<Upserted>>,
}

impl Written {
    /// For each op, in the order they were added, how many documents it
    /// wrote: 1 for an insert, an update or a delete by id; for
    /// `update_fields` 1, or 0 where the operators left the document as it
    /// was; for `update_many` the documents it changed, for `delete_many`
    /// those it deleted; for `upsert_fields` 1, or 0 where it found a
    /// document the operators left as it was; 0 for a precondition.
    pub fn counts(&self) -> &[usize] {
        &self.counts
    }

    /// What op `op` of the batch, counted from 0 in the order added, did
    /// if it is an `upsert_fields` (SPEC §175): `Upserted::Inserted` with
    /// the new document's id, or `Upserted::Updated` with the id of the one
    /// it found, written or left as it was. `None` for any other op, and
    /// past the last.
    pub fn upserted(&self, op: usize) -> Option<Upserted> {
        self.upserted.get(op).copied().flatten()
    }

    /// How many documents the ops wrote, all of them together: a document
    /// two ops wrote counts twice.
    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }
}

/// The two can't overlap: `Document` isn't `Serialize`, and being local to
/// this crate, only this crate could make it so.
impl<T: Serialize> IntoDocument for T {
    fn into_document(self) -> crate::Result<Document> {
        Document::from_value(&self)
    }
}

impl IntoDocument for Document {
    fn into_document(self) -> crate::Result<Document> {
        Ok(self)
    }
}

/// An atomic multi-op write: collect inserts, updates and deletes against
/// any of this database's collections, typed or untyped, then `commit`
/// them as one unit — all of them, or (on any error) none.
///
/// Each op takes the `Collection` handle it targets, so `T` and the
/// collection name come from the handle rather than being repeated.
/// Values are converted to `Document`s as ops are added, so a value the
/// serde bridge can't represent fails right there, leaving the batch as
/// it was. Errors that depend on the stored state — an insert whose id
/// exists (`Error::DuplicateId`), an update or delete of a missing id
/// (`Error::NotFound`), a precondition that doesn't hold
/// (`Error::PreconditionFailed`, SPEC §162) — surface at `commit`, which
/// then rolls back the whole batch (SPEC §22.2).
///
/// Nothing is read or written before `commit`: reads in the meantime see
/// the state before the batch. Ops apply in the order they were added, so
/// a later op may update or delete a document an earlier one inserted.
/// Dropping a batch without committing discards it.
#[must_use = "a batch writes nothing until `commit`"]
pub struct Batch {
    db: Database,
    ops: Vec<Op>,
}

impl Batch {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            ops: Vec::new(),
        }
    }

    /// Adds an insert and returns the document's id right away — its own
    /// `_id` if it has one (SPEC §59), otherwise one generated here, not at
    /// `commit`, so later ops in the same batch can refer to it. It only
    /// names a stored document once `commit` succeeds.
    pub fn insert<T: IntoDocument>(
        &mut self,
        collection: &Collection<T>,
        doc: T,
    ) -> crate::Result<DocId> {
        let name = self.name_of(collection);
        let document = doc.into_document()?;
        let id = crate::collection::id_for_insert(&self.db, &document);
        self.ops
            .push(Op::Write(WriteOp::Insert(name, id, document)));
        Ok(id)
    }

    /// Adds an update. Unlike `Collection::update`, a missing id isn't an
    /// `Ok(false)` here: it fails the whole batch at `commit`.
    pub fn update<T: IntoDocument>(
        &mut self,
        collection: &Collection<T>,
        id: &DocId,
        doc: T,
    ) -> crate::Result<()> {
        let name = self.name_of(collection);
        let document = doc.into_document()?;
        self.ops
            .push(Op::Write(WriteOp::Update(name, *id, document)));
        Ok(())
    }

    /// Adds `update`'s operators to document `id` (SPEC §144.1), made at
    /// `commit` to the document as the ops before leave it, as
    /// `Collection::update_fields` makes them. An update it would refuse
    /// as written is refused here, leaving the batch as it was.
    ///
    /// At `commit`, a missing id fails the whole batch, as for `update`;
    /// so does an operator that can't be made (`Error::Update`), and, on a
    /// typed collection, a changed document that no longer converts to
    /// `T`. A document the operators leave as it was isn't written.
    ///
    /// ```no_run
    /// # fn main() -> trunkdb::Result<()> {
    /// use trunkdb::{Database, DocId, Document, query::Update};
    ///
    /// let db = Database::open("app.trunkdb")?;
    /// let accounts = db.collection::<Document>("accounts");
    /// # let (from, to) = (DocId::NIL, DocId::NIL);
    /// let mut batch = db.batch();
    /// batch.update_fields(&accounts, &from, &Update::new().inc("balance", -25))?;
    /// batch.update_fields(&accounts, &to, &Update::new().inc("balance", 25))?;
    /// batch.commit()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_fields<T: FromDocument>(
        &mut self,
        collection: &Collection<T>,
        id: &DocId,
        update: &Update,
    ) -> crate::Result<()> {
        let name = self.name_of(collection);
        update.check().map_err(crate::collection::invalid_input)?;
        self.ops.push(Op::Fields {
            collection: name,
            id: *id,
            update: update.clone(),
            accept: T::accept,
        });
        Ok(())
    }

    /// Adds a delete. A missing id fails the whole batch at `commit`, as
    /// for `update`.
    pub fn delete<T>(&mut self, collection: &Collection<T>, id: &DocId) {
        let name = self.name_of(collection);
        self.ops.push(Op::Write(WriteOp::Delete(name, *id)));
    }

    /// Adds a precondition (SPEC §162): at `commit`, after the ops before
    /// it and under the same lock as every write, `filter` must match at
    /// least one document of `collection`, or the whole batch writes
    /// nothing and fails with `Error::PreconditionFailed`. So a batch
    /// writes only while what it was decided on still holds: no other
    /// write can come between the check and the batch's own.
    ///
    /// With `Filter::id`, it holds one document to what the program read:
    /// a `version` field the program keeps, or a balance that must cover
    /// what the batch takes from it. A precondition writes nothing and
    /// counts as an op, as `len` counts it.
    ///
    /// Only the filter's conditions count: one with a sort, a skip, a
    /// limit or a selection is refused here, leaving the batch as it
    /// was. Reading follows the filter as `count` does, through an index
    /// where one bounds it, and stops at the first match.
    ///
    /// ```no_run
    /// # fn main() -> trunkdb::Result<()> {
    /// use trunkdb::{Database, DocId, Document, Error, query::{Filter, Update}};
    ///
    /// let db = Database::open("app.trunkdb")?;
    /// let accounts = db.collection::<Document>("accounts");
    /// # let (from, to) = (DocId::NIL, DocId::NIL);
    /// let mut batch = db.batch();
    /// batch.require(&accounts, Filter::new().id(from).gte("balance", 25))?;
    /// batch.update_fields(&accounts, &from, &Update::new().inc("balance", -25))?;
    /// batch.update_fields(&accounts, &to, &Update::new().inc("balance", 25))?;
    /// match batch.commit() {
    ///     Ok(_) => {}
    ///     Err(Error::PreconditionFailed { .. }) => { /* not enough left: nothing moved */ }
    ///     Err(other) => return Err(other),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn require<T>(&mut self, collection: &Collection<T>, filter: Filter) -> crate::Result<()> {
        self.push_require(collection, filter, false)
    }

    /// Adds a precondition that `filter` matches no document of
    /// `collection` at `commit` (SPEC §162), as `require` checks its
    /// own: "no user has this email yet", "no booking overlaps this
    /// slot", for a rule a unique index can't hold. Ops before it count,
    /// so one after an insert sees the inserted document.
    pub fn require_none<T>(
        &mut self,
        collection: &Collection<T>,
        filter: Filter,
    ) -> crate::Result<()> {
        self.push_require(collection, filter, true)
    }

    fn push_require<T>(
        &mut self,
        collection: &Collection<T>,
        filter: Filter,
        none: bool,
    ) -> crate::Result<()> {
        let name = self.name_of(collection);
        conditions_only(&filter, "a precondition's")?;
        self.ops.push(Op::Require {
            collection: name,
            filter,
            none,
        });
        Ok(())
    }

    /// Adds `update`'s operators, made at `commit` to every document of
    /// `collection` that `filter` finds then (SPEC §168): at the op's
    /// place, on the documents as the ops before it leave them, under the
    /// writer's lock, so no other write comes between finding them and
    /// changing them. As `Collection::update_fields` with a filter: its
    /// sort, skip and limit count, "the oldest ten", and a document the
    /// operators leave as it was isn't written. `Written::counts`
    /// says how many were.
    ///
    /// An update refused as written is refused here, leaving the batch as
    /// it was. At `commit`, an operator that can't be made fails the whole
    /// batch (`Error::Update`), as does, on a typed collection, a changed
    /// document that no longer converts to `T`. Finding nothing is no
    /// error: a `require` before it says that something must be there.
    ///
    /// ```no_run
    /// # fn main() -> trunkdb::Result<()> {
    /// use trunkdb::{Database, DateTime, Document, Map, query::{Filter, Update}};
    ///
    /// let db = Database::open("app.trunkdb")?;
    /// let (tasks, log) = (db.collection::<Document>("tasks"), db.collection::<Document>("log"));
    /// let now = DateTime::now();
    /// let overdue = Filter::new().lt("due", now).eq("done", false).eq("overdue", false);
    /// let mut entry = Map::new();
    /// entry.insert("at".to_string(), Document::DateTime(now));
    /// entry.insert("what".to_string(), "marked overdue tasks".into());
    /// let mut batch = db.batch();
    /// batch.update_many(&tasks, overdue, &Update::new().set("overdue", true))?;
    /// batch.insert(&log, Document::from(entry))?;
    /// let written = batch.commit()?;
    /// println!("{} marked", written.counts()[0]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_many<T: FromDocument>(
        &mut self,
        collection: &Collection<T>,
        filter: Filter,
        update: &Update,
    ) -> crate::Result<()> {
        let name = self.name_of(collection);
        update.check().map_err(crate::collection::invalid_input)?;
        self.ops.push(Op::UpdateMany {
            collection: name,
            filter,
            update: update.clone(),
            accept: T::accept,
        });
        Ok(())
    }

    /// Adds a delete of every document of `collection` that `filter`
    /// finds at `commit` (SPEC §168), at the op's place and under the
    /// writer's lock, as `update_many` finds them; its sort, skip and
    /// limit count, as for `Collection::delete_many`. Finding nothing is
    /// no error. No document is converted to `T`.
    pub fn delete_many<T>(&mut self, collection: &Collection<T>, filter: Filter) {
        let name = self.name_of(collection);
        self.ops.push(Op::DeleteMany {
            collection: name,
            filter,
        });
    }

    /// Adds an upsert by operators (SPEC §175), as
    /// `Collection::upsert_fields` makes one, at the op's place: at
    /// `commit`, `update`'s operators are made to the one document of
    /// `collection` that `filter` finds then, as the ops before leave it;
    /// where it finds none, a document made of the filter's `eq`s and
    /// then the operators, `set_on_insert` included, is inserted.
    /// `Written::upserted` says which, and the id.
    ///
    /// Refused here, leaving the batch as it was, with
    /// `ErrorKind::InvalidInput`: an update refused as written, a filter
    /// whose `eq`s can't be made into one document (SPEC §144.2), and a
    /// filter with a sort, a skip, a limit or a selection, as for
    /// `require`: only its conditions count. The id a new document gets is
    /// chosen here, the filter's `_id` if it has one.
    ///
    /// At `commit`, several matches fail the whole batch with
    /// `Error::MultipleMatches`; so does an operator that can't be made
    /// (`Error::Update`), and, on a typed collection, a document written
    /// that doesn't convert to `T`. A match the operators leave as it was
    /// isn't written.
    ///
    /// ```no_run
    /// # fn main() -> trunkdb::Result<()> {
    /// use trunkdb::{Database, DateTime, Document, query::{Filter, Update}};
    ///
    /// let db = Database::open("app.trunkdb")?;
    /// let (visits, totals) = (db.collection::<Document>("visits"), db.collection::<Document>("totals"));
    /// // A page's count and the site's, in one commit: each made by the
    /// // first visit, added to by the later ones.
    /// let mut batch = db.batch();
    /// batch.upsert_fields(
    ///     &visits,
    ///     Filter::new().eq("page", "/home"),
    ///     &Update::new().inc("hits", 1).set_on_insert("first", DateTime::now()),
    /// )?;
    /// batch.upsert_fields(&totals, Filter::new().eq("site", "main"), &Update::new().inc("hits", 1))?;
    /// let written = batch.commit()?;
    /// let page = written.upserted(0).unwrap().id();
    /// # Ok(())
    /// # }
    /// ```
    pub fn upsert_fields<T: FromDocument>(
        &mut self,
        collection: &Collection<T>,
        filter: Filter,
        update: &Update,
    ) -> crate::Result<()> {
        let name = self.name_of(collection);
        conditions_only(&filter, "an upsert's")?;
        update.check().map_err(crate::collection::invalid_input)?;
        let (named, seed) =
            crate::update::seed(&filter.conditions).map_err(crate::collection::invalid_input)?;
        let id = named.unwrap_or_else(|| self.db.id_gen().generate());
        self.ops.push(Op::Upsert {
            collection: name,
            filter,
            id,
            seed,
            update: update.clone(),
            accept: T::accept,
        });
        Ok(())
    }

    /// How many ops the batch holds.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether it holds no ops.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Applies every op as one atomic, durable unit (SPEC §19.3), and
    /// says how many documents each wrote (SPEC §168).
    pub fn commit(self) -> crate::Result<Written> {
        if self.ops.is_empty() {
            // Reports a poisoned database.
            return self.db.write_batch(Vec::new()).map(|()| Written {
                counts: Vec::new(),
                upserted: Vec::new(),
            });
        }
        self.db.transact(|catalog, store| self.run(catalog, store))
    }

    /// Runs every op as `commit` would, under the writer's lock, and
    /// writes nothing (SPEC §168.4): what each would write, or the error
    /// `commit` would fail with, against the database as it is now. Not a
    /// promise: a write after it can change what `commit` finds. It waits
    /// for the writer's lock as a commit does, and a read-only database
    /// refuses it with `Error::ReadOnly`, as it refuses a commit.
    pub fn dry_run(&self) -> crate::Result<Written> {
        self.db.rehearse(|catalog, store| self.run(catalog, store))
    }

    /// The ops, in order, on the catalog and store each leaves for the
    /// next.
    fn run(&self, catalog: &mut Catalog, store: &mut dyn PageStore) -> crate::Result<Written> {
        let mut counts = Vec::with_capacity(self.ops.len());
        let mut upserted = Vec::with_capacity(self.ops.len());
        for (at, op) in self.ops.iter().enumerate() {
            upserted.push(None);
            counts.push(match op {
                Op::Write(write) => {
                    apply_write_op(catalog, store, write)?;
                    1
                }
                // The document as the ops before left it: read here,
                // under the writer's lock.
                Op::Fields {
                    collection,
                    id,
                    update,
                    accept,
                } => usize::from(update_by_id(
                    catalog,
                    store,
                    collection,
                    *id,
                    update,
                    &mut |doc| accept(doc),
                )?),
                // The catalog and store as the ops before left them.
                Op::Require {
                    collection,
                    filter,
                    none,
                } => {
                    let matched = any_match_in(catalog, store, collection, filter)?;
                    if matched == *none {
                        return Err(crate::Error::PreconditionFailed {
                            collection: collection.clone(),
                            op: at,
                            matched,
                        });
                    }
                    0
                }
                Op::UpdateMany {
                    collection,
                    filter,
                    update,
                    accept,
                } => update_matching_in(catalog, store, collection, filter, &mut |id, mut doc| {
                    update
                        .apply(&mut doc)
                        .map_err(|message| crate::Error::Update {
                            collection: collection.clone(),
                            id,
                            message,
                        })?;
                    accept(&doc)?;
                    Ok(doc)
                })?,
                Op::DeleteMany { collection, filter } => {
                    delete_matching_in(catalog, store, collection, filter)?
                }
                Op::Upsert {
                    collection,
                    filter,
                    id,
                    seed,
                    update,
                    accept,
                } => {
                    let (done, written) = upsert_fields_in(
                        catalog,
                        store,
                        collection,
                        filter,
                        (*id, seed),
                        update,
                        &mut |doc| accept(doc),
                    )?;
                    upserted[at] = Some(done);
                    usize::from(written)
                }
            });
        }
        Ok(Written { counts, upserted })
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

/// Refuses `filter` with anything but conditions, for an op where only
/// they count: `whose` filter, as the error names it.
fn conditions_only(filter: &Filter, whose: &str) -> crate::Result<()> {
    let has = [
        (!filter.sort.is_empty() || filter.by_key.is_some(), "a sort"),
        (filter.skip > 0, "a skip"),
        (filter.limit.is_some(), "a limit"),
        (
            filter.select.is_some() || !filter.except.is_empty(),
            "a selection",
        ),
    ];
    match has.iter().find(|(has, _)| *has) {
        Some((_, what)) => Err(crate::collection::invalid_input(format!(
            "{whose} filter can't have {what}: only its conditions are checked"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use crate::query::{Condition, Filter, Op, Update};
    use crate::testing::{Text, text};
    use crate::{Database, DocId, Document, Error};
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
            conditions: vec![Condition::Compare {
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
    /// SPEC §60: one batch takes typed and untyped collections alike; an
    /// untyped document's own `_id` is used (§59), and it all commits or
    /// none of it does.
    #[test]
    fn a_batch_takes_typed_and_untyped_collections() {
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
        struct Car {
            color: String,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let cars = db.collection::<Car>("cars");
        let notes = db.collection::<Document>("notes");
        let given = DocId([5; 16]);
        let note = |text: &str, id: Option<DocId>| {
            let mut fields = crate::Map::new();
            if let Some(id) = id {
                fields.insert("_id".to_string(), Document::Id(id));
            }
            fields.insert("text".to_string(), Document::String(text.into()));
            Document::Object(fields)
        };

        let mut batch = db.batch();
        let car = batch
            .insert(
                &cars,
                Car {
                    color: "black".into(),
                },
            )
            .unwrap();
        assert_eq!(
            batch.insert(&notes, note("first", Some(given))).unwrap(),
            given
        );
        batch.update(&notes, &given, note("changed", None)).unwrap();
        batch.commit().unwrap();
        assert_eq!(cars.get(&car).unwrap().unwrap().color, "black");
        let Some(Document::Object(stored)) = notes.get(&given).unwrap() else {
            panic!("the note is there");
        };
        assert_eq!(stored["text"], Document::String("changed".into()));

        let mut failing = db.batch();
        failing
            .insert(
                &cars,
                Car {
                    color: "red".into(),
                },
            )
            .unwrap();
        failing.insert(&notes, note("again", Some(given))).unwrap();
        assert!(failing.commit().is_err(), "the id is taken");
        assert_eq!(
            cars.count(crate::query::Filter::new()).unwrap(),
            1,
            "rolled back whole"
        );
    }

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
        let log = db.collection::<Text>("log");
        let existing = entries.insert(entry("a", 10)).unwrap();

        let mut batch = db.batch();
        batch.update(&entries, &existing, entry("a", 99)).unwrap();
        let logged = batch.insert(&log, text("synced")).unwrap();
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
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Counter {
            count: u64,
        }
        let counters = db.collection::<Counter>("counters");

        let mut batch = db.batch();
        batch.insert(&counters, Counter { count: 1 }).unwrap();
        assert!(matches!(
            batch.insert(&counters, Counter { count: u64::MAX }),
            Err(Error::Document(_))
        ));
        assert_eq!(batch.len(), 1, "the failed op wasn't added");
        batch.commit().unwrap();
        assert_eq!(
            counters.find(Filter::default()).unwrap(),
            [Counter { count: 1 }]
        );
    }

    /// SPEC §144.1: operators in a batch, on what the ops before them
    /// left: a document inserted, replaced or changed earlier in it.
    #[test]
    fn update_fields_works_on_what_the_batch_has_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let entries = db.collection::<Entry>("entries");
        entries.ensure_index("price").unwrap();
        let (a, b) = (
            entries.insert(entry("a", 100)).unwrap(),
            entries.insert(entry("b", 0)).unwrap(),
        );
        let by = |n: i64| Update::new().inc("price", n);

        let mut batch = db.batch();
        batch.update_fields(&entries, &a, &by(-25)).unwrap();
        batch.update_fields(&entries, &b, &by(25)).unwrap();
        let c = batch.insert(&entries, entry("c", 1)).unwrap();
        batch.update_fields(&entries, &c, &by(1)).unwrap();
        batch
            .update_fields(&entries, &c, &Update::new().mul("price", 10))
            .unwrap();
        batch.update(&entries, &b, entry("b", 50)).unwrap();
        batch.update_fields(&entries, &b, &by(5)).unwrap();
        assert_eq!(batch.len(), 7);
        batch.commit().unwrap();
        assert_eq!(entries.get(&a).unwrap(), Some(entry("a", 75)));
        assert_eq!(entries.get(&b).unwrap(), Some(entry("b", 55)));
        assert_eq!(entries.get(&c).unwrap(), Some(entry("c", 20)));
        assert_eq!(
            entries.count(Filter::new().gte("price", 55)).unwrap(),
            2,
            "the index follows"
        );
    }

    /// SPEC §144.1: a missing id, an operator that can't be made, or a
    /// typed document that no longer converts fails the whole batch; an
    /// update refused as written is refused when added; one that changes
    /// nothing isn't written.
    #[test]
    fn update_fields_in_a_batch_fails_it_whole() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let entries = db.collection::<Entry>("entries");
        let notes = db.collection::<Document>("notes");
        let a = entries.insert(entry("a", 1)).unwrap();
        let note = notes.insert("{\"text\": \"x\"}".parse().unwrap()).unwrap();
        let inc = Update::new().inc("price", 1);
        let missing = DocId([7; 16]);

        let mut gone = db.batch();
        gone.update_fields(&entries, &a, &inc).unwrap();
        gone.update_fields(&entries, &missing, &inc).unwrap();
        let Err(Error::NotFound { id, .. }) = gone.commit() else {
            panic!("expected NotFound");
        };
        assert_eq!(id, missing);

        let mut deleted = db.batch();
        deleted.delete(&entries, &a);
        deleted.update_fields(&entries, &a, &inc).unwrap();
        assert!(matches!(deleted.commit(), Err(Error::NotFound { .. })));

        let mut failing = db.batch();
        let logged = failing.insert(&notes, "{}".parse().unwrap()).unwrap();
        failing.update_fields(&entries, &a, &inc).unwrap();
        failing
            .update_fields(&notes, &note, &Update::new().inc("text", 1))
            .unwrap();
        let Err(Error::Update { collection, id, .. }) = failing.commit() else {
            panic!("expected Error::Update");
        };
        assert_eq!((collection.as_str(), id), ("notes", note));
        assert_eq!(notes.get(&logged).unwrap(), None);

        let mut typed = db.batch();
        typed
            .update_fields(&entries, &a, &Update::new().set("price", "high"))
            .unwrap();
        assert!(matches!(typed.commit(), Err(Error::Document(_))));
        // The untyped path takes it: nothing to convert to.
        let mut untyped = db.batch();
        let raw = db.collection::<Document>("entries");
        untyped
            .update_fields(&raw, &a, &Update::new().set("level", "high"))
            .unwrap();
        untyped.commit().unwrap();
        assert_eq!(
            entries.get(&a).unwrap(),
            Some(entry("a", 1)),
            "nothing else landed"
        );

        let mut refused = db.batch();
        let Err(Error::Io(e)) = refused.update_fields(&entries, &a, &Update::new().unset("_id"))
        else {
            panic!("expected InvalidInput");
        };
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert!(refused.is_empty(), "the refused op wasn't added");

        let changes = db.subscribe().unwrap();
        let mut same = db.batch();
        same.update_fields(&entries, &a, &Update::new().set("price", 1))
            .unwrap();
        same.commit().unwrap();
        assert!(
            changes.try_recv().is_none(),
            "nothing changed, nothing written"
        );
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
