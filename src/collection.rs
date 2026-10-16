use crate::catalog::{Catalog, CollectionMeta, IndexInfo, IndexMeta, IndexOptions};
use crate::cursor::Cursor;
use crate::data::{self, Stored};
use crate::database::{Committed, Database, ReadGuard};
use crate::document::{DocId, Document, encode_document, with_id};
use crate::explain::Trace;
use crate::id::IdGenerator;
use crate::index::{BTreeIndex, Index, KeyRange, key};
use crate::query::{
    Condition, Covering, Decider, Explained, Filter, OrderedRead, QueryPlan, SortOrder, SortWork,
    Update,
};
use crate::storage::{PageId, PageStore, RecordLocation};
use crate::txn::WriteOp;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// What `ensure_index`, `ensure_unique_index` and `drop_index` take: one
/// field — `"age"`, `"address.city"`, `"tags[*]"` — or several, for a
/// compound index (SPEC §43): `["status", "created"]`.
///
/// Sealed (SPEC §60): usable as a bound, implemented only here, for the
/// forms field names come in.
pub trait IndexFields: sealed::Sealed {
    #[doc(hidden)]
    fn into_fields(self) -> Vec<String>;
}

mod sealed {
    /// Outside code can't name this, so it can't implement `IndexFields`.
    pub trait Sealed {}
    impl Sealed for &str {}
    impl Sealed for String {}
    impl Sealed for &String {}
    impl<const N: usize> Sealed for [&str; N] {}
    impl Sealed for &[&str] {}
    impl Sealed for Vec<String> {}
    impl Sealed for Vec<&str> {}
    impl Sealed for &[String] {}
}

impl IndexFields for &str {
    fn into_fields(self) -> Vec<String> {
        vec![self.to_string()]
    }
}

impl IndexFields for String {
    fn into_fields(self) -> Vec<String> {
        vec![self]
    }
}

impl IndexFields for &String {
    fn into_fields(self) -> Vec<String> {
        vec![self.clone()]
    }
}

impl<const N: usize> IndexFields for [&str; N] {
    fn into_fields(self) -> Vec<String> {
        self.iter().map(|field| field.to_string()).collect()
    }
}

impl IndexFields for &[&str] {
    fn into_fields(self) -> Vec<String> {
        self.iter().map(|field| field.to_string()).collect()
    }
}

/// Field names known only at run time, from a configuration say.
impl IndexFields for Vec<String> {
    fn into_fields(self) -> Vec<String> {
        self
    }
}

impl IndexFields for Vec<&str> {
    fn into_fields(self) -> Vec<String> {
        self.into_iter().map(str::to_string).collect()
    }
}

impl IndexFields for &[String] {
    fn into_fields(self) -> Vec<String> {
        self.to_vec()
    }
}
use std::marker::PhantomData;

/// The public-facing API. A `Collection` is a lightweight handle: a clone
/// of the `Database` handle plus a name, no borrow — so it can be stored,
/// cloned, and moved to another thread, and any number of them (different
/// `T`, different names) can be held side by side (SPEC §27). It takes
/// the database's lock only for the duration of each method call.
///
/// Both the untyped path (`Collection<Document>`, its own `impl` block
/// below) and the typed path (`Collection<T>` for any `T: Serialize +
/// DeserializeOwned`, right below) are real. The typed path converts `T`
/// to/from `Document` via `serde_bridge`, then delegates every actual
/// storage operation to a `Collection<Document>` built from the same
/// `db`/`name` — so the catalog/index/data-page logic exists in exactly
/// one place, not duplicated per `T`.
///
/// Every document is an object (SPEC §95), so `T` is a struct or a map:
/// a plain value, a sequence or a unit fails its first write with
/// `Error::NotAnObject`. A plain value goes into a one-field struct.
///
/// A `T` can carry its document's id (SPEC §59): every read fills the
/// field in, and an insert uses it if it's `Some`, or makes one:
///
/// ```
/// # use serde::{Deserialize, Serialize};
/// # use trunkdb::{Database, DocId, query::Filter};
/// #[derive(Serialize, Deserialize)]
/// struct Task {
///     #[serde(rename = "_id")]
///     id: Option<DocId>,
///     title: String,
/// }
/// # let dir = tempfile::tempdir().unwrap();
/// # let db = Database::open(dir.path().join("doc.trunkdb")).unwrap();
/// let tasks = db.collection::<Task>("tasks");
/// let id = tasks.insert(Task { id: None, title: "write it down".into() }).unwrap();
/// let found = tasks.find_one(Filter::new().eq("title", "write it down")).unwrap();
/// assert_eq!(found.unwrap().id, Some(id));
/// ```
pub struct Collection<T> {
    db: Database,
    name: String,
    /// The commit this handle reads, if it is a snapshot's (`View`, a
    /// cursor: SPEC §82); the last one, as of each call, if `None`. A
    /// handle with a commit never writes: nothing outside this crate can
    /// reach a write method on one.
    at: Option<Committed>,
    /// `fn() -> T`, not `T`: the handle holds no `T`, so whether it's
    /// `Send`/`Sync` shouldn't depend on whether `T` is — a plain
    /// `PhantomData<T>` would make it so.
    _marker: PhantomData<fn() -> T>,
}

/// By hand, not derived: `#[derive(Clone)]` would require `T: Clone`,
/// though only the handle is cloned, never a `T`.
impl<T> Clone for Collection<T> {
    fn clone(&self) -> Self {
        self.retyped()
    }
}

impl<T> Collection<T> {
    pub(crate) fn new(db: Database, name: impl Into<String>) -> Self {
        Self {
            db,
            name: name.into(),
            at: None,
            _marker: PhantomData,
        }
    }

    /// A handle that reads the collection as of `at`, every time (SPEC
    /// §82).
    pub(crate) fn pinned(db: Database, name: impl Into<String>, at: Committed) -> Self {
        Self {
            at: Some(at),
            ..Self::new(db, name)
        }
    }

    /// The same handle, for another type of document: same database,
    /// same collection, same commit if it reads one. Cloning `name` (a
    /// short `String`) and the handles (`Arc` counts) is cheap.
    fn retyped<U>(&self) -> Collection<U> {
        Collection {
            db: self.db.clone(),
            name: self.name.clone(),
            at: self.at.clone(),
            _marker: PhantomData,
        }
    }

    /// Access for a read: of this handle's commit if it has one, of the
    /// last one otherwise.
    fn read(&self) -> crate::Result<ReadGuard<'_>> {
        match &self.at {
            Some(at) => self.db.read_at(at),
            None => self.db.read(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The database this handle belongs to — `Batch` checks it, so an op
    /// can't target another database's collection by accident.
    pub(crate) fn db(&self) -> &Database {
        &self.db
    }

    /// Makes sure there's a secondary index on `fields` (SPEC §28): `true`
    /// if it was created now — from every document already stored, in
    /// one atomic batch — `false` if it already existed. Creates the
    /// collection if needed, so indexes can be declared up front. From
    /// then on every write keeps the index up to date, and `find` uses it
    /// for `Eq`/`Lt`/`Lte`/`Gt`/`Gte` conditions on its fields. The index
    /// persists; call this at startup.
    ///
    /// One field — `"age"`, a dotted path into nested objects like
    /// `"address.city"` (SPEC §31), or into array elements like
    /// `"tags[*]"` (SPEC §42) — or several, for a compound index:
    /// `["status", "created"]` (SPEC §43). A compound index serves `Eq`
    /// on its first fields and a range or a sort on the next one:
    /// `status == "Queued"`, sorted by `created`. `_id` is always indexed
    /// (the primary index) and is rejected here, and so are paths below
    /// it and paths with an empty part (`a..b`, `.a`).
    ///
    /// An existing index on the same fields with other options (unique,
    /// sparse) is an error, not a match — see `ensure_index_with`.
    pub fn ensure_index(&self, fields: impl IndexFields) -> crate::Result<bool> {
        self.ensure_index_with(fields, IndexOptions::new())
    }

    /// `ensure_index` with `options`: unique, sparse, or both.
    ///
    /// A unique index (SPEC §33) lets no two documents have equal values
    /// in the field — equal as a filter's `Eq` sees it, so `1` and `1.0`
    /// count as equal, `"a"` and `"A"` don't. Null and missing values are
    /// exempt: any number of documents may lack the field. On several
    /// fields, no two documents may be equal in all of them, and a
    /// document with a null in any is exempt (SPEC §43.4). A write that
    /// would break it fails with `Error::DuplicateValue`, and its whole
    /// batch is rolled back. Creating it over documents that already
    /// break it fails the same way, naming two of them, and creates
    /// nothing.
    ///
    /// A sparse index (SPEC §44) has no entry for a
    /// null or missing value — on several fields, for a document null in
    /// all of them — so an index on a field few documents have stays
    /// small. The price: `find` uses it only where the filter itself
    /// rules nulls out, with a comparison on one of its fields that null
    /// fails — `nick == "ada"`, `nick > "m"`, and for a sort by it also
    /// `nick != null`; never for `nick == null`, nor for a sort alone.
    ///
    /// ```
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let db = trunkdb::Database::open(dir.path().join("db.trunk")).unwrap();
    /// # let users = db.collection::<trunkdb::Document>("users");
    /// use trunkdb::IndexOptions;
    ///
    /// users.ensure_index_with("email", IndexOptions::new().unique())?;
    /// users.ensure_index_with("nick", IndexOptions::new().sparse())?;
    /// # Ok::<(), trunkdb::Error>(())
    /// ```
    ///
    /// An existing index on the same fields with other options is an
    /// error: drop it first, then call this.
    pub fn ensure_index_with(
        &self,
        fields: impl IndexFields,
        options: IndexOptions,
    ) -> crate::Result<bool> {
        let fields = fields.into_fields();
        let invalid = |message: &str| {
            crate::Error::from(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                message.to_string(),
            ))
        };
        if fields.is_empty() {
            return Err(invalid("an index needs at least one field"));
        }
        if fields.len() > key::MAX_COMPOUND_FIELDS {
            return Err(invalid(&format!(
                "a compound index has at most {} fields",
                key::MAX_COMPOUND_FIELDS
            )));
        }
        for (i, field) in fields.iter().enumerate() {
            // Each step a field name, then any number of `[*]` (SPEC §42).
            let names = field.split('.').map(|step| step.trim_end_matches("[*]"));
            if names.clone().next() == Some("_id") {
                return Err(primary_key_error());
            }
            if names.clone().any(str::is_empty) {
                return Err(invalid("an index path needs a name between every two dots"));
            }
            if names.clone().any(|name| name.contains(['[', ']'])) {
                return Err(invalid(
                    "in an index path, only `[*]`, right after a name, may use brackets",
                ));
            }
            if fields.len() > 1 && crate::query::is_multi(field) {
                return Err(invalid(
                    "a compound index can't be on array elements (`[*]`): one entry per \
                     document is what lets it sort (SPEC §43)",
                ));
            }
            if fields[..i].contains(field) {
                return Err(invalid(&format!("{field:?} is in the index twice")));
            }
        }
        self.db
            .transact(|catalog, store| build_index(catalog, store, &self.name, &fields, options))
    }

    /// Drops the secondary index on `fields` and frees its pages: `false`
    /// if there was none. `_id` is the primary key, which can't be
    /// dropped: an error, like `ensure_index` on it.
    pub fn drop_index(&self, fields: impl IndexFields) -> crate::Result<bool> {
        let fields = fields.into_fields();
        if fields
            .iter()
            .any(|f| f.split('.').next().map(|s| s.trim_end_matches("[*]")) == Some("_id"))
        {
            return Err(primary_key_error());
        }
        self.db.transact(|catalog, store| {
            let Some(index) = catalog.drop_index(store, &self.name, &fields)? else {
                return Ok(false);
            };
            BTreeIndex::new(index.root).free_all(store)?;
            Ok(true)
        })
    }

    /// This collection's secondary indexes, in creation order: each with
    /// its fields and options (SPEC §60).
    pub fn indexes(&self) -> crate::Result<Vec<IndexInfo>> {
        let state = self.read()?;
        Ok(state
            .catalog()
            .indexes(&self.name)
            .iter()
            .map(IndexMeta::info)
            .collect())
    }

    /// The names of `indexes`: a field, or a compound index's fields in
    /// parentheses, `(status, created)`. For tests.
    #[cfg(test)]
    pub(crate) fn index_names(&self) -> crate::Result<Vec<String>> {
        self.index_names_where(|_| true)
    }

    #[cfg(test)]
    pub(crate) fn unique_indexes(&self) -> crate::Result<Vec<String>> {
        self.index_names_where(|index| index.unique)
    }

    #[cfg(test)]
    pub(crate) fn sparse_indexes(&self) -> crate::Result<Vec<String>> {
        self.index_names_where(|index| index.sparse)
    }

    #[cfg(test)]
    fn index_names_where(&self, keep: impl Fn(&IndexMeta) -> bool) -> crate::Result<Vec<String>> {
        let state = self.read()?;
        Ok(state
            .catalog()
            .indexes(&self.name)
            .iter()
            .filter(|index| keep(index))
            .map(IndexMeta::name)
            .collect())
    }

    /// How `find` would run `filter`: a full scan, lookups by id in the
    /// primary index, or ranges of secondary indexes.
    pub fn explain(&self, filter: &Filter) -> crate::Result<QueryPlan> {
        let state = self.read()?;
        Ok(plan_for(filter, state.catalog().indexes(&self.name)))
    }

    /// Runs `filter` as `find` does and says how (SPEC §94): the plan,
    /// as `explain` names it; each index read, with the conditions that
    /// bound it; how the sort was done; and how many index entries and
    /// documents it read, how many matched and were returned, and how
    /// long it took. The documents are read and checked as `find` would,
    /// then dropped — for them too, run `find` and `explain_run` on one
    /// `View`. Where an index's entries decide the filter, its read says
    /// so (`IndexRead::covering`), and the documents they reject or a
    /// skip passes over aren't read (SPEC §100).
    ///
    /// It is `find` with its counters read out, not a second
    /// implementation: `find` counts the same on every call, and throws
    /// the counts away. `skip` and `limit` are part of it, since they
    /// change the plan (§34.2) and the cost (§72.3). Errors as `find`'s.
    ///
    /// ```
    /// # use serde::Serialize;
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb"))?;
    /// use trunkdb::query::{Filter, QueryPlan};
    ///
    /// #[derive(Serialize, serde::Deserialize)]
    /// struct Person { age: i64 }
    ///
    /// let people = db.collection::<Person>("people");
    /// people.ensure_index("age")?;
    /// for age in 0..100 {
    ///     people.insert(Person { age })?;
    /// }
    /// let adults = people.explain_run(Filter::new().gte("age", 18).lt("age", 65))?;
    /// assert_eq!(adults.plan, QueryPlan::Index { field: "age".into() });
    /// assert_eq!(adults.reads[0].bounds.len(), 2); // age >= 18, age < 65
    /// // The range holds age 65 too, which the check against the
    /// // filter leaves out (SPEC §28.3) — on the index entry, which
    /// // holds the age, so the document isn't read (SPEC §100).
    /// assert_eq!(adults.stats.keys_examined, 48);
    /// assert!(adults.reads[0].covering);
    /// assert_eq!(adults.stats.documents_examined, 47);
    /// assert_eq!(adults.stats.documents_returned, 47);
    ///
    /// // A condition on a field the index lacks: every document read.
    /// let some = people.explain_run(Filter::new().gte("age", 18).lt("age", 65).ne("name", "x"))?;
    /// assert!(!some.reads[0].covering);
    /// assert_eq!(some.stats.documents_examined, 48);
    /// # Ok::<(), trunkdb::Error>(())
    /// ```
    pub fn explain_run(&self, filter: Filter) -> crate::Result<Explained> {
        let started = std::time::Instant::now();
        let state = self.read()?;
        let at = state.snapshot();
        let indexes = at.catalog.indexes(&self.name);
        let plan = plan_for(&filter, indexes);
        let sort = match filter.sort.len() {
            0 => None,
            keys => {
                let from_index = filter.index_order(indexes).map_or(0, |order| order.served);
                Some(SortWork {
                    from_index,
                    in_memory: keys - from_index,
                })
            }
        };
        let trace = Trace::default();
        let found = find_in(at.catalog, &at.store, &self.name, &filter, true, &trace)?;
        Ok(trace.explained(plan, sort, found.len(), started.elapsed()))
    }
}

impl<T> Collection<T>
where
    T: Serialize + DeserializeOwned,
{
    /// A fresh, equally lightweight `Collection<Document>` handle for the
    /// same collection — cloning `name` (a short `String`) and the
    /// database handle (an `Arc` count) is cheap.
    fn as_document(&self) -> Collection<Document> {
        self.retyped()
    }

    pub fn insert(&self, doc: T) -> crate::Result<DocId> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().insert(document)
    }

    pub fn get(&self, id: &DocId) -> crate::Result<Option<T>> {
        match self.as_document().stored(id)? {
            Some(stored) => Ok(Some(stored.deserialize()?)),
            None => Ok(None),
        }
    }

    pub fn update(&self, id: &DocId, doc: T) -> crate::Result<bool> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().update(id, document)
    }

    pub fn delete(&self, id: &DocId) -> crate::Result<bool> {
        self.as_document().delete(id)
    }

    /// See the untyped `update_many`: `change` gets each match as a `T`.
    /// A document that doesn't convert to `T` fails the whole batch.
    /// Whether a document changed is judged after the round trip through
    /// `T`, so one written before a field was added to `T` counts as
    /// changed — it gets the field.
    pub fn update_many(
        &self,
        filter: Filter,
        mut change: impl FnMut(&mut T),
    ) -> crate::Result<usize> {
        self.as_document().update_matching(filter, |_id, doc| {
            let mut value: T = from_document(doc)?;
            change(&mut value);
            Ok(crate::serde_bridge::to_document(&value)?)
        })
    }

    /// See the untyped `update_fields`. Each document the update changes
    /// must still convert to `T`, or the whole batch fails: a field set
    /// to a value of the wrong type is caught here, not by the next read.
    pub fn update_fields(&self, filter: Filter, update: &Update) -> crate::Result<usize> {
        self.as_document()
            .update_fields_then(filter, update, |doc| {
                from_document::<T>(doc.clone()).map(drop)
            })
    }

    /// See the untyped `delete_many`. No document is converted to `T`.
    pub fn delete_many(&self, filter: Filter) -> crate::Result<usize> {
        self.as_document().delete_many(filter)
    }

    pub fn find(&self, filter: Filter) -> crate::Result<Vec<T>> {
        self.as_document()
            .matches_found(&filter, false)?
            .iter()
            .map(|found| found.stored.deserialize())
            .collect()
    }

    /// The first match — see the untyped `find_one`.
    pub fn find_one(&self, filter: Filter) -> crate::Result<Option<T>> {
        self.cursor(first_only(filter))?.next().transpose()
    }

    /// How many documents match — see the untyped `count`. No document is
    /// converted to `T`.
    pub fn count(&self, filter: Filter) -> crate::Result<usize> {
        self.as_document().count(filter)
    }

    /// A streaming `find` — see `Cursor`.
    pub fn cursor(&self, filter: Filter) -> crate::Result<Cursor<T>> {
        self.as_document()
            .cursor_converting(filter, false, |found| found.stored.deserialize())
    }

    /// Replaces the one document matching `filter`, or inserts `doc` if
    /// none does — see the untyped `upsert`.
    pub fn upsert(&self, filter: Filter, doc: T) -> crate::Result<Upserted> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().upsert(filter, document)
    }
}

/// The id an insert stores `doc` under (SPEC §59): the id in its `_id`
/// field, if it has one and it isn't `DocId::NIL`; otherwise a new one.
/// Any other `_id` (a string, `null`) is overwritten, as before.
pub(crate) fn id_for_insert(db: &Database, doc: &Document) -> DocId {
    match doc {
        Document::Object(map) => match map.get("_id") {
            Some(Document::Id(id)) if *id != DocId::NIL => *id,
            _ => db.id_gen().generate(),
        },
        _ => db.id_gen().generate(),
    }
}

/// The serde bridge's `from_document`, with the crate's error type — as a
/// plain function, so `Cursor` can hold it as a `fn` pointer.
fn from_document<T: DeserializeOwned>(doc: Document) -> crate::Result<T> {
    Ok(crate::serde_bridge::from_document(doc)?)
}

/// What `upsert` did, and to which document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upserted {
    /// Nothing matched; the document was inserted with this new id.
    Inserted(DocId),
    /// One document matched and was replaced; it keeps its id.
    Updated(DocId),
}

impl Upserted {
    pub fn id(&self) -> DocId {
        match self {
            Upserted::Inserted(id) | Upserted::Updated(id) => *id,
        }
    }
}

impl Collection<Document> {
    /// A single-op write is just a one-op batch — delegates to
    /// `Database::write_batch` so both paths share one stage/apply/log/
    /// write-back/checkpoint protocol (SPEC §19.3) instead of two copies.
    fn write(&self, op: WriteOp) -> crate::Result<()> {
        self.db.write_batch(vec![op])
    }

    /// Inserts `doc`, under its own `_id` if it has one (SPEC §59), and
    /// returns the id it's stored under.
    pub fn insert(&self, doc: Document) -> crate::Result<DocId> {
        let id = id_for_insert(&self.db, &doc);
        self.write(WriteOp::Insert(self.name.clone(), id, doc))?;
        Ok(id)
    }

    pub fn get(&self, id: &DocId) -> crate::Result<Option<Document>> {
        match self.stored(id)? {
            Some(stored) => Ok(Some(stored.decode()?)),
            None => Ok(None),
        }
    }

    /// The document `id` as it is stored, for `get` to decode as a
    /// `Document` or a `T` (SPEC §99).
    fn stored(&self, id: &DocId) -> crate::Result<Option<Stored>> {
        let state = self.read()?;
        let at = state.snapshot();
        let Some(meta) = at.catalog.get(&self.name) else {
            return Ok(None); // collection doesn't exist yet, so neither does the document
        };

        let index = BTreeIndex::new(meta.index_root);
        let Some(loc) = index.lookup(&at.store, &key::primary(*id))? else {
            return Ok(None);
        };
        Ok(Some(data::Records::new(&at.store).stored(loc)?))
    }

    /// `false` if there's no document `id` — for a single op that's an
    /// ordinary answer, not an error (unlike inside a batch, where it
    /// fails the batch: `Error::NotFound`).
    pub fn update(&self, id: &DocId, doc: Document) -> crate::Result<bool> {
        found(self.write(WriteOp::Update(self.name.clone(), *id, doc)))
    }

    /// `false` if there's no document `id`, as for `update`.
    pub fn delete(&self, id: &DocId) -> crate::Result<bool> {
        found(self.write(WriteOp::Delete(self.name.clone(), *id)))
    }

    /// Changes the documents `find(filter)` would return — `sort`, `skip`
    /// and `limit` included — by calling `change` on each, in that order, and
    /// writes the ones it changed; returns how many (SPEC §38). One batch
    /// under the writer's lock, like `delete_many`: all changes land or none
    /// — a unique index refusing one (§33) rolls back every one. An `_id`
    /// can't be changed: whatever `change` puts there, the document keeps
    /// its own.
    ///
    /// `change` runs while the database is locked for writing: it must
    /// not use this database itself, which would deadlock.
    pub fn update_many(
        &self,
        filter: Filter,
        mut change: impl FnMut(&mut Document),
    ) -> crate::Result<usize> {
        self.update_matching(filter, |_id, mut doc| {
            change(&mut doc);
            Ok(doc)
        })
    }

    /// Changes the documents `find(filter)` would return by `update`'s
    /// operators, `set`, `unset` and `inc` on dotted paths, and returns
    /// how many changed (SPEC §68). Otherwise as `update_many`: one batch,
    /// all or nothing, `sort`, `skip` and `limit` included, unchanged matches
    /// neither written nor counted.
    ///
    /// ```no_run
    /// # fn main() -> trunkdb::Result<()> {
    /// use trunkdb::{Database, Document, query::{Filter, Update}};
    ///
    /// let db = Database::open("app.trunkdb")?;
    /// let jobs = db.collection::<Document>("jobs");
    /// let started = jobs.update_fields(
    ///     Filter::new().eq("status", "Queued"),
    ///     &Update::new().set("status", "Running").inc("tries", 1).unset("error"),
    /// )?;
    /// # let _ = started;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// An update that names `_id`, uses brackets or has an empty step in
    /// a path, or `inc`s by something that isn't a number, is refused
    /// before anything is read (`ErrorKind::InvalidInput`). One that
    /// can't be made to a document found, an `inc` of a string, a `set`
    /// through a number, fails the batch with `Error::Update`.
    pub fn update_fields(&self, filter: Filter, update: &Update) -> crate::Result<usize> {
        self.update_fields_then(filter, update, |_doc| Ok(()))
    }

    /// `update_fields`, with `accept` to refuse a changed document: the
    /// typed path's check that it still converts to `T`.
    fn update_fields_then(
        &self,
        filter: Filter,
        update: &Update,
        mut accept: impl FnMut(&Document) -> crate::Result<()>,
    ) -> crate::Result<usize> {
        update.check().map_err(|message| {
            crate::Error::from(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                message,
            ))
        })?;
        self.update_matching(filter, |id, mut doc| {
            update
                .apply(&mut doc)
                .map_err(|message| crate::Error::Update {
                    collection: self.name.clone(),
                    id,
                    message,
                })?;
            accept(&doc)?;
            Ok(doc)
        })
    }

    /// `update_many` and `update_fields` for both paths: `change` maps
    /// each document found to its new version, or fails the whole batch.
    fn update_matching(
        &self,
        filter: Filter,
        mut change: impl FnMut(DocId, Document) -> crate::Result<Document>,
    ) -> crate::Result<usize> {
        self.db.transact(|catalog, store| {
            let found = find_in(catalog, store, &self.name, &filter, true, &Trace::default())?;
            let mut changed = 0;
            for found in found {
                let (id, old) = (found.stored.id(), found.decode()?);
                let new = with_id(change(id, old.clone())?, id);
                // By encoding, not `==`: a NaN isn't equal to itself, and
                // a document holding one would never count as unchanged.
                if encode_document(&new) == encode_document(&old) {
                    continue; // nothing to write
                }
                apply_write_op(catalog, store, &WriteOp::Update(self.name.clone(), id, new))?;
                changed += 1;
            }
            Ok(changed)
        })
    }

    /// Deletes exactly the documents `find(filter)` would return — `sort`,
    /// `skip` and `limit` included, so "the oldest 100" works — and says how many
    /// (SPEC §37). One batch under the writer's lock, like `upsert`: all of
    /// them go or none, and no write lands between the lookup and the
    /// deletes. A collection that doesn't exist has nothing to delete and
    /// isn't created.
    pub fn delete_many(&self, filter: Filter) -> crate::Result<usize> {
        self.db.transact(|catalog, store| {
            let doomed = find_in(
                catalog,
                store,
                &self.name,
                &filter,
                false,
                &Trace::default(),
            )?;
            for found in &doomed {
                let op = WriteOp::Delete(self.name.clone(), found.stored.id());
                apply_write_op(catalog, store, &op)?;
            }
            Ok(doomed.len())
        })
    }

    /// Every match with the id of its cell: what `find` and a sorted
    /// `cursor` read.
    fn matches_with_ids(&self, filter: &Filter) -> crate::Result<Vec<(DocId, Document)>> {
        self.matches_found(filter, true)?
            .into_iter()
            .map(|found| Ok((found.stored.id(), found.decode()?)))
            .collect()
    }

    /// Every match, in `find`'s order, for the caller to decode as a
    /// `Document` or a `T` (SPEC §99.3) — `whole_from_index` as in
    /// `find_in`.
    fn matches_found(&self, filter: &Filter, whole_from_index: bool) -> crate::Result<Vec<Found>> {
        let state = self.read()?;
        let at = state.snapshot();
        let trace = Trace::default();
        find_in(
            at.catalog,
            &at.store,
            &self.name,
            filter,
            whole_from_index,
            &trace,
        )
    }

    /// Every document matching `filter`. An `Object` document carries its
    /// id as `_id` (SPEC §18, §59); a document of another kind has no
    /// field to carry it, so its id is only what `insert` returned (§74).
    ///
    /// Reads only the documents in one secondary index's range if the
    /// filter has a condition an index can answer (see `explain`), every
    /// document otherwise. Either way each candidate is checked against
    /// the whole filter — an index range may hold a few documents that
    /// don't match (SPEC §28.3). Without a `sort`, the order of the
    /// results is unspecified.
    ///
    /// With a `sort` and a `limit`, and an index on the sort field, it
    /// reads that index in order instead and stops after `skip` plus
    /// `limit` matches (SPEC §34.2, §72). Sorted by `_id`, that is the
    /// primary index, and `gt("_id", last)` starts it after `last`
    /// (SPEC §96). Equal sort values come in id order either way.
    pub fn find(&self, filter: Filter) -> crate::Result<Vec<Document>> {
        Ok(self
            .matches_with_ids(&filter)?
            .into_iter()
            .map(|(_id, doc)| doc)
            .collect())
    }

    /// The first match, reading no further than it: the first in `sort`
    /// order if the filter has one, any match otherwise. `None` if nothing
    /// matches. With a `sort` on an indexed field, or on `_id`, this reads
    /// one index entry's worth of documents, not every match (SPEC §34.2,
    /// §96).
    pub fn find_one(&self, filter: Filter) -> crate::Result<Option<Document>> {
        self.cursor(first_only(filter))?.next().transpose()
    }

    /// How many documents `find(filter)` would return: those matching its
    /// conditions, less its `skip`, at most its `limit`; `sort` is
    /// irrelevant. For the total behind a page, count without `skip` and
    /// `limit` (SPEC §72). Without conditions it only counts the primary
    /// index's entries — no document is read.
    pub fn count(&self, filter: Filter) -> crate::Result<usize> {
        let state = self.read()?;
        let at = state.snapshot();
        let trace = Trace::default();
        let streamed = count_in(at.catalog, &at.store, &self.name, &filter)?;
        let count = if let Some(count) = streamed {
            count
        } else {
            let names = filter.fields_with(false);
            read_candidates(
                at.catalog,
                &at.store,
                &self.name,
                &filter,
                names.as_deref(),
                false,
                &trace,
            )?
            .iter()
            .filter(|(source, fields)| source.decided() || filter.matches(fields))
            .count()
        };
        let count = count.saturating_sub(filter.skip);
        Ok(filter.limit.map_or(count, |limit| count.min(limit)))
    }

    /// A streaming `find`: yields matches one at a time, holding only
    /// where they are in memory, and no lock. It shows the collection as
    /// it was when the cursor was made, and keeps that moment until it's
    /// dropped — see `Cursor`.
    pub fn cursor(&self, filter: Filter) -> crate::Result<Cursor<Document>> {
        self.cursor_converting(filter, true, |found| Ok(found.decode()?))
    }

    /// `cursor`, decoding each match with `convert` — into a `Document`,
    /// or a `T` for the typed path (SPEC §99.3).
    fn cursor_converting<T>(
        &self,
        filter: Filter,
        whole_from_index: bool,
        convert: fn(Found) -> crate::Result<T>,
    ) -> crate::Result<Cursor<T>> {
        if !filter.sort.is_empty() {
            let results = self.matches_found(&filter, whole_from_index)?;
            let converted = results
                .into_iter()
                .map(convert)
                .collect::<crate::Result<Vec<_>>>()?;
            return Ok(Cursor::collected(converted));
        }
        // One commit for the candidates and for every document read
        // after them (SPEC §82): kept by the cursor, as a snapshot is.
        let kept = match &self.at {
            Some(_) => self.clone(),
            None => Collection::pinned(self.db.clone(), self.name.clone(), self.db.pin()?),
        };
        let state = kept.read()?;
        let at = state.snapshot();
        let (entries, indexed) = candidate_entries(
            at.catalog,
            &at.store,
            &self.name,
            &filter,
            &Trace::default(),
        )?;
        // Where the entries decide (SPEC §100), a rejected one is left
        // out, and a match is kept as decided: passed over by a skip
        // unread.
        let mut decides = indexed.covering(&filter, false).map(|c| c.decider(&filter));
        let candidates = entries
            .into_iter()
            .filter_map(
                |(key, loc)| match decides.as_mut().and_then(|d| d.decide(&key)) {
                    Some(true) => Some((loc, true)),
                    Some(false) => None,
                    None => Some((loc, false)),
                },
            )
            .collect();
        drop(state);
        Ok(Cursor::streaming(kept, candidates, filter, convert))
    }

    /// The document at `loc`, as this handle's commit has it: for a
    /// cursor, which took `loc` from an index of the same commit.
    pub(crate) fn stored_at(&self, loc: RecordLocation) -> crate::Result<Stored> {
        debug_assert!(
            self.at.is_some(),
            "a location is only good within one commit"
        );
        let state = self.read()?;
        Ok(data::Records::new(&state.snapshot().store).stored(loc)?)
    }

    /// Replaces the one document matching `filter`'s conditions (keeping
    /// its id), or inserts `doc` with a new id if none matches;
    /// `Error::MultipleMatches` if several do. `sort`, `skip` and `limit`
    /// don't matter. Atomic: the lookup and the write happen under one write
    /// lock, so two threads upserting the same key can't both insert —
    /// with a secondary index on the key's field, the lookup is cheap
    /// (SPEC §29.3).
    pub fn upsert(&self, filter: Filter, doc: Document) -> crate::Result<Upserted> {
        let id_if_new = id_for_insert(&self.db, &doc);
        self.db.transact(|catalog, store| {
            let names = filter.fields_with(false);
            let trace = Trace::default();
            let matches: Vec<DocId> = read_candidates(
                catalog,
                store,
                &self.name,
                &filter,
                names.as_deref(),
                false,
                &trace,
            )?
            .into_iter()
            .filter(|(source, fields)| source.decided() || filter.matches(fields))
            .map(|(source, _fields)| source.id())
            .collect();
            let (op, outcome) = match matches[..] {
                [] => (
                    WriteOp::Insert(self.name.clone(), id_if_new, doc),
                    Upserted::Inserted(id_if_new),
                ),
                [id] => (
                    WriteOp::Update(self.name.clone(), id, doc),
                    Upserted::Updated(id),
                ),
                _ => {
                    return Err(crate::Error::MultipleMatches {
                        collection: self.name.clone(),
                        count: matches.len(),
                    });
                }
            };
            apply_write_op(catalog, store, &op)?;
            Ok(outcome)
        })
    }
}

// Each match with its id, for tests that need the id of a document they
// didn't insert themselves, or of one that isn't an object. The public
// `find_with_ids` and `find_one_with_id` were removed before 1.0 (SPEC §74).
#[cfg(test)]
impl<T: Serialize + DeserializeOwned> Collection<T> {
    pub(crate) fn find_with_ids(&self, filter: Filter) -> crate::Result<Vec<(DocId, T)>> {
        self.as_document()
            .matches_found(&filter, false)?
            .iter()
            .map(|found| Ok((found.stored.id(), found.stored.deserialize()?)))
            .collect()
    }

    pub(crate) fn find_one_with_id(&self, filter: Filter) -> crate::Result<Option<(DocId, T)>> {
        Ok(self.find_with_ids(first_only(filter))?.into_iter().next())
    }
}

#[cfg(test)]
impl Collection<Document> {
    pub(crate) fn find_with_ids(&self, filter: Filter) -> crate::Result<Vec<(DocId, Document)>> {
        self.matches_with_ids(&filter)
    }

    pub(crate) fn find_one_with_id(
        &self,
        filter: Filter,
    ) -> crate::Result<Option<(DocId, Document)>> {
        Ok(self.find_with_ids(first_only(filter))?.into_iter().next())
    }
}

/// The plan `find` follows for `filter` among a collection's `indexes`
/// — what `explain` says.
fn plan_for(filter: &Filter, indexes: &[IndexMeta]) -> QueryPlan {
    if filter.ids().is_some() {
        return QueryPlan::ById;
    }
    if let Some(order) = filter.index_order(indexes) {
        return QueryPlan::IndexOrder {
            field: order.name(),
        };
    }
    match filter.index_ranges(indexes) {
        None => QueryPlan::Scan,
        Some(ranges) => match &ranges[..] {
            [range] => QueryPlan::Index {
                field: range.index.name(),
            },
            _ => QueryPlan::IndexUnion {
                fields: ranges.iter().map(|range| range.index.name()).collect(),
            },
        },
    }
}

/// Every match with its id, against a catalog and store the caller
/// has locked — for a read, or inside a write batch (`delete_many`).
/// What it read and matched goes to `trace` (SPEC §94.3).
fn find_in(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
    whole_from_index: bool,
    trace: &Trace,
) -> crate::Result<Vec<Found>> {
    let indexes = catalog.indexes(collection);
    let mut records = data::Records::new(store);
    if let Some(order) = filter.index_order(indexes) {
        // The skipped matches are passed over by the walk (SPEC §72),
        // unread where their entries decided (§100), and not kept (§101).
        let found = walk_in_index_order(catalog, store, collection, filter, order, trace)?;
        return found
            .into_iter()
            .map(|(source, _fields)| Ok(Found::stored(source.read(&mut records, trace)?)))
            .collect();
    }
    // Candidates from an index range or by id nearly all match: for a
    // caller that wants whole documents, each is decoded whole once and
    // checked on that, rather than on its fields and then again whole
    // (SPEC §99.2). A scan's are mostly rejected, and checked on fields.
    let whole =
        whole_from_index && (filter.ids().is_some() || filter.index_ranges(indexes).is_some());
    let names = match whole {
        true => None,
        false => filter.fields(),
    };
    let sort = !filter.sort.is_empty();
    let mut matches: Vec<Candidate> = read_candidates(
        catalog,
        store,
        collection,
        filter,
        names.as_deref(),
        sort,
        trace,
    )?
    .into_iter()
    .filter(|(source, fields)| source.decided() || filter.matches(fields))
    .collect();
    trace.matched(matches.len());
    if sort {
        // So equal sort values end up in id order, as they do when read
        // from an index (the sort is stable).
        matches.sort_unstable_by_key(|(source, _fields)| source.id());
    }
    let arranged = filter.arrange(matches, |(_source, fields)| fields);
    arranged
        .into_iter()
        .map(|(source, fields)| {
            // Only a document read was decoded whole, not an entry's.
            let whole = (whole && matches!(source, Source::Read(_))).then_some(fields);
            let stored = source.read(&mut records, trace)?;
            Ok(Found { stored, whole })
        })
        .collect()
}

/// A match of `find_in`, not decoded yet — or, where it was decoded whole
/// to be checked, with that document (`whole`).
pub(crate) struct Found {
    pub(crate) stored: Stored,
    whole: Option<Document>,
}

impl From<Stored> for Found {
    fn from(stored: Stored) -> Self {
        Found::stored(stored)
    }
}

impl Found {
    fn stored(stored: Stored) -> Self {
        Found {
            stored,
            whole: None,
        }
    }

    /// The whole document, decoded now unless it already was.
    pub(crate) fn decode(self) -> std::io::Result<Document> {
        match self.whole {
            Some(document) => Ok(document),
            None => self.stored.decode(),
        }
    }
}

/// A candidate as `find` checks it (SPEC §99.2): the document as it is
/// stored, and the fields its filter and sort name, decoded
/// (`Filter::fields`) — or, where its index entry decided (SPEC §100),
/// where it is and its entry's document. The whole document is decoded
/// only for what is handed out (§99.3).
type Candidate = (Source, Document);

/// Where a candidate's document is: read, or not yet (SPEC §100).
enum Source {
    Read(Stored),
    /// Its index entry decided: its id and where it is, read only if
    /// handed out.
    Entry(DocId, RecordLocation),
}

impl Source {
    /// Its entry decided it matches: no need to check it again.
    fn decided(&self) -> bool {
        matches!(self, Source::Entry(..))
    }

    fn id(&self) -> DocId {
        match self {
            Source::Read(stored) => stored.id(),
            Source::Entry(id, _) => *id,
        }
    }

    /// The document as it is stored, read now if it wasn't.
    fn read(self, records: &mut data::Records, trace: &Trace) -> std::io::Result<Stored> {
        match self {
            Source::Read(stored) => Ok(stored),
            Source::Entry(_, loc) => {
                trace.examined();
                records.stored(loc)
            }
        }
    }
}

/// The index entries `find` has to look at for `filter`: the primary
/// index's entries for the ids it names (SPEC §61), else the secondary
/// index ranges its conditions allow (SPEC §28.4, §36.3), otherwise the
/// whole primary index. Each entry's key ends with its document's id, and
/// each document comes once, even if several ranges hold it (an OR
/// whose branches overlap) or one range holds it several times (a
/// multikey index, SPEC §42.2). With them, which index they come from,
/// for deciding by entry (SPEC §100).
fn candidate_entries<'a>(
    catalog: &'a Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
    trace: &Trace,
) -> std::io::Result<(Vec<Entry>, Indexed<'a>)> {
    let Some(meta) = catalog.get(collection) else {
        return Ok((Vec::new(), Indexed::Primary));
    };
    let mut reads = Vec::new();
    let (entries, indexed) = if let Some(ids) = filter.ids() {
        let by_id = |id: &DocId| Condition::id(*id);
        let bounds = match &ids[..] {
            [id] => vec![by_id(id)],
            _ => vec![Condition::any(ids.iter().map(by_id))],
        };
        let read = trace.read(crate::document::ID_FIELD.to_string(), bounds, false);
        trace.entries(read, ids.len());
        reads.push(read);
        let primary = BTreeIndex::new(meta.index_root);
        let mut entries = Vec::new();
        for id in ids {
            let key = key::primary(id);
            if let Some(loc) = primary.lookup(store, &key)? {
                entries.push((key, loc));
            }
        }
        (entries, Indexed::Primary)
    } else {
        match filter.index_ranges(catalog.indexes(collection)) {
            None => {
                let read = trace.read(crate::document::ID_FIELD.to_string(), Vec::new(), false);
                let entries = BTreeIndex::new(meta.index_root).scan(store)?;
                trace.entries(read, entries.len());
                reads.push(read);
                (entries, Indexed::Primary)
            }
            Some(ranges) => {
                // An OR of no branches has no ranges, and no entries.
                let first = ranges.first().map(|range| range.index);
                let indexed = match first {
                    Some(first) if ranges.iter().all(|r| std::ptr::eq(r.index, first)) => {
                        Indexed::One(first)
                    }
                    _ => Indexed::Several,
                };
                // One range of an index with one key per document holds
                // each once; several ranges, or a multikey index, may
                // hold one several times.
                let multikey = ranges[0]
                    .index
                    .fields
                    .iter()
                    .any(|f| crate::query::is_multi(f));
                let once = ranges.len() == 1 && !multikey;
                let mut seen = std::collections::HashSet::new();
                let mut entries = Vec::new();
                for range in ranges {
                    let read = trace.read(range.index.name(), range.by, false);
                    let found = BTreeIndex::new(range.index.root).range(store, &range.range)?;
                    trace.entries(read, found.len());
                    reads.push(read);
                    if once {
                        entries = found;
                        continue;
                    }
                    for entry in found {
                        if seen.insert(key::doc_id(&entry.0)) {
                            entries.push(entry);
                        }
                    }
                }
                (entries, indexed)
            }
        }
    };
    if indexed.covering(filter, false).is_some() {
        reads.into_iter().for_each(|read| trace.covering(read));
    }
    Ok((entries, indexed))
}

/// An index entry: its key, and where its document is.
type Entry = (Vec<u8>, RecordLocation);

/// Which index `candidate_entries` read.
#[derive(Clone, Copy)]
enum Indexed<'a> {
    Primary,
    One(&'a IndexMeta),
    /// Ranges of several, for an OR (SPEC §36.3): no one index holds
    /// every field the filter names, so their entries decide nothing.
    Several,
}

impl<'a> Indexed<'a> {
    /// `Covering::new` for the index read.
    fn covering(self, filter: &Filter, sort: bool) -> Option<Covering<'a>> {
        match self {
            Indexed::Primary => Covering::new(filter, None, sort),
            Indexed::One(index) => Covering::new(filter, Some(index), sort),
            Indexed::Several => None,
        }
    }
}

/// `find` for a filter `Filter::index_order` chose (SPEC §34.2): walks the
/// index in sort order (`BTreeIndex::walk`, SPEC §49), checks each
/// document against the whole filter, and stops as soon as `limit`
/// match — the entries and documents after that are never read. Already
/// filtered, sorted, skipped and limited.
///
/// Entries are read in groups that share the values of the served sort
/// keys (`OrderedRead::served`). A group comes in id order, which is sort
/// order when those values are all equal (`key::part_is_exact`) and no
/// sort key is left for memory; otherwise the group is read whole and
/// sorted by every key (SPEC §47) — the rare group whose values may
/// differ (huge numbers, cut strings), or the documents equal in the
/// served keys, for the keys after them. Values no one-field index holds
/// (§34.1: arrays, NaN, ...) sort after everything: if the index runs
/// out before the limit and no range condition on the sort field ruled
/// them out, a scan finds them.
///
/// A compound index (SPEC §43.3) is read within the values its first
/// fields are fixed to — by id within a group, which fields after the
/// served ones would otherwise order. It holds every document, the
/// unordered values too, under a tag of their own that sorts last:
/// right for an ascending walk, and a descending one holds those groups
/// back until it has passed what they must follow (`Held`).
///
/// The primary index, for a sort by `_id` (SPEC §96), leaves most of
/// that idle: each key is one id, a group of its own and exact, and
/// every document has one.
///
/// Each document is checked on the fields the filter and sort name, and
/// handed out undecoded (SPEC §99). Where the index covers the filter
/// (SPEC §100), an entry of an exact group decides without its document,
/// which is read only if handed out.
///
/// Where the walk's order is the order handed out — forward, or by id;
/// every served value fixed or served, so that ties are in key order;
/// nothing sorted in memory or held — an exact group is never collected:
/// each entry is handed out as it's walked, its key lent from its leaf
/// (SPEC §101.5). The matches a skip passes over are counted, not kept,
/// and a leaf whose ends decide it is passed whole (§101.3).
fn walk_in_index_order(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
    order: OrderedRead,
    trace: &Trace,
) -> crate::Result<Vec<Candidate>> {
    let (Some(first), Some(limit)) = (filter.sort.first(), filter.limit) else {
        unreachable!("index_order needs a sort and a limit");
    };
    let mut page = Page {
        results: Vec::new(),
        skipped: 0,
        skip: filter.skip,
        limit,
    };
    if limit == 0 {
        return Ok(page.results);
    }
    let name = order.name();
    let (root, fields, compound) = match order.index {
        Some(index) => (index.root, index.fields.len(), index.is_compound()),
        None => match catalog.get(collection) {
            Some(meta) => (meta.index_root, 1, false),
            None => return Ok(page.results),
        },
    };
    let primary = order.index.is_none();
    let unordered_can_match = order.range.is_none() && !compound && !primary;
    let backward = first.order == SortOrder::Desc;
    let walk = trace.read(name, order.by, backward);
    let mut entries = BTreeIndex::new(root).walk(
        store,
        order.range.unwrap_or_else(KeyRange::everything),
        backward,
    );
    let (at, end) = (order.fixed, order.fixed + order.served);
    let spans = Spans {
        at,
        end,
        fields,
        primary,
    };
    let in_memory = filter.sort.len() > order.served;
    // The primary index gives the whole order: every key exact, nothing
    // unordered (SPEC §96), so only the conditions' fields are looked at.
    let names = filter.fields_with(!primary || in_memory);
    let mut decider = Covering::new(filter, order.index, false).map(|c| c.decider(filter));
    if decider.is_some() {
        trace.covering(walk);
    }
    let mut records = data::Records::new(store);
    let mut read = |loc: &RecordLocation| -> std::io::Result<Candidate> {
        trace.examined();
        let stored = records.stored(*loc)?;
        let fields = stored.fields(names.as_deref())?;
        Ok((Source::Read(stored), fields))
    };
    // One entry of an exact group, handed out in order: decided by its
    // entry where it can be, else read and checked. `true` once the page
    // is full.
    let one = |key: &[u8],
               loc: RecordLocation,
               decider: &mut Option<Decider>,
               read: &mut dyn FnMut(&RecordLocation) -> std::io::Result<Candidate>,
               page: &mut Page|
     -> crate::Result<bool> {
        let (source, doc) = match decider.as_mut().and_then(|d| d.decide(key)) {
            Some(false) => return Ok(false),
            Some(true) => (Source::Entry(key::doc_id(key), loc), Document::Null),
            None => read(&loc)?,
        };
        if !source.decided() && !filter.matches(&doc) {
            return Ok(false);
        }
        trace.matched(1);
        Ok(page.push((source, doc)))
    };
    // Hands out one collected group's matches; `true` once the page is
    // full.
    let take = |group: &mut Group,
                decider: &mut Option<Decider>,
                read: &mut dyn FnMut(&RecordLocation) -> std::io::Result<Candidate>,
                page: &mut Page|
     -> crate::Result<bool> {
        // Ties in id order: fields after the served ones would order them
        // otherwise, and a backward walk hands them out last id first.
        group.sort_by_key(|(key, _)| key::doc_id(key));
        if spans.exact(&group[0].0) && !in_memory {
            for (key, loc) in group.iter() {
                if one(key, *loc, decider, read, page)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        let docs = group
            .iter()
            .map(|(_key, loc)| read(loc))
            .collect::<Result<Vec<_>, _>>()?;
        let unlimited = Filter {
            skip: 0,
            limit: None,
            ..filter.clone()
        };
        let found = unlimited.apply_to(docs, |(_id, doc)| doc);
        trace.matched(found.len());
        for found in found {
            if page.push(found) {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let mut held = Held::new(backward && compound, spans, first.order);
    let direct =
        (!backward || primary) && !in_memory && !held.active && (primary || spans.end == fields);
    // The group being collected, and the served bytes it shares.
    let mut group: Group = Vec::new();
    let mut shared: Vec<u8> = Vec::new();
    loop {
        if direct && group.is_empty() {
            // A leaf passed whole: its entries all rejected, or all
            // matches the skip passes over — where the group its last
            // entry is in, which may go on past it, is exact.
            let whole = entries.whole_leaf()?.map(|leaf| {
                let decided = decider
                    .as_mut()
                    .and_then(|d| d.decide_all(leaf.lowest, leaf.highest));
                (decided, leaf.entries, spans.exact(leaf.highest))
            });
            let passed = match whole {
                Some((Some(false), n, _)) => Some(n),
                Some((Some(true), n, true)) if page.skipped + n <= page.skip => {
                    trace.matched(n);
                    page.skipped += n;
                    Some(n)
                }
                _ => None,
            };
            if let Some(n) = passed {
                trace.entries(walk, n);
                entries.pass_leaf();
                continue;
            }
        }
        let entry = entries.step().transpose()?;
        trace.entries(walk, usize::from(entry.is_some()));
        let ends_group =
            !group.is_empty() && entry.is_none_or(|(key, _)| spans.group(key) != shared.as_slice());
        if ends_group {
            let next = entry.map(|(key, _)| key);
            if held.active {
                for mut ready in held.pass(std::mem::take(&mut group), next) {
                    if take(&mut ready, &mut decider, &mut read, &mut page)? {
                        return Ok(page.results);
                    }
                }
            } else {
                if take(&mut group, &mut decider, &mut read, &mut page)? {
                    return Ok(page.results);
                }
                group.clear();
            }
        }
        let Some((key, loc)) = entry else {
            break;
        };
        if direct && group.is_empty() && spans.exact(key) {
            if one(key, loc, &mut decider, &mut read, &mut page)? {
                return Ok(page.results);
            }
            continue;
        }
        if group.is_empty() {
            shared.clear();
            shared.extend_from_slice(spans.group(key));
        }
        #[cfg(test)]
        crate::index::KEYS_COPIED.with(|n| n.set(n.get() + 1));
        group.push((key.to_vec(), loc));
        // An id is a group of its own (SPEC §96): handed out without
        // reading the next key to see the group end.
        if primary {
            if take(&mut group, &mut decider, &mut read, &mut page)? {
                return Ok(page.results);
            }
            group.clear();
        }
    }
    if unordered_can_match {
        let meta = catalog
            .get(collection)
            .expect("an indexed collection exists");
        let mut unordered = Vec::new();
        let primary = BTreeIndex::new(meta.index_root).scan(store)?;
        let scan = trace.read(crate::document::ID_FIELD.to_string(), Vec::new(), false);
        trace.entries(scan, primary.len());
        let wanted = page.skip.saturating_add(limit);
        for (_key, loc) in primary {
            let (id, doc) = read(&loc)?;
            let value = crate::query::value_or_null(&doc, &first.field);
            if crate::query::is_unordered(value) && filter.matches(&doc) {
                trace.matched(1);
                unordered.push((id, doc));
                // Without keys after the first, id order is their order.
                if !in_memory && page.passed() + unordered.len() == wanted {
                    break;
                }
            }
        }
        let unlimited = Filter {
            conditions: Vec::new(),
            skip: 0,
            limit: None,
            ..filter.clone()
        };
        for found in unlimited.apply_to(unordered, |(_id, doc)| doc) {
            if page.push(found) {
                break;
            }
        }
    }
    Ok(page.results)
}

/// The page `walk_in_index_order` hands out (SPEC §101.5): matches in
/// order, the first `skip` only counted.
struct Page {
    results: Vec<Candidate>,
    skipped: usize,
    skip: usize,
    limit: usize,
}

impl Page {
    /// The next match in order: passed over while the skip lasts, kept
    /// after. `true` once `limit` are kept.
    fn push(&mut self, found: Candidate) -> bool {
        if self.skipped < self.skip {
            self.skipped += 1;
            return false;
        }
        self.results.push(found);
        self.results.len() == self.limit
    }

    /// Matches so far, skipped or kept.
    fn passed(&self) -> usize {
        self.skipped + self.results.len()
    }
}

/// Index entries sharing the values of the served sort keys.
type Group = Vec<(Vec<u8>, RecordLocation)>;

/// Where the served sort keys' values are in an index key.
#[derive(Clone, Copy)]
struct Spans {
    /// Fixed fields before them.
    at: usize,
    /// Fixed fields and served keys.
    end: usize,
    fields: usize,
    /// The primary index (SPEC §96): a key is an id, not values.
    primary: bool,
}

impl Spans {
    /// The bytes a group shares: the fixed values and the served ones —
    /// up to the first that may stand for several values (a cut string,
    /// a huge number): the keys after it don't order entries it holds,
    /// whose true values may differ; they're sorted in memory instead.
    /// A damaged key with fewer values than the index has fields groups
    /// by the ones it has (SPEC §55).
    fn group<'k>(&self, key: &'k [u8]) -> &'k [u8] {
        if self.primary {
            return key; // one document, whose id is unique
        }
        let mut len = 0;
        let parts = key::each_part(key::value_part(key)).take(self.end);
        for (i, part) in parts.enumerate() {
            len += part.len();
            if i >= self.at && !key::part_is_exact(part, self.fields) {
                break;
            }
        }
        &key[..len]
    }

    /// Whether the served values of `key`'s group are each exact: one
    /// value each, so the group's entries are equal in them.
    fn exact(&self, key: &[u8]) -> bool {
        self.primary
            || key::each_part(self.group(key))
                .skip(self.at)
                .all(|part| key::part_is_exact(part, self.fields))
    }

    /// The served values a group shares, each on its own.
    fn values<'k>(&self, key: &'k [u8]) -> Vec<&'k [u8]> {
        if self.primary {
            return Vec::new(); // an id, exact
        }
        let shared = self.group(key);
        key::parts(shared).into_iter().skip(self.at).collect()
    }
}

/// A descending walk of a compound index hands out a group whose served
/// value is unordered (`key::is_other`) before the groups with the same
/// values up to it, since that tag sorts last — but in a sort, unordered
/// values go last in both directions (§34.1). So such a group is held
/// until the walk leaves those values, then handed out after them
/// (SPEC §49). Unordered values are rare; so are held groups.
struct Held {
    active: bool,
    spans: Spans,
    order: SortOrder,
    /// Held groups by the bytes before their first unordered value, the
    /// innermost last.
    stack: Vec<(Vec<u8>, Vec<Group>)>,
}

impl Held {
    fn new(active: bool, spans: Spans, order: SortOrder) -> Self {
        Held {
            active,
            spans,
            order,
            stack: Vec::new(),
        }
    }

    /// Takes a finished group, `next` being the walk's next key (`None`
    /// at the end), and returns the groups to hand out now, in order.
    fn pass(&mut self, group: Group, next: Option<&[u8]>) -> Vec<Group> {
        if !self.active {
            return vec![group];
        }
        let mut ready = Vec::new();
        let key = group[0].0.clone();
        let values = self.spans.values(&key);
        match values.iter().position(|part| key::is_other(part)) {
            Some(i) => {
                let before: usize = self.spans.group(&key).len()
                    - values[i..].iter().map(|p| p.len()).sum::<usize>();
                let prefix = key[..before].to_vec();
                match self.stack.last_mut() {
                    Some((top, groups)) if *top == prefix => groups.push(group),
                    _ => self.stack.push((prefix, vec![group])),
                }
            }
            None => ready.push(group),
        }
        while let Some((prefix, _)) = self.stack.last() {
            if next.is_some_and(|next| next.starts_with(prefix)) {
                break;
            }
            let (_, mut groups) = self.stack.pop().expect("just looked");
            let order = self.order;
            let spans = self.spans;
            groups.sort_by_cached_key(|group| {
                spans
                    .values(&group[0].0)
                    .into_iter()
                    .map(|part| (key::is_other(part), Directed(part.to_vec(), order)))
                    .collect::<Vec<_>>()
            });
            ready.extend(groups);
        }
        ready
    }
}

/// An encoded value that sorts ascending or descending as its
/// `SortOrder` says — the key `read_in_index_order` sorts groups by.
struct Directed(Vec<u8>, SortOrder);

impl Ord for Directed {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let ord = self.0.cmp(&other.0);
        match self.1 {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        }
    }
}

impl PartialOrd for Directed {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Directed {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Directed {}

/// `filter` stopping at its first match — what `find_one` asks for, so
/// a sorted one can stop early too (SPEC §34.2).
fn first_only(mut filter: Filter) -> Filter {
    filter.limit = Some(filter.limit.map_or(1, |limit| limit.min(1)));
    filter
}

/// The candidates behind `candidate_entries`, not yet checked against
/// the filter — an index range may hold a few that don't match (SPEC
/// §28.3) — each with the fields `names` names. Where the index read
/// covers the filter (SPEC §100), an entry decides instead of its
/// document: one that matches comes unread (`Source::Entry`), already
/// checked, and one that doesn't isn't read at all. With `sort`, for a
/// sort in memory, such a match comes with its entry's document to sort
/// by; where the index lacks a sort key, it is read for that key.
fn read_candidates(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
    names: Option<&[&str]>,
    sort: bool,
    trace: &Trace,
) -> std::io::Result<Vec<Candidate>> {
    let (entries, indexed) = candidate_entries(catalog, store, collection, filter, trace)?;
    let mut decides = indexed.covering(filter, false).map(|c| c.decider(filter));
    let orders = match sort {
        true => indexed.covering(filter, true),
        false => None,
    };
    let mut records = data::Records::new(store);
    let mut candidates = Vec::new();
    for (key, loc) in entries {
        let decided = decides.as_mut().and_then(|d| d.decide(&key));
        if decided == Some(false) {
            continue; // rejected by its entry, never read
        }
        // A match whose entry also holds the sort keys, or with no sort
        // to need them, stays unread: its entry's document is all the
        // sort looks at.
        let entry = match (decided, &orders) {
            (Some(true), _) if !sort => Some(Document::Null),
            (Some(true), Some(orders)) => orders.entry(&key),
            _ => None,
        };
        if let Some(fields) = entry {
            candidates.push((Source::Entry(key::doc_id(&key), loc), fields));
            continue;
        }
        trace.examined();
        // The id stored in the data cell itself (SPEC §11).
        let stored = records.stored(loc)?;
        let fields = stored.fields(names)?;
        candidates.push((Source::Read(stored), fields));
    }
    Ok(candidates)
}

/// `count`'s conditions counted entry by entry as an index is walked,
/// nothing collected (SPEC §101.4): with no conditions, the primary
/// index's entries, leaf by leaf; with conditions one range of one index
/// covers (§100), each entry decided by its run of equal values and each
/// leaf inside the range whole where its ends decide it, and only an
/// entry that can't decide read. `None` for every other filter, which
/// `read_candidates` counts: a lookup by id, a union of ranges, a
/// multikey index, a filter the entries don't cover.
fn count_in(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
) -> std::io::Result<Option<usize>> {
    let Some(meta) = catalog.get(collection) else {
        return Ok(Some(0));
    };
    if filter.conditions.is_empty() {
        let primary = BTreeIndex::new(meta.index_root);
        return primary.count(store, KeyRange::everything()).map(Some);
    }
    if filter.ids().is_some() {
        return Ok(None);
    }
    let (root, range, index) = match filter.index_ranges(catalog.indexes(collection)) {
        None => (meta.index_root, KeyRange::everything(), None),
        Some(mut ranges) if ranges.len() == 1 => {
            let range = ranges.pop().expect("one");
            if range.index.fields.iter().any(|f| crate::query::is_multi(f)) {
                return Ok(None);
            }
            (range.index.root, range.range, Some(range.index))
        }
        Some(_) => return Ok(None),
    };
    let Some(covering) = Covering::new(filter, index, false) else {
        return Ok(None);
    };
    let mut decider = covering.decider(filter);
    let names = filter.fields_with(false);
    let mut records = data::Records::new(store);
    let mut walk = BTreeIndex::new(root).walk(store, range, false);
    let mut count = 0;
    loop {
        let whole = walk
            .whole_leaf()?
            .map(|leaf| (decider.decide_all(leaf.lowest, leaf.highest), leaf.entries));
        if let Some((Some(matches), entries)) = whole {
            count += if matches { entries } else { 0 };
            walk.pass_leaf();
            continue;
        }
        let Some(entry) = walk.step() else {
            return Ok(Some(count));
        };
        let (key, loc) = entry?;
        let matches = match decider.decide(key) {
            Some(decided) => decided,
            None => {
                let fields = records.stored(loc)?.fields(names.as_deref())?;
                filter.matches(&fields)
            }
        };
        count += usize::from(matches);
    }
}

/// Maps a single-op write's outcome to `update`/`delete`'s `bool`.
fn found(result: crate::Result<()>) -> crate::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(crate::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Looks up a collection's `CollectionMeta`, creating it (allocating its
/// `index_root`) on first use — mirrors `Catalog::load`'s own
/// create-on-miss behavior for the catalog page itself. A free function
/// (not a `Collection` method) since `apply_write_op` runs inside
/// `Database::write_batch`, not on a `Collection`.
pub(crate) fn get_or_create_meta(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    name: &str,
) -> crate::Result<CollectionMeta> {
    match catalog.get(name) {
        Some(meta) => Ok(*meta),
        None => catalog.create_collection(store, name),
    }
}

/// Applies one `WriteOp` against its own collection's data+index pages —
/// called by `Database::write_batch` for each op, against staged pages.
/// `Insert` of an id that's already there is `Error::DuplicateId`;
/// `Update`/`Delete` of one that isn't is `Error::NotFound` — either
/// fails the whole batch (SPEC §22.2). These used to be silent no-ops,
/// once load-bearing for op replay (§16.1), which page-image recovery
/// (§19.4) no longer needs.
/// A document nested deeper than `Document::MAX_NESTING` is refused like a name
/// too long (SPEC §56): an `InvalidInput` error, and the batch rolls back.
fn refuse_too_deep(collection: &str, id: &DocId, doc: &Document) -> crate::Result<()> {
    if doc.nests_too_deep() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "document {id} for collection {collection:?} nests deeper than the limit of {} levels",
                Document::MAX_NESTING
            ),
        )
        .into());
    }
    Ok(())
}

pub(crate) fn apply_write_op(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    op: &WriteOp,
) -> crate::Result<()> {
    if let WriteOp::Insert(collection, _, doc) | WriteOp::Update(collection, _, doc) = op {
        // Every document is an object, to carry its id as `_id` (SPEC §95).
        if !matches!(doc, Document::Object(_)) {
            return Err(crate::Error::NotAnObject {
                collection: collection.clone(),
            });
        }
    }
    apply_unchecked(catalog, store, op)
}

/// `apply_write_op` without its check that the document is an object:
/// for tests that need a file holding one, as before SPEC §95.
fn apply_unchecked(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    op: &WriteOp,
) -> crate::Result<()> {
    match op {
        WriteOp::Insert(collection, id, doc) => {
            let meta = get_or_create_meta(catalog, store, collection)?;
            let mut index = BTreeIndex::new(meta.index_root);
            if index.lookup(store, &key::primary(*id))?.is_some() {
                return Err(crate::Error::DuplicateId {
                    collection: collection.clone(),
                    id: *id,
                });
            }
            let doc = with_id(doc.clone(), *id);
            refuse_too_deep(collection, id, &doc)?;
            let mut current = meta.current_data_page;
            let free = catalog.free_space_mut(collection);
            let loc = data::insert_record(store, &mut current, free, *id, &doc)?;
            index.insert(store, &key::primary(*id), loc)?;
            let secondary = catalog.indexes(collection);
            update_secondary_indexes(collection, secondary, store, *id, None, Some((&doc, loc)))?;
            save_current_data_page(catalog, store, collection, &meta, current)
        }
        WriteOp::Update(collection, id, doc) => {
            let (meta, loc) = locate(catalog, store, collection, id)?;
            let mut index = BTreeIndex::new(meta.index_root);
            let doc = with_id(doc.clone(), *id);
            refuse_too_deep(collection, id, &doc)?;
            let (secondary, free) = catalog.indexes_and_free_space(collection);
            let old = old_document(secondary, store, loc)?;
            let mut current = meta.current_data_page;
            let new_loc = data::update_record(store, &mut current, free, loc, *id, &doc)?;
            if new_loc != loc {
                // The document outgrew its page and moved (SPEC §20.3).
                index.remove(store, &key::primary(*id))?;
                index.insert(store, &key::primary(*id), new_loc)?;
            }
            let old = old.as_ref().map(|old| (old, loc));
            update_secondary_indexes(
                collection,
                secondary,
                store,
                *id,
                old,
                Some((&doc, new_loc)),
            )?;
            save_current_data_page(catalog, store, collection, &meta, current)
        }
        WriteOp::Delete(collection, id) => {
            let (meta, loc) = locate(catalog, store, collection, id)?;
            let mut index = BTreeIndex::new(meta.index_root);
            let (secondary, free) = catalog.indexes_and_free_space(collection);
            let old = old_document(secondary, store, loc)?;
            data::delete_record(store, meta.current_data_page, free, loc)?;
            index.remove(store, &key::primary(*id))?;
            let old = old.as_ref().map(|old| (old, loc));
            update_secondary_indexes(collection, secondary, store, *id, old, None)?;
            Ok(())
        }
    }
}

/// Where document `id` of `collection` lives — `Error::NotFound` if the
/// collection or the document doesn't exist.
fn locate(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    id: &DocId,
) -> crate::Result<(CollectionMeta, RecordLocation)> {
    let not_found = || crate::Error::NotFound {
        collection: collection.to_string(),
        id: *id,
    };
    let meta = *catalog.get(collection).ok_or_else(not_found)?;
    let loc = BTreeIndex::new(meta.index_root)
        .lookup(store, &key::primary(*id))?
        .ok_or_else(not_found)?;
    Ok((meta, loc))
}

/// The document stored at `loc`, if the collection has secondary indexes
/// to take its old keys out of — otherwise it isn't read at all.
fn old_document(
    indexes: &[IndexMeta],
    store: &dyn PageStore,
    loc: RecordLocation,
) -> crate::Result<Option<Document>> {
    if indexes.is_empty() {
        return Ok(None);
    }
    Ok(Some(data::get_record(store, loc)?.1))
}

/// A document's keys in `index`, in key order, each once. On one field
/// (SPEC §28): a key per value at the path that has an indexed type
/// (`key::encode_value`) — one value for a plain path, where a missing
/// field is indexed as null so `field == null` can use the index (§32);
/// one per element for a path with `[*]`, and none for no elements, a
/// multikey index (§42.2). On several fields, exactly one key, whatever
/// they hold: a compound index holds every document (§43.2). A sparse
/// index (§44) leaves out the null values — on several fields, the key
/// of a document null in all of them.
pub(crate) fn index_keys(doc: &Document, index: &IndexMeta, id: DocId) -> Vec<Vec<u8>> {
    let kept = |value: &&Document| !(index.sparse && matches!(value, Document::Null));
    let mut keys: Vec<Vec<u8>> = match index.single() {
        Some(field) => crate::query::values_at(doc, field)
            .into_iter()
            .filter(kept)
            .filter_map(|value| key::secondary(value, id))
            .collect(),
        None => {
            let values = field_values(doc, index);
            match values.iter().any(kept) {
                true => vec![key::compound(&values, id)],
                false => Vec::new(),
            }
        }
    };
    keys.sort();
    keys.dedup();
    keys
}

/// The values a compound index's key encodes, one per field.
fn field_values<'a>(doc: &'a Document, index: &IndexMeta) -> Vec<&'a Document> {
    index
        .fields
        .iter()
        .map(|field| crate::query::value_or_null(doc, field))
        .collect()
}

/// What a unique `index` compares of a document (SPEC §33): tuples of
/// values — one per field — each with the value part of the key it's
/// under. On one field, a 1-tuple per value, per element for `[*]`
/// (several elements can share a key: equal ones, and long strings a
/// key can't tell apart, §28.1 — each must be checked). On several
/// fields, the one tuple of their values (§43.4). Left out, since they
/// never collide: null values — on several fields, a tuple with a null
/// in it, as SQL does — and values no comparison finds equal (arrays,
/// objects, NaN, ...).
pub(crate) fn unique_tuples<'a>(
    doc: &'a Document,
    index: &IndexMeta,
) -> Vec<(Vec<u8>, Vec<&'a Document>)> {
    let comparable =
        |value: &Document| !matches!(value, Document::Null) && key::encode_value(value).is_some();
    match index.single() {
        Some(field) => crate::query::values_at(doc, field)
            .into_iter()
            .filter(|value| comparable(value))
            .map(|value| (key::encode_value(value).expect("comparable"), vec![value]))
            .collect(),
        None => {
            let values = field_values(doc, index);
            match values.iter().all(|value| comparable(value)) {
                true => vec![(key::encode_values(&values), values)],
                false => Vec::new(),
            }
        }
    }
}

/// Two `unique_tuples` tuples collide: every value equal as a filter's
/// `Eq` sees it.
pub(crate) fn same_values(a: &[&Document], b: &[&Document]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| crate::query::equal(a, b))
}

/// Brings every secondary index from a document's `old` state to its
/// `new` one — each `(document, location)`, `None` for "not there":
/// insert is `(None, new)`, delete `(old, None)`. Only keys that went
/// away are removed and only new ones inserted — all of them, if the
/// document moved. A unique index checks each new key's values before
/// anything goes in, not one that stayed (only the document moved).
fn update_secondary_indexes(
    collection: &str,
    indexes: &[IndexMeta],
    store: &mut dyn PageStore,
    id: DocId,
    old: Option<(&Document, RecordLocation)>,
    new: Option<(&Document, RecordLocation)>,
) -> crate::Result<()> {
    let moved = old.map(|(_, loc)| loc) != new.map(|(_, loc)| loc);
    for index in indexes {
        let before = old.map_or(Vec::new(), |(doc, _)| index_keys(doc, index, id));
        let after = new.map_or(Vec::new(), |(doc, _)| index_keys(doc, index, id));
        let mut tree = BTreeIndex::new(index.root);
        for key in &before {
            if moved || !after.contains(key) {
                tree.remove(store, key)?;
            }
        }
        let Some((doc, loc)) = new else { continue };
        if index.unique {
            for (value_part, values) in unique_tuples(doc, index) {
                let key = [value_part.as_slice(), &id.0].concat();
                if !before.contains(&key) {
                    check_unique(store, collection, index, id, &value_part, &values)?;
                }
            }
        }
        for key in &after {
            if moved || !before.contains(key) {
                tree.insert(store, key, loc)?;
            }
        }
    }
    Ok(())
}

/// Before document `id`'s `values` go into the unique `index` under
/// `value_part`: `Error::DuplicateValue` if another document has equal
/// values (SPEC §33) — in a multikey index, in any of its elements
/// (§42.2); in a compound index, in all of its fields (§43.4). Equal
/// means equal to a filter's `Eq` — so `1` and `1.0` collide, `"a"` and
/// `"A"` don't. Keys can't decide it alone: different values can share a
/// key's value part (large ints rounding to one `f64`, strings cut to the
/// key budget, §28.1), so every document under the same value part is
/// read and compared — normally none.
fn check_unique(
    store: &dyn PageStore,
    collection: &str,
    index: &IndexMeta,
    id: DocId,
    value_part: &[u8],
    values: &[&Document],
) -> crate::Result<()> {
    let tree = BTreeIndex::new(index.root);
    for (entry, loc) in tree.range(store, &KeyRange::prefixed(value_part))? {
        let existing = key::doc_id(&entry);
        if existing == id {
            continue;
        }
        let (_, other) = data::get_record(store, loc)?;
        let collides = unique_tuples(&other, index)
            .iter()
            .any(|(_, others)| same_values(others, values));
        if collides {
            return Err(crate::Error::DuplicateValue {
                collection: collection.to_string(),
                field: index.name(),
                id,
                existing,
            });
        }
    }
    Ok(())
}

/// Creates the secondary index on `field` and fills it from every
/// document in the collection — `false` if it already exists as asked.
/// An existing index with other options is an error, not changed: the
/// caller drops it first (SPEC §33.2). A unique index checks each
/// document as it goes in, so the first duplicate fails the batch.
fn build_index(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    fields: &[String],
    options: IndexOptions,
) -> crate::Result<bool> {
    if let Some(existing) = catalog
        .indexes(collection)
        .iter()
        .find(|i| i.fields == fields)
    {
        if existing.options() == options {
            return Ok(false);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{collection:?} already has {} index on {:?}, not {}; drop it first",
                describe(existing.options()),
                existing.name(),
                describe(options)
            ),
        )
        .into());
    }
    let meta = get_or_create_meta(catalog, store, collection)?;
    let index = catalog.create_index(store, collection, fields, options)?;
    let documents = BTreeIndex::new(meta.index_root).scan(store)?;
    fill_index(store, collection, &index, &documents)?;
    Ok(true)
}

/// Puts the `documents` (the primary index's entries) into the empty
/// `index`. A unique index checks each document as it goes in, so the
/// first duplicate fails the batch.
fn fill_index(
    store: &mut dyn PageStore,
    collection: &str,
    index: &IndexMeta,
    documents: &[(Vec<u8>, RecordLocation)],
) -> crate::Result<()> {
    let mut tree = BTreeIndex::new(index.root);
    for (_key, loc) in documents {
        let (id, doc) = data::get_record(store, *loc)?;
        if index.unique {
            for (value_part, values) in unique_tuples(&doc, index) {
                check_unique(store, collection, index, id, &value_part, &values)?;
            }
        }
        for key in index_keys(&doc, index, id) {
            tree.insert(store, &key, *loc)?;
        }
    }
    Ok(())
}

/// Makes sure a file of a format before 12 holds only objects (SPEC
/// §95), in `Database::open`'s batch, before it is stamped 12: every
/// document of every collection, by the first byte of its encoding.
/// One that isn't an object fails the open with
/// `FormatError::NotAnObject`, naming the first found.
pub(crate) fn only_objects_in_old_file(
    catalog: &Catalog,
    store: &crate::storage::FileStore,
    format: u32,
) -> crate::Result<()> {
    if format >= 12 {
        return Ok(());
    }
    let mut names: Vec<&str> = catalog.names().collect();
    names.sort_unstable();
    for name in names {
        let meta = catalog.get(name).expect("a listed collection");
        let mut records = data::Records::new(store);
        for (_key, loc) in BTreeIndex::new(meta.index_root).scan(store)? {
            let (id, object) = records.is_object(loc)?;
            if !object {
                return Err(crate::Error::Format(crate::FormatError::NotAnObject {
                    collection: name.to_string(),
                    id,
                }));
            }
        }
    }
    Ok(())
}

/// Brings a file from before ids had index keys (format 10, SPEC §62) up
/// to date, in `Database::open`'s batch: every secondary index that a
/// document holds an id in is rebuilt in place, and the header is
/// stamped, so that the check runs once and a build before 10 refuses
/// the file from then on. Only files with secondary indexes: without
/// them nothing needs checking, and the first write stamps the file.
/// Rebuilding a unique index can fail, if documents share an id in it:
/// before, ids never collided.
pub(crate) fn key_ids_in_old_indexes(
    catalog: &Catalog,
    store: &mut crate::storage::FileStore,
) -> crate::Result<()> {
    if !store.predates_id_keys()? {
        return Ok(());
    }
    let mut any = false;
    for name in catalog.names() {
        if catalog.indexes(name).is_empty() {
            continue;
        }
        any = true;
        let (documents, stale) = indexes_lacking_ids(catalog, store, name)?;
        for index in stale {
            BTreeIndex::new(index.root).clear(store)?;
            fill_index(store, name, index, &documents)?;
        }
    }
    if any {
        store.stamp_format_version()?;
    }
    Ok(())
}

/// Index entries: each key with the location of its document.
type Entries = Vec<(Vec<u8>, RecordLocation)>;

/// The secondary indexes of `collection` that a document holds an id in
/// (`holds_id`), and the collection's documents (the primary index's
/// entries), to rebuild them from.
fn indexes_lacking_ids<'c>(
    catalog: &'c Catalog,
    store: &dyn PageStore,
    collection: &str,
) -> crate::Result<(Entries, Vec<&'c IndexMeta>)> {
    let indexes = catalog.indexes(collection);
    let meta = catalog.get(collection).expect("a listed collection");
    let documents = BTreeIndex::new(meta.index_root).scan(store)?;
    let mut stale = vec![false; indexes.len()];
    let mut records = data::Records::new(store);
    for (_key, loc) in &documents {
        let (_id, doc) = records.get(*loc)?;
        for (stale, index) in stale.iter_mut().zip(indexes) {
            *stale |= holds_id(&doc, index);
        }
    }
    let stale = indexes
        .iter()
        .zip(stale)
        .filter(|(_, stale)| *stale)
        .map(|(index, _)| index)
        .collect();
    Ok((documents, stale))
}

/// Whether `doc` has an id among the values `index` keys: an entry it
/// lacks if the index was built before format 10 (SPEC §62).
fn holds_id(doc: &Document, index: &IndexMeta) -> bool {
    let is_id = |value: &&Document| matches!(value, Document::Id(_));
    match index.single() {
        Some(field) => crate::query::values_at(doc, field).iter().any(is_id),
        None => field_values(doc, index).iter().any(is_id),
    }
}

/// "a unique, sparse", "a plain", ... — an index's options in an error.
fn describe(options: IndexOptions) -> &'static str {
    match (options.unique, options.sparse) {
        (false, false) => "a plain",
        (true, false) => "a unique",
        (false, true) => "a sparse",
        (true, true) => "a unique, sparse",
    }
}

/// Persists a collection's current data page if `data::insert_record`/
/// `update_record` moved it to a newly allocated page — a catalog write
/// only once per filled page, not per insert.
fn save_current_data_page(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    meta: &CollectionMeta,
    current: PageId,
) -> crate::Result<()> {
    if current != meta.current_data_page {
        catalog.set_current_data_page(store, collection, current)?;
    }
    Ok(())
}

/// What creating or dropping an index on `_id` is refused with.
fn primary_key_error() -> crate::Error {
    crate::Error::from(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "`_id` is the primary key; it needs no secondary index",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serde::{Deserialize, Serialize};

    /// `{"v": value}`: a document holding a value, as every document is
    /// an object (SPEC §95).
    fn v(value: Document) -> Document {
        Document::Object([("v".to_string(), value)].into_iter().collect())
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct User {
        name: String,
        age: i64,
    }

    #[test]
    fn the_primary_key_index_is_neither_created_nor_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("t.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        for fields in ["_id", "_id.x"] {
            let create = users.ensure_index(fields).unwrap_err().to_string();
            let drop = users.drop_index(fields).unwrap_err().to_string();
            assert!(create.contains("primary key"), "{create}");
            assert_eq!(create, drop);
        }
        assert!(!users.drop_index("age").unwrap());
    }

    #[test]
    fn typed_collection_insert_then_get_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");

        let id = users
            .insert(User {
                name: "Ada".to_string(),
                age: 30,
            })
            .unwrap();

        let found = users.get(&id).unwrap();
        assert_eq!(
            found,
            Some(User {
                name: "Ada".to_string(),
                age: 30,
            })
        );
    }

    #[test]
    fn typed_collection_find_filters_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");

        users
            .insert(User {
                name: "Young".to_string(),
                age: 10,
            })
            .unwrap();
        users
            .insert(User {
                name: "Old".to_string(),
                age: 30,
            })
            .unwrap();

        let filter = Filter {
            conditions: vec![crate::query::Condition::Compare {
                field: "age".to_string(),
                op: crate::query::Op::Gte,
                value: Document::Int(18),
            }],
            ..Default::default()
        };

        let found = users.find(filter).unwrap();
        assert_eq!(
            found,
            vec![User {
                name: "Old".to_string(),
                age: 30
            }]
        );
    }

    #[test]
    fn insert_then_get_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let ada = v(Document::String("Ada".to_string()));
        let id = users.insert(ada.clone()).unwrap();

        let found = users.get(&id).unwrap();
        assert_eq!(found, Some(with_id(ada, id)));
    }

    #[test]
    fn get_on_unknown_id_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        assert_eq!(users.get(&DocId([9; 16])).unwrap(), None);
    }

    #[test]
    fn get_on_never_touched_collection_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let ghosts = db.collection::<Document>("ghosts");

        assert_eq!(ghosts.get(&DocId([1; 16])).unwrap(), None);
    }

    #[test]
    fn update_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let id = users.insert(v(Document::Int(1))).unwrap();
        assert!(users.update(&id, v(Document::Int(2))).unwrap());

        assert_eq!(
            users.get(&id).unwrap(),
            Some(with_id(v(Document::Int(2)), id))
        );
    }

    #[test]
    fn update_on_unknown_id_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        assert!(!users.update(&DocId([9; 16]), v(Document::Int(1))).unwrap());
    }

    #[test]
    fn delete_removes_it_from_get_and_find() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let id = users.insert(v(Document::Int(1))).unwrap();
        assert!(users.delete(&id).unwrap());

        assert_eq!(users.get(&id).unwrap(), None);
        assert_eq!(users.find(Filter::default()).unwrap(), Vec::new());
        assert!(!users.delete(&id).unwrap(), "second delete finds nothing");
    }

    #[test]
    fn find_filters_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let mut young = indexmap::IndexMap::new();
        young.insert("age".to_string(), Document::Int(10));
        let mut old = indexmap::IndexMap::new();
        old.insert("age".to_string(), Document::Int(30));

        users.insert(Document::Object(young)).unwrap();
        users.insert(Document::Object(old)).unwrap();

        let filter = Filter {
            conditions: vec![crate::query::Condition::Compare {
                field: "age".to_string(),
                op: crate::query::Op::Gte,
                value: Document::Int(18),
            }],
            ..Default::default()
        };

        let found = users.find(filter).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn insert_then_get_includes_the_id_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Document::String("Ada".to_string()));
        let id = users.insert(Document::Object(fields)).unwrap();

        let Document::Object(found) = users.get(&id).unwrap().unwrap() else {
            panic!("expected an Object back");
        };
        assert_eq!(found.get("_id"), Some(&Document::Id(id)));
        assert_eq!(
            found.get("name"),
            Some(&Document::String("Ada".to_string()))
        );
    }

    #[test]
    fn find_also_includes_the_id_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Document::String("Ada".to_string()));
        let id = users.insert(Document::Object(fields)).unwrap();

        let found = users.find(Filter::default()).unwrap();
        assert_eq!(found.len(), 1);
        let Document::Object(doc) = &found[0] else {
            panic!("expected an Object back");
        };
        assert_eq!(doc.get("_id"), Some(&Document::Id(id)));
    }

    /// An id in `_id` is the one an insert uses (SPEC §59, which changed
    /// §18's rule of always making a new one); any other `_id` — a
    /// string, the nil id — is still overwritten with the real one.
    #[test]
    fn insert_keeps_a_caller_supplied_id_and_overwrites_anything_else() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let with_id = |id: Document| {
            let mut fields = indexmap::IndexMap::new();
            fields.insert("_id".to_string(), id);
            fields.insert("name".to_string(), Document::String("Ada".to_string()));
            Document::Object(fields)
        };

        let chosen = DocId([0xFF; 16]);
        assert_eq!(users.insert(with_id(Document::Id(chosen))).unwrap(), chosen);
        let err = users.insert(with_id(Document::Id(chosen))).unwrap_err();
        assert!(
            matches!(err, crate::Error::DuplicateId { id, .. } if id == chosen),
            "{err:?}"
        );

        for other in [
            Document::String("mine".into()),
            Document::Id(DocId::NIL),
            Document::Null,
        ] {
            let id = users.insert(with_id(other.clone())).unwrap();
            assert!(id != chosen && id != DocId::NIL, "{other:?}");
            let Document::Object(found) = users.get(&id).unwrap().unwrap() else {
                panic!("expected an Object back");
            };
            assert_eq!(found.get("_id"), Some(&Document::Id(id)), "{other:?}");
        }
    }

    #[test]
    fn update_also_refreshes_the_id_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".to_string(), Document::String("Ada".to_string()));
        let id = users.insert(Document::Object(fields)).unwrap();

        let mut updated = indexmap::IndexMap::new();
        updated.insert("name".to_string(), Document::String("Grace".to_string()));
        assert!(users.update(&id, Document::Object(updated)).unwrap());

        let Document::Object(found) = users.get(&id).unwrap().unwrap() else {
            panic!("expected an Object back");
        };
        assert_eq!(found.get("_id"), Some(&Document::Id(id)));
        assert_eq!(
            found.get("name"),
            Some(&Document::String("Grace".to_string()))
        );
    }

    /// SPEC §95: every write refuses a document that isn't an object,
    /// typed or untyped, alone or in a batch, and the batch rolls back.
    #[test]
    fn every_write_refuses_a_document_that_is_not_an_object() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let ada = users.insert(v(Document::Int(1))).unwrap();
        let refused = |result: crate::Result<()>| {
            let Err(crate::Error::NotAnObject { collection }) = result else {
                panic!("expected NotAnObject, got {result:?}");
            };
            assert_eq!(collection, "users");
        };

        for value in [
            Document::Int(42),
            Document::String("Ada".into()),
            Document::Array(vec![Document::Int(1)]),
            Document::Null,
            Document::Bool(true),
        ] {
            refused(users.insert(value.clone()).map(drop));
            refused(users.update(&ada, value.clone()).map(drop));
            refused(users.upsert(Filter::new().eq("v", 2), value).map(drop));
        }
        refused(db.collection::<i64>("users").insert(7).map(drop));
        refused(db.collection::<Vec<i64>>("users").insert(vec![7]).map(drop));

        // A batch rolls back whole: the object before the number too.
        let mut batch = db.batch();
        batch.insert(&users, v(Document::Int(2))).unwrap();
        batch.insert(&users, Document::Int(3)).unwrap();
        refused(batch.commit().map(drop));
        // So does `update_many`, whose closure returned a number for one.
        users.insert(v(Document::Int(4))).unwrap();
        let changed = users.update_many(Filter::new(), |doc| {
            if crate::query::field_value(doc, "v") == Some(&Document::Int(4)) {
                *doc = Document::Int(4);
            } else {
                *doc = v(Document::Int(0));
            }
        });
        refused(changed.map(drop));

        let found = users.find(Filter::new().sort_asc("v")).unwrap();
        let values: Vec<Option<&Document>> = found
            .iter()
            .map(|doc| crate::query::field_value(doc, "v"))
            .collect();
        assert_eq!(values, [Some(&Document::Int(1)), Some(&Document::Int(4))]);
    }

    #[test]
    fn two_collections_stay_independent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let posts = db.collection::<Document>("posts");

        let id = users
            .insert(v(Document::String("Ada".to_string())))
            .unwrap();

        assert_eq!(posts.get(&id).unwrap(), None);
    }

    /// The point of SPEC §20: small documents share pages. 1000
    /// small documents used to take 1000 data pages (~8 MB); packed they
    /// take a handful.
    #[test]
    fn small_documents_are_packed_into_few_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        let pings = db.collection::<Document>("pings");

        let ops: Vec<WriteOp> = (0..1000)
            .map(|i| {
                let doc = Document::Object(
                    [
                        ("lat".to_string(), Document::Int(i)),
                        ("lon".to_string(), Document::Int(-i)),
                    ]
                    .into_iter()
                    .collect(),
                );
                WriteOp::Insert("pings".to_string(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();

        let pages = std::fs::metadata(&path).unwrap().len() / crate::storage::PAGE_SIZE as u64;
        assert!(pages < 30, "1000 small documents took {pages} pages");
        assert_eq!(pings.find(Filter::default()).unwrap().len(), 1000);
    }

    /// Growing documents until they no longer fit beside their neighbors
    /// moves them to other pages; the index must follow, across a reopen.
    #[test]
    fn documents_that_outgrow_their_page_stay_reachable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let ids: Vec<DocId> = {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            let ids: Vec<DocId> = (0..20)
                .map(|_| docs.insert(v(Document::Binary(vec![1; 100]))).unwrap())
                .collect();
            for (n, id) in ids.iter().enumerate() {
                assert!(
                    docs.update(id, v(Document::Binary(vec![n as u8; 3000])))
                        .unwrap()
                );
            }
            ids
        };

        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        for (n, id) in ids.iter().enumerate() {
            assert_eq!(
                docs.get(id).unwrap(),
                Some(with_id(v(Document::Binary(vec![n as u8; 3000])), *id))
            );
        }
        assert_eq!(docs.find(Filter::default()).unwrap().len(), 20);

        // Deleting everything frees the data pages for reuse by new ones.
        let len_before = std::fs::metadata(&path).unwrap().len();
        for id in &ids {
            assert!(docs.delete(id).unwrap());
        }
        for _ in 0..20 {
            docs.insert(v(Document::Binary(vec![9; 3000]))).unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len_before);
        assert_eq!(docs.find(Filter::default()).unwrap().len(), 20);
    }

    fn age_filter(op: crate::query::Op, age: i64) -> Filter {
        Filter {
            conditions: vec![crate::query::Condition::Compare {
                field: "age".to_string(),
                op,
                value: Document::Int(age),
            }],
            ..Filter::default()
        }
    }

    /// The sync workload prototype's blocker (SPEC §5.3): find by a field,
    /// then update by id — after a restart, with no id kept from
    /// `insert`. The struct carries its id (§59), the way that replaced
    /// `find_with_ids` (§74).
    #[test]
    fn a_found_struct_carries_an_updatable_id_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        {
            let db = Database::open(&path).unwrap();
            let tasks = db.collection::<Todo>("tasks");
            for title in ["write", "test", "ship"] {
                tasks.insert(todo(title)).unwrap();
            }
        }

        let db = Database::open(&path).unwrap();
        let tasks = db.collection::<Todo>("tasks");
        let found = tasks
            .find(Filter::new().any_of([
                Condition::eq("title", "test"),
                Condition::eq("title", "ship"),
            ]))
            .unwrap();
        assert_eq!(found.len(), 2);
        for task in &found {
            assert_eq!(tasks.get(&task.id.unwrap()).unwrap().as_ref(), Some(task));
        }

        let mut ship = found.into_iter().find(|t| t.title == "ship").unwrap();
        ship.owner = Some(DocId([7; 16]));
        let id = ship.id.unwrap();
        assert!(tasks.update(&id, ship.clone()).unwrap());
        assert_eq!(tasks.get(&id).unwrap(), Some(ship));
    }

    /// SPEC §60: `indexes` says what each index is — its fields and
    /// options — in creation order; field names come in every form a
    /// caller holds them in.
    #[test]
    fn indexes_list_their_fields_and_options() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let from_config: Vec<String> = vec!["status".into(), "created".into()];
        docs.ensure_index("age").unwrap();
        docs.ensure_index_with("email", IndexOptions::new().unique())
            .unwrap();
        docs.ensure_index_with(from_config.clone(), IndexOptions::new().sparse())
            .unwrap();
        docs.ensure_index(vec!["a", "b"]).unwrap();
        docs.ensure_index(&["c".to_string()][..]).unwrap();

        let listed: Vec<(String, Vec<String>, bool, bool)> = docs
            .indexes()
            .unwrap()
            .into_iter()
            .map(|index| {
                let fields = index.fields().to_vec();
                (index.name(), fields, index.is_unique(), index.is_sparse())
            })
            .collect();
        let owned = |fields: &[&str]| fields.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        assert_eq!(
            listed,
            [
                ("age".to_string(), owned(&["age"]), false, false),
                ("email".to_string(), owned(&["email"]), true, false),
                ("(status, created)".to_string(), from_config, false, true),
                ("(a, b)".to_string(), owned(&["a", "b"]), false, false),
                ("c".to_string(), owned(&["c"]), false, false),
            ]
        );
        assert_eq!(
            docs.indexes().unwrap()[1].options(),
            IndexOptions::new().unique()
        );
    }

    /// `limit` takes a number or an `Option` of one, so a caller's
    /// optional limit passes straight through.
    #[test]
    fn limit_takes_an_option() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        for n in 0..5 {
            docs.insert(v(Document::Int(n))).unwrap();
        }
        let found = |limit: Option<usize>| docs.find(Filter::new().limit(limit)).unwrap().len();
        assert_eq!(found(None), 5);
        assert_eq!(found(Some(2)), 2);
        assert_eq!(docs.find(Filter::new().limit(3)).unwrap().len(), 3);
    }

    /// SPEC §59: a struct carrying `#[serde(rename = "_id")] id:
    /// Option<DocId>`, and a `DocId` field referring to another document.
    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    struct Todo {
        #[serde(rename = "_id")]
        id: Option<DocId>,
        title: String,
        owner: Option<DocId>,
    }

    fn todo(title: &str) -> Todo {
        Todo {
            id: None,
            title: title.to_string(),
            owner: None,
        }
    }

    /// SPEC §74: a type without an id field, say from another crate, gets
    /// one by being wrapped, its fields flattened into the wrapper's; the
    /// stored document is the same, so the bare type still reads it.
    #[test]
    fn a_wrapper_gives_a_type_without_an_id_field_its_id() {
        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
        struct WithId {
            #[serde(rename = "_id")]
            id: Option<DocId>,
            #[serde(flatten)]
            user: User,
        }

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<WithId>("users");
        users.ensure_index("age").unwrap();
        let ada = User {
            name: "Ada".to_string(),
            age: 36,
        };
        let id = users
            .insert(WithId {
                id: None,
                user: ada.clone(),
            })
            .unwrap();

        let by_age = Filter::new().eq("age", 36);
        let found = users.find(by_age.clone()).unwrap();
        let streamed: Vec<WithId> = users.cursor(by_age).unwrap().map(Result::unwrap).collect();
        let expected = WithId {
            id: Some(id),
            user: ada.clone(),
        };
        assert_eq!((found, streamed), (vec![expected.clone()], vec![expected]));
        let bare = db.collection::<User>("users").find(Filter::new()).unwrap();
        assert_eq!(bare, [ada]);
    }

    /// Every way of reading fills the id in; `None` on insert makes one,
    /// `Some` is used; an update's id argument wins over the field.
    #[test]
    fn a_struct_carries_its_own_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let tasks = db.collection::<Todo>("tasks");

        let made = tasks.insert(todo("made")).unwrap();
        let chosen = DocId([0xAB; 16]);
        let given = Todo {
            id: Some(chosen),
            ..todo("given")
        };
        assert_eq!(tasks.insert(given.clone()).unwrap(), chosen);
        assert!(matches!(
            tasks.insert(given.clone()).unwrap_err(),
            crate::Error::DuplicateId { id, .. } if id == chosen
        ));

        assert_eq!(tasks.get(&made).unwrap().unwrap().id, Some(made));
        assert_eq!(tasks.get(&chosen).unwrap(), Some(given));
        let everything = || Filter::new().sort_asc("title");
        let found: Vec<Option<DocId>> = tasks
            .find(everything())
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(found, [Some(chosen), Some(made)]);
        let one = tasks.find_one(Filter::new().eq("title", "made")).unwrap();
        assert_eq!(one.unwrap().id, Some(made));
        let streamed: Vec<Option<DocId>> = tasks
            .cursor(Filter::new())
            .unwrap()
            .map(|item| item.unwrap().id)
            .collect();
        assert_eq!(streamed.len(), 2);
        assert!(streamed.iter().all(Option::is_some));

        // The argument names the document; the field can't move it.
        let moved = Todo {
            id: Some(DocId([1; 16])),
            ..todo("renamed")
        };
        assert!(tasks.update(&made, moved).unwrap());
        assert_eq!(tasks.get(&made).unwrap().unwrap().id, Some(made));
        assert_eq!(tasks.get(&DocId([1; 16])).unwrap(), None);
    }

    /// `upsert` and a typed batch use a given id too, and a `DocId` field
    /// is stored as an id, so a filter on it finds it.
    #[test]
    fn upserts_batches_and_references_keep_ids() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let tasks = db.collection::<Todo>("tasks");

        let upserted = DocId([2; 16]);
        let outcome = tasks
            .upsert(
                Filter::new().eq("title", "nothing"),
                Todo {
                    id: Some(upserted),
                    ..todo("upserted")
                },
            )
            .unwrap();
        assert_eq!(outcome, Upserted::Inserted(upserted));

        let batched = DocId([3; 16]);
        let mut batch = db.batch();
        let id = batch
            .insert(
                &tasks,
                Todo {
                    id: Some(batched),
                    owner: Some(upserted),
                    ..todo("batched")
                },
            )
            .unwrap();
        assert_eq!(id, batched);
        batch.commit().unwrap();

        // A filter on a reference: a scan here, with no index on it
        // (with one, it reads the index since SPEC §62).
        let by_owner = Filter::new().eq("owner", upserted);
        assert_eq!(
            tasks.explain(&by_owner).unwrap(),
            crate::query::QueryPlan::Scan
        );
        let found = tasks.find(by_owner).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].id, found[0].owner),
            (Some(batched), Some(upserted))
        );
    }

    /// The other style: a plain `DocId` field, `DocId::NIL` meaning "make
    /// one".
    #[test]
    fn a_plain_id_field_with_nil_for_new() {
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct Note {
            #[serde(rename = "_id")]
            id: DocId,
            text: String,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let notes = db.collection::<Note>("notes");
        let id = notes
            .insert(Note {
                id: DocId::NIL,
                text: "hi".into(),
            })
            .unwrap();
        assert_ne!(id, DocId::NIL);
        assert_eq!(notes.get(&id).unwrap().unwrap().id, id);
    }

    /// Sort and limit act on the documents, and each id stays with its
    /// own document through both — what `delete_many` and `update_many`
    /// rely on to write the right ones.
    #[test]
    fn ids_stay_paired_through_sort_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        let mut ids = std::collections::HashMap::new();
        for (name, age) in [("Ada", 36), ("Grace", 45), ("Linus", 21), ("Ken", 30)] {
            let id = users
                .insert(User {
                    name: name.to_string(),
                    age,
                })
                .unwrap();
            ids.insert(name.to_string(), id);
        }

        let filter = Filter {
            sort: vec![crate::query::Sort {
                field: "age".to_string(),
                order: crate::query::SortOrder::Desc,
            }],
            limit: Some(3),
            ..age_filter(crate::query::Op::Gte, 25)
        };
        let found = users.find_with_ids(filter).unwrap();

        let names: Vec<&str> = found.iter().map(|(_id, u)| u.name.as_str()).collect();
        assert_eq!(names, ["Grace", "Ada", "Ken"]);
        for (id, user) in &found {
            assert_eq!(ids[&user.name], *id);
        }
    }

    /// SPEC §71: a tool that only speaks JSON text — inserts it, reads
    /// it, edits it, writes it back — through `Collection<Document>`.
    #[test]
    fn documents_go_in_and_out_as_json_text() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let people = db.collection::<Document>("people");

        let new: Document = r#"{"name":"Ann","born":{"$date":"1990-05-01T12:00:00Z"}}"#
            .parse()
            .unwrap();
        let id = people.insert(new).unwrap();
        let text = people.get(&id).unwrap().unwrap().to_string();
        assert_eq!(
            text,
            format!(
                r#"{{"_id":{{"$id":"{id}"}},"name":"Ann","born":{{"$date":"1990-05-01T12:00:00Z"}}}}"#
            )
        );

        let edited = text.replace("Ann", "Bea").parse().unwrap();
        assert!(people.update(&id, edited).unwrap());
        let found = people.find(Filter::new().eq("name", "Bea")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].to_string(), text.replace("Ann", "Bea"));

        // The id in the text is kept on insert, as any `_id` holding an id is (§59).
        let other = DocId([0x42; 16]);
        let copy: Document = text
            .replace(&id.to_string(), &other.to_string())
            .parse()
            .unwrap();
        assert_eq!(people.insert(copy.clone()).unwrap(), other);
        // An update takes its id from its argument; the text's `_id` is ignored.
        assert!(people.update(&id, copy).unwrap());
        let got = people.get(&id).unwrap().unwrap();
        assert_eq!(got.to_string(), text);
    }

    /// A search field (SPEC §5.3): a case-insensitive
    /// substring search, combined with other conditions, on the typed path.
    #[test]
    fn typed_find_with_a_case_insensitive_contains() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        for (name, age) in [("Ada Lovelace", 36), ("ada", 12), ("Grace Hopper", 45)] {
            users
                .insert(User {
                    name: name.to_string(),
                    age,
                })
                .unwrap();
        }

        let mut filter = age_filter(crate::query::Op::Gt, 18);
        filter.conditions.push(crate::query::Condition::Compare {
            field: "name".to_string(),
            op: crate::query::Op::Contains,
            value: Document::String("ADA".to_string()),
        });
        let found = users.find(filter).unwrap();

        assert_eq!(
            found,
            [User {
                name: "Ada Lovelace".to_string(),
                age: 36
            }]
        );
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Article {
        title: String,
        body: String,
    }

    /// The large-document workload (SPEC §5.2, §26): articles whose body is
    /// far larger than a page, stored, found by content, rewritten, and read back after a
    /// reopen.
    #[test]
    fn documents_larger_than_a_page_work_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let article = |title: &str, sentence: &str, times| Article {
            title: title.to_string(),
            body: sentence.repeat(times),
        };
        let id = {
            let db = Database::open(&path).unwrap();
            let articles = db.collection::<Article>("articles");
            articles
                .insert(article("short", "A quiet morning. ", 3))
                .unwrap();
            let id = articles
                .insert(article("long", "The storm kept on. ", 5_000))
                .unwrap();
            assert!(
                articles
                    .update(&id, article("long", "The storm broke at last. ", 8_000))
                    .unwrap()
            );
            id
        };

        let db = Database::open(&path).unwrap();
        let articles = db.collection::<Article>("articles");
        let expected = article("long", "The storm broke at last. ", 8_000);
        assert_eq!(articles.get(&id).unwrap(), Some(expected.clone()));
        let found = articles
            .find(Filter {
                conditions: vec![crate::query::Condition::Compare {
                    field: "body".to_string(),
                    op: crate::query::Op::Contains,
                    value: Document::String("STORM BROKE".to_string()),
                }],
                ..Filter::default()
            })
            .unwrap();
        assert_eq!(found, vec![expected]);
    }

    // --- Secondary indexes (SPEC §28) ---

    use crate::query::{Condition, Op, QueryPlan};
    use crate::testing::XorShift;

    fn object(pairs: Vec<(&str, Document)>) -> Document {
        Document::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn cond(field: &str, op: Op, value: Document) -> Condition {
        Condition::Compare {
            field: field.to_string(),
            op,
            value,
        }
    }

    fn filter(conditions: Vec<Condition>) -> Filter {
        Filter {
            conditions,
            ..Filter::default()
        }
    }

    /// A value of a type an index handles, or doesn't — duplicates are
    /// likely, and so are values that share an encoding (large ints
    /// rounding to one f64, long strings agreeing past the key's cut).
    fn random_value(rng: &mut XorShift) -> Document {
        let big = 1i64 << 53;
        match rng.below(12) {
            0 | 1 => Document::Int(rng.below(11) as i64 - 5),
            2 => Document::Int(big + rng.below(4) as i64),
            3 => Document::Float((rng.below(21) as f64 - 10.0) / 2.0),
            4 => [Document::Float(-0.0), Document::Float(f64::NAN)][rng.below(2)].clone(),
            5 | 6 => Document::String(["", "a", "ab", "b", "B", "a\0"][rng.below(6)].to_string()),
            7 => Document::String("x".repeat(1200) + ["a", "b", ""][rng.below(3)]),
            8 => Document::Bool(rng.below(2) == 1),
            9 => [Document::Null, Document::Array(vec![Document::Int(1)])][rng.below(2)].clone(),
            // A few ids, which compare, sort and are indexed (SPEC §62).
            10 => Document::Id(DocId([1 + rng.below(3) as u8; 16])),
            // A few date-times, before and after 1970 (SPEC §69).
            _ => Document::DateTime(
                crate::DateTime::from_unix(rng.below(5) as i64 - 2, [0, 999_999_999][rng.below(2)])
                    .unwrap(),
            ),
        }
    }

    fn random_document(rng: &mut XorShift) -> Document {
        let mut fields = vec![("w", Document::Int(rng.below(3) as i64))];
        if rng.below(8) != 0 {
            fields.push(("v", random_value(rng)));
        }
        // Padding of very different sizes, so an update often no longer
        // fits its page and moves the document (SPEC §20.3) — its index
        // entries must follow even when the value didn't change. Now and
        // then large enough for overflow pages.
        let pad = [0, 0, 1500, 3000, 4500, 9000][rng.below(6)];
        fields.push(("pad", Document::String("y".repeat(pad))));
        object(fields)
    }

    /// Conditions on the indexed `path`, sometimes with one on `w`.
    fn random_filter(rng: &mut XorShift, path: &str) -> Filter {
        let ops = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
        let mut conditions = vec![cond(path, ops[rng.below(5)].clone(), random_value(rng))];
        match rng.below(3) {
            0 => conditions.push(cond(path, ops[rng.below(5)].clone(), random_value(rng))),
            1 => conditions.push(cond("w", Op::Eq, Document::Int(rng.below(3) as i64))),
            _ => {}
        }
        filter(conditions)
    }

    /// Every collection's free-space map names only its own data pages,
    /// with the room they really have (SPEC §75).
    fn assert_free_space_is_true(db: &Database) {
        let state = db.state();
        for name in state.catalog().names() {
            let Some(free) = state.catalog().free_space(name) else {
                continue;
            };
            let meta = *state.catalog().get(name).unwrap();
            let locs = BTreeIndex::new(meta.index_root).scan(&state.store).unwrap();
            let current = meta.current_data_page;
            let locs = locs.into_iter().map(|(_key, loc)| loc);
            let (owned, _chains) = data::collection_pages(&state.store, current, locs).unwrap();
            data::assert_free_space_is_true(&state.store, current, &owned, free);
        }
    }

    /// A document of about `pad` bytes, numbered `n`.
    fn padded(n: i64, pad: usize) -> Document {
        object(vec![
            ("n", Document::Int(n)),
            ("pad", Document::String("x".repeat(pad))),
        ])
    }

    /// SPEC §75: the space deletes leave behind in older pages is refilled
    /// by later inserts, so a collection deleting and inserting as much
    /// stays the size it was.
    #[test]
    fn inserts_refill_the_space_deletes_leave_behind() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        // In batches of 100, each one commit: one flush, not one per document.
        let mut ids = Vec::new();
        for chunk in (0..2000).collect::<Vec<i64>>().chunks(100) {
            let mut batch = db.batch();
            for &n in chunk {
                ids.push(batch.insert(&docs, padded(n, 400)).unwrap());
            }
            batch.commit().unwrap();
        }
        let mut batch = db.batch();
        for id in ids.iter().step_by(2) {
            batch.delete(&docs, id);
        }
        batch.commit().unwrap();
        let before = db.file_info().unwrap().pages;

        for chunk in (0..1000).collect::<Vec<i64>>().chunks(100) {
            let mut batch = db.batch();
            for &n in chunk {
                batch.insert(&docs, padded(n, 400)).unwrap();
            }
            batch.commit().unwrap();
        }
        let after = db.file_info().unwrap().pages;
        // The new ids' index entries need new pages; the documents don't.
        let state = db.state();
        let new_data_pages = (before..after)
            .filter(|&page| {
                state.store.read_page(page).unwrap()[0] == crate::storage::PageType::Data as u8
            })
            .count();
        drop(state);
        assert_eq!(new_data_pages, 0, "of {} new pages", after - before);
        assert_consistent(&db);
    }

    /// SPEC §75: dropping a collection forgets its map, whose pages are
    /// freed; a new collection of the same name starts without it.
    #[test]
    fn a_dropped_collection_takes_its_free_space_map_along() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let ids: Vec<DocId> = (0..8)
            .map(|n| docs.insert(padded(n, 3000)).unwrap())
            .collect();
        docs.delete(&ids[0]).unwrap();
        assert!(db.drop_collection("docs").unwrap());

        docs.insert(padded(0, 3000)).unwrap();
        assert_consistent(&db);
    }

    /// SPEC §75: a failed batch leaves the map as it was, so it names no
    /// page the batch allocated or freed and the rollback took back.
    #[test]
    fn a_failed_batch_leaves_the_free_space_map_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let ids: Vec<DocId> = (0..8)
            .map(|n| docs.insert(padded(n, 3000)).unwrap())
            .collect();
        docs.delete(&ids[0]).unwrap();
        let map = |db: &Database| db.state().catalog().free_space("docs").unwrap().entries();
        let before = map(&db);
        assert!(!before.is_empty());

        let mut batch = db.batch();
        for id in &ids[1..6] {
            batch.delete(&docs, id); // empties and frees pages
        }
        for n in 0..6 {
            batch.insert(&docs, padded(n, 5000)).unwrap(); // allocates pages
        }
        batch.update(&docs, &DocId([7; 16]), padded(0, 1)).unwrap();
        assert!(matches!(batch.commit(), Err(crate::Error::NotFound { .. })));

        assert_eq!(map(&db), before);
        assert_consistent(&db);
    }

    fn sorted_ids(results: Vec<(DocId, Document)>) -> Vec<DocId> {
        let mut ids: Vec<DocId> = results.into_iter().map(|(id, _)| id).collect();
        ids.sort();
        ids
    }

    /// The whole file checks out (SPEC §39): no leaked or doubly used
    /// page, every index agreeing with the documents. Then it's compacted
    /// (SPEC §41) — keeping every document, and checking out again — so
    /// whatever the test does next runs on a rebuilt file.
    fn assert_consistent(db: &Database) {
        let report = db.check().unwrap();
        assert!(report.is_ok(), "{:#?}", report.problems);
        assert_free_space_is_true(db);

        let everything = |db: &Database| {
            let mut names = db.collections().unwrap();
            names.sort();
            names
                .into_iter()
                .map(|name| {
                    let docs = db.collection::<Document>(&name);
                    let mut all = docs.find_with_ids(Filter::default()).unwrap();
                    all.sort_by_key(|(id, _)| *id);
                    // Debug text: a NaN isn't equal to itself.
                    format!("{name} {:?} {all:?}", docs.index_names().unwrap())
                })
                .collect::<Vec<_>>()
        };
        let before = everything(db);
        let compacted = db.compact().unwrap();
        assert!(compacted.pages_after <= compacted.pages_before);
        assert_eq!(everything(db), before);
        let report = db.check().unwrap();
        assert!(report.is_ok(), "after compacting: {:#?}", report.problems);
    }

    /// Every random filter must find through the index exactly what it
    /// finds by checking every document.
    fn assert_index_agrees_with_scan(docs: &Collection<Document>, rng: &mut XorShift, path: &str) {
        assert_consistent(docs.db());
        let all = docs.find_with_ids(Filter::default()).unwrap();
        for _ in 0..300 {
            let f = random_filter(rng, path);
            let expected = sorted_ids(f.apply_to(all.clone(), |(_, doc)| doc));
            let plan = explained(docs, &f);
            let found = sorted_ids(docs.find_with_ids(f.clone()).unwrap());
            assert_eq!(found, expected, "{f:?} via {plan:?}");
        }
    }

    #[test]
    fn indexed_finds_match_full_scans_through_every_kind_of_write() {
        indexed_finds_match_full_scans(IndexOptions::default());
    }

    const SPARSE: IndexOptions = IndexOptions {
        unique: false,
        sparse: true,
    };

    /// A sparse index (SPEC §44) lacks the null and missing `v`s, which
    /// a random filter asks for now and then: it must be left out then.
    #[test]
    fn sparse_finds_match_full_scans_through_every_kind_of_write() {
        indexed_finds_match_full_scans(SPARSE);
    }

    fn indexed_finds_match_full_scans(options: IndexOptions) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
        {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            let mut ids = Vec::new();
            // Batches of 25 ops keep the test fast (each commit syncs to
            // disk twice); every op still goes through `apply_write_op`.
            let mut insert = |rng: &mut XorShift, count| {
                for _ in 0..count / 25 {
                    let ops = (0..25)
                        .map(|_| {
                            let id = db.id_gen().generate();
                            ids.push(id);
                            WriteOp::Insert("docs".into(), id, random_document(rng))
                        })
                        .collect();
                    db.write_batch(ops).unwrap();
                }
            };
            // Some documents before the index exists: it's built from them.
            insert(&mut rng, 150);
            assert!(docs.ensure_index_with("v", options).unwrap());
            // ...and some after: inserts maintain it.
            insert(&mut rng, 150);
            for _ in 0..6 {
                let ops = (0..25)
                    .map(|_| {
                        let id = ids[rng.below(ids.len())];
                        let new = match docs.get(&id).unwrap() {
                            // Half the time only the padding changes, so
                            // `v`'s key stays and only a move touches it.
                            Some(Document::Object(mut fields)) if rng.below(2) == 0 => {
                                let pad = "z".repeat([0, 4500, 9000][rng.below(3)]);
                                fields.insert("pad".into(), Document::String(pad));
                                Document::Object(fields)
                            }
                            _ => random_document(&mut rng),
                        };
                        WriteOp::Update("docs".into(), id, new)
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            let deleted = (0..60).map(|_| ids.swap_remove(rng.below(ids.len())));
            let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
            db.write_batch(ops.collect()).unwrap();
            let v_eq_1 = filter(vec![cond("v", Op::Eq, Document::Int(1))]);
            assert_eq!(
                docs.explain(&v_eq_1).unwrap(),
                QueryPlan::Index {
                    field: "v".to_string()
                }
            );
            assert_index_agrees_with_scan(&docs, &mut rng, "v");
        }

        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        assert_eq!(docs.index_names().unwrap(), vec!["v".to_string()]);
        assert_index_agrees_with_scan(&docs, &mut rng, "v");
    }

    /// A document with arrays for `[*]` paths (SPEC §42): `tags` holds
    /// up to 4 random values (repeats and nested arrays included), or
    /// is missing, empty or a scalar; `items` holds objects with an `n`
    /// (or without one), and now and then something that isn't an
    /// object.
    fn random_array_document(rng: &mut XorShift) -> Document {
        let mut fields = vec![("w", Document::Int(rng.below(3) as i64))];
        match rng.below(8) {
            0 => {}
            1 => fields.push(("tags", random_value(rng))),
            _ => {
                let tags = (0..rng.below(5)).map(|_| random_value(rng)).collect();
                fields.push(("tags", Document::Array(tags)));
            }
        }
        let items = (0..rng.below(4))
            .map(|_| match rng.below(6) {
                0 => object(vec![]),
                1 => random_value(rng),
                _ => object(vec![("n", random_value(rng))]),
            })
            .collect();
        fields.push(("items", Document::Array(items)));
        let pad = [0, 0, 1500, 4500, 9000][rng.below(5)];
        fields.push(("pad", Document::String("y".repeat(pad))));
        object(fields)
    }

    /// A condition about one element, for `elem_match`: on `n` of an
    /// `items` element, or on a `tags` element itself (`""`) — ANDs,
    /// ORs and NOTs of comparisons, any of which may be null.
    fn random_element_condition(rng: &mut XorShift, path: &str) -> Condition {
        let ops = [Op::Eq, Op::Ne, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
        let one = |rng: &mut XorShift| cond(path, ops[rng.below(6)].clone(), random_value(rng));
        match rng.below(6) {
            0 | 1 => one(rng),
            2 | 3 => one(rng) & one(rng),
            4 => one(rng) | one(rng),
            _ => !one(rng),
        }
    }

    /// Random `elem_match` filters must find what checking every
    /// document finds (SPEC §46), bounded by the multikey indexes on
    /// `items[*].n` and `tags[*]` where their condition allows — which
    /// must happen.
    fn assert_elem_match_agrees_with_scan(docs: &Collection<Document>, rng: &mut XorShift) {
        let all = docs.find_with_ids(Filter::default()).unwrap();
        let mut plans = std::collections::HashSet::new();
        for _ in 0..300 {
            let mut f = match rng.below(2) {
                0 => Filter::new().elem_match("items", random_element_condition(rng, "n")),
                _ => Filter::new().elem_match("tags", random_element_condition(rng, "")),
            };
            if rng.below(3) == 0 {
                f = f.eq("w", rng.below(3) as i64);
            }
            let expected = sorted_ids(f.apply_to(all.clone(), |(_, doc)| doc));
            let plan = explained(docs, &f);
            let found = sorted_ids(docs.find_with_ids(f.clone()).unwrap());
            assert_eq!(found, expected, "{f:?} via {plan:?}");
            plans.insert(format!("{plan:?}"));
        }
        for plan in [
            r#"Index { field: "items[*].n" }"#,
            r#"Index { field: "tags[*]" }"#,
            "Scan",
        ] {
            assert!(plans.contains(plan), "{plan} never ran: {plans:?}");
        }
    }

    /// `indexed_finds_match_full_scans_through_every_kind_of_write` for
    /// multikey indexes (SPEC §42.2): a document has as many entries as
    /// elements, and every write must keep exactly those.
    #[test]
    fn multikey_finds_match_full_scans_through_every_kind_of_write() {
        multikey_finds_match_full_scans(IndexOptions::default());
    }

    /// Sparse, a multikey index leaves out the null elements (SPEC §44).
    #[test]
    fn sparse_multikey_finds_match_full_scans_through_every_kind_of_write() {
        multikey_finds_match_full_scans(SPARSE);
    }

    fn multikey_finds_match_full_scans(options: IndexOptions) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut rng = XorShift(0x6A09_E667_F3BC_C908);
        let paths = ["tags[*]", "items[*].n"];
        {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            let mut ids = Vec::new();
            let mut insert = |rng: &mut XorShift, count| {
                for _ in 0..count / 25 {
                    let ops = (0..25)
                        .map(|_| {
                            let id = db.id_gen().generate();
                            ids.push(id);
                            WriteOp::Insert("docs".into(), id, random_array_document(rng))
                        })
                        .collect();
                    db.write_batch(ops).unwrap();
                }
            };
            insert(&mut rng, 150);
            for path in paths {
                assert!(docs.ensure_index_with(path, options).unwrap());
            }
            insert(&mut rng, 150);
            for _ in 0..6 {
                let ops = (0..25)
                    .map(|_| {
                        let id = ids[rng.below(ids.len())];
                        let new = match docs.get(&id).unwrap() {
                            Some(Document::Object(mut fields)) if rng.below(2) == 0 => {
                                let pad = "z".repeat([0, 4500, 9000][rng.below(3)]);
                                fields.insert("pad".into(), Document::String(pad));
                                Document::Object(fields)
                            }
                            _ => random_array_document(&mut rng),
                        };
                        WriteOp::Update("docs".into(), id, new)
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            let deleted = (0..60).map(|_| ids.swap_remove(rng.below(ids.len())));
            let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
            db.write_batch(ops.collect()).unwrap();
            for path in paths {
                let by_value = filter(vec![cond(path, Op::Eq, Document::Int(1))]);
                let field = path.to_string();
                assert_eq!(docs.explain(&by_value).unwrap(), QueryPlan::Index { field });
                assert_index_agrees_with_scan(&docs, &mut rng, path);
            }
            assert_elem_match_agrees_with_scan(&docs, &mut rng);
        }

        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        assert_eq!(docs.index_names().unwrap(), paths);
        for path in paths {
            assert_index_agrees_with_scan(&docs, &mut rng, path);
        }
        assert_elem_match_agrees_with_scan(&docs, &mut rng);
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Post {
        title: String,
        tags: Vec<String>,
        comments: Vec<Comment>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Comment {
        author: String,
        likes: i64,
    }

    fn post(title: &str, tags: &[&str], comments: &[(&str, i64)]) -> Post {
        Post {
            title: title.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            comments: comments
                .iter()
                .map(|(author, likes)| Comment {
                    author: author.to_string(),
                    likes: *likes,
                })
                .collect(),
        }
    }

    /// The typed use: conditions on elements, the same answers with and
    /// without indexes, each post once even when several of its elements
    /// match, and every kind of write keeping the indexes right.
    #[test]
    fn posts_are_found_by_any_tag_or_comment() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let posts = db.collection::<Post>("posts");
        posts
            .insert(post("a", &["rust", "db"], &[("ann", 3), ("bob", 12)]))
            .unwrap();
        posts
            .insert(post("b", &["rust", "rust", "web"], &[("ann", 40)]))
            .unwrap();
        posts.insert(post("c", &[], &[])).unwrap();
        posts.insert(post("d", &["draft"], &[("cy", 1)])).unwrap();

        let titles = |f: Filter| {
            let mut titles: Vec<String> = posts
                .find(f.sort_asc("title"))
                .unwrap()
                .into_iter()
                .map(|p| p.title)
                .collect();
            titles.dedup();
            titles
        };
        let queries = || {
            [
                (Filter::new().eq("tags[*]", "rust"), vec!["a", "b"]),
                (Filter::new().ne("tags[*]", "rust"), vec!["c", "d"]),
                (Filter::new().gt("comments[*].likes", 10), vec!["a", "b"]),
                (
                    Filter::new().eq("comments[*].author", "ann"),
                    vec!["a", "b"],
                ),
                (Filter::new().contains("tags[*]", "RA"), vec!["d"]),
                (
                    Filter::new()
                        .eq("tags[*]", "rust")
                        .lt("comments[*].likes", 5),
                    vec!["a"],
                ),
            ]
        };
        for (f, expected) in queries() {
            assert_eq!(titles(f.clone()), expected, "{f:?}");
        }
        posts.ensure_index("tags[*]").unwrap();
        posts.ensure_index("comments[*].likes").unwrap();
        for (f, expected) in queries() {
            assert_eq!(
                titles(f.clone()),
                expected,
                "{f:?} via {:?}",
                posts.explain(&f)
            );
        }
        let rust = Filter::new().eq("tags[*]", "rust");
        assert_eq!(
            posts.explain(&rust).unwrap(),
            QueryPlan::Index {
                field: "tags[*]".into()
            }
        );
        // "b" has "rust" twice and matches once, in every kind of read.
        assert_eq!(posts.find(rust.clone()).unwrap().len(), 2);
        assert_eq!(posts.count(rust.clone()).unwrap(), 2);
        assert_eq!(posts.cursor(rust.clone()).unwrap().count(), 2);

        // Writes: a tag removed stops matching, one added starts.
        let changed = posts
            .update_many(Filter::new().eq("title", "a"), |p| {
                p.tags = vec!["db".into(), "new".into()];
            })
            .unwrap();
        assert_eq!(changed, 1);
        assert_eq!(titles(rust.clone()), vec!["b"]);
        assert_eq!(titles(Filter::new().eq("tags[*]", "new")), vec!["a"]);
        assert_eq!(
            posts
                .delete_many(Filter::new().eq("tags[*]", "web"))
                .unwrap(),
            1
        );
        assert_eq!(titles(rust), Vec::<String>::new());
        assert_consistent(&db);

        // Export and import keep the multikey indexes as they are.
        let mut export = Vec::new();
        db.export(&mut export).unwrap();
        let copy = Database::open(dir.path().join("copy.trunkdb")).unwrap();
        copy.import(export.as_slice()).unwrap();
        let copied = copy.collection::<Post>("posts");
        assert_eq!(
            copied.index_names().unwrap(),
            ["tags[*]", "comments[*].likes"]
        );
        let new = Filter::new().eq("tags[*]", "new");
        assert_eq!(
            copied.find(new.clone()).unwrap(),
            posts.find(new.clone()).unwrap()
        );
        assert_eq!(
            copied.explain(&new).unwrap(),
            QueryPlan::Index {
                field: "tags[*]".into()
            }
        );
        assert_consistent(&copy);
    }

    /// Unique across documents, not within one (SPEC §42.2): no two
    /// users share an alias, and one user may list an alias twice.
    #[test]
    fn a_unique_multikey_index_refuses_an_element_another_document_has() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let aliases = |names: &[&str]| {
            let names = names.iter().map(|n| Document::from(*n)).collect();
            object(vec![("aliases", Document::Array(names))])
        };
        users
            .ensure_index_with("aliases[*]", crate::IndexOptions::new().unique())
            .unwrap();
        let ada = users.insert(aliases(&["ada", "al"])).unwrap();
        users.insert(aliases(&["bo", "bo"])).unwrap();
        users.insert(aliases(&[])).unwrap();
        users.insert(aliases(&[])).unwrap();
        match users.insert(aliases(&["cy", "al"])) {
            Err(crate::Error::DuplicateValue {
                existing, field, ..
            }) => {
                assert_eq!((existing, field.as_str()), (ada, "aliases[*]"));
            }
            other => panic!("expected DuplicateValue, got {other:?}"),
        }
        // Ada gives "al" up; then someone else may have it.
        assert!(users.update(&ada, aliases(&["ada"])).unwrap());
        users.insert(aliases(&["cy", "al"])).unwrap();
        // Ada keeps what she has: her own elements don't collide.
        assert!(users.update(&ada, aliases(&["ada", "ada2"])).unwrap());
        assert_consistent(&db);

        // Built over existing documents that share an element: refused.
        let other = db.collection::<Document>("other");
        other.insert(aliases(&["x", "y"])).unwrap();
        other.insert(aliases(&["y"])).unwrap();
        assert!(matches!(
            other.ensure_index_with("aliases[*]", crate::IndexOptions::new().unique()),
            Err(crate::Error::DuplicateValue { .. })
        ));
        assert_eq!(other.index_names().unwrap(), Vec::<String>::new());
    }

    /// Two long strings that differ only past the key budget share a key
    /// (SPEC §28.1). In one document's elements they're one entry — and
    /// still both checked: a document listing `…a` and `…b` can't take
    /// the `…b` another one has, whichever comes first.
    #[test]
    fn a_unique_multikey_index_checks_every_element_under_a_shared_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let long = |end: &str| Document::String("x".repeat(1200) + end);
        let aliases = |names: Vec<Document>| object(vec![("aliases", Document::Array(names))]);
        users
            .ensure_index_with("aliases[*]", crate::IndexOptions::new().unique())
            .unwrap();
        users.insert(aliases(vec![long("b")])).unwrap();
        for both in [vec![long("a"), long("b")], vec![long("b"), long("a")]] {
            assert!(matches!(
                users.insert(aliases(both)),
                Err(crate::Error::DuplicateValue { .. })
            ));
        }
        users.insert(aliases(vec![long("a"), long("c")])).unwrap();
        assert_consistent(&db);
    }

    #[test]
    fn index_paths_take_brackets_only_as_a_whole_step_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        for bad in [
            "tags[*", "tags[1]", "ta[*]gs", "[*]", "a.[*]", "_id[*]", "tags[]",
        ] {
            assert!(docs.ensure_index(bad).is_err(), "{bad}");
        }
        for good in ["tags[*]", "items[*].n", "grid[*][*]", "a.b[*].c"] {
            assert!(docs.ensure_index(good).unwrap(), "{good}");
        }
    }

    /// Sorting by a path with `[*]` has nothing to sort by — every
    /// document reads as null there, in id order — and never reads a
    /// document twice, with or without its index (SPEC §42.3).
    #[test]
    fn sorting_by_elements_reads_each_document_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        for tags in [vec![3, 1], vec![2], vec![5, 4, 5]] {
            let tags = tags.into_iter().map(Document::from).collect();
            docs.insert(object(vec![("tags", Document::Array(tags))]))
                .unwrap();
        }
        docs.ensure_index("tags[*]").unwrap();
        let sorted = Filter::new().sort_desc("tags[*]").limit(10);
        assert_ne!(
            docs.explain(&sorted).unwrap(),
            QueryPlan::IndexOrder {
                field: "tags[*]".into()
            }
        );
        let found = docs.find_with_ids(sorted).unwrap();
        let ids: Vec<DocId> = found.iter().map(|(id, _)| *id).collect();
        let mut in_order = ids.clone();
        in_order.sort();
        assert_eq!(ids, in_order);
    }

    #[test]
    fn ensure_and_drop_index_are_idempotent_and_drop_frees_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        let mut batch = db.batch();
        for age in 0..1000 {
            let name = format!("user {age}");
            batch.insert(&users, User { name, age }).unwrap();
        }
        batch.commit().unwrap();
        let file_len = || {
            std::fs::metadata(dir.path().join("test.trunkdb"))
                .unwrap()
                .len()
        };
        let adults = age_filter(Op::Gte, 18);

        assert!(users.ensure_index("age").unwrap());
        assert!(!users.ensure_index("age").unwrap());
        assert!(users.ensure_index("name").unwrap());
        assert_eq!(users.index_names().unwrap(), ["age", "name"]);
        assert_eq!(
            users.explain(&adults).unwrap(),
            QueryPlan::Index {
                field: "age".to_string()
            }
        );
        let with_indexes = file_len();

        assert!(users.drop_index("age").unwrap());
        assert!(!users.drop_index("age").unwrap());
        assert_eq!(users.index_names().unwrap(), ["name"]);
        assert_eq!(users.explain(&adults).unwrap(), QueryPlan::Scan);
        assert_eq!(users.find(adults.clone()).unwrap().len(), 982);

        // The dropped index's pages are reused, not appended after.
        assert!(users.ensure_index("age").unwrap());
        assert_eq!(file_len(), with_indexes);
        assert_eq!(users.find(adults).unwrap().len(), 982);
    }

    #[test]
    fn an_index_can_be_declared_before_the_collection_exists() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");

        assert!(users.ensure_index("name").unwrap());
        users
            .insert(User {
                name: "Ada".to_string(),
                age: 36,
            })
            .unwrap();

        let found = users
            .find(filter(vec![cond(
                "name",
                Op::Eq,
                Document::String("Ada".to_string()),
            )]))
            .unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn id_malformed_and_overlong_paths_cannot_be_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");

        let invalid = ["_id", "_id.x", "", ".", "a.", ".a", "a..b"].map(String::from);
        for field in invalid.into_iter().chain(["f".repeat(256)]) {
            let Err(crate::Error::Io(err)) = docs.ensure_index(&field) else {
                panic!("{field:?} must be rejected");
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert!(docs.index_names().unwrap().is_empty());
    }

    /// A document with a value at `a.b.c`, or at a spot that path must
    /// not reach: one level short, inside an array, under a key with a
    /// dot in it — or no `a` at all.
    fn random_nested_document(rng: &mut XorShift) -> Document {
        let v = random_value(rng);
        let a = match rng.below(8) {
            0..=3 => object(vec![("b", object(vec![("c", v), ("d", Document::Int(1))]))]),
            4 => object(vec![("b", v)]),
            5 => object(vec![("b.c", v)]),
            6 => Document::Array(vec![object(vec![("b", object(vec![("c", v)]))])]),
            _ => v,
        };
        let mut fields = vec![("w", Document::Int(rng.below(3) as i64))];
        match rng.below(8) {
            0 => fields.push(("a.b.c", random_value(rng))),
            1 => {}
            _ => fields.push(("a", a)),
        }
        let pad = [0, 0, 1500, 9000][rng.below(4)];
        fields.push(("pad", Document::String("y".repeat(pad))));
        object(fields)
    }

    #[test]
    fn nested_path_indexes_match_full_scans_through_every_kind_of_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            let mut ids = Vec::new();
            let mut insert = |rng: &mut XorShift| {
                let ops = (0..25)
                    .map(|_| {
                        let id = db.id_gen().generate();
                        ids.push(id);
                        WriteOp::Insert("docs".into(), id, random_nested_document(rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            };
            (0..4).for_each(|_| insert(&mut rng));
            assert!(docs.ensure_index("a.b.c").unwrap());
            (0..4).for_each(|_| insert(&mut rng));
            // Updates move values into and out of the path's reach.
            for _ in 0..4 {
                let ops = (0..25)
                    .map(|_| {
                        let id = ids[rng.below(ids.len())];
                        WriteOp::Update("docs".into(), id, random_nested_document(&mut rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            let deleted = (0..30).map(|_| ids.swap_remove(rng.below(ids.len())));
            let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
            db.write_batch(ops.collect()).unwrap();

            let reachable = docs
                .find(Filter::default())
                .unwrap()
                .iter()
                .filter(|doc| crate::query::field_value(doc, "a.b.c").is_some())
                .count();
            assert!(reachable > 50, "only {reachable} documents have the path");
            let eq_1 = filter(vec![cond("a.b.c", Op::Eq, Document::Int(1))]);
            assert_eq!(
                docs.explain(&eq_1).unwrap(),
                QueryPlan::Index {
                    field: "a.b.c".to_string()
                }
            );
            assert_index_agrees_with_scan(&docs, &mut rng, "a.b.c");
        }

        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        assert_eq!(docs.index_names().unwrap(), ["a.b.c"]);
        assert_index_agrees_with_scan(&docs, &mut rng, "a.b.c");
    }

    /// The typed path: a nested struct is a nested object, so its fields
    /// are reachable by path — for filters, indexes and sort alike.
    #[test]
    fn typed_nested_fields_are_filtered_indexed_and_sorted_by_path() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Address {
            city: String,
            zip: i64,
        }
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Person {
            name: String,
            address: Address,
        }
        let person = |name: &str, city: &str, zip| Person {
            name: name.to_string(),
            address: Address {
                city: city.to_string(),
                zip,
            },
        };

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let people = db.collection::<Person>("people");
        people.insert(person("Ada", "Berlin", 10115)).unwrap();
        people.insert(person("Bob", "Hamburg", 20095)).unwrap();
        people.insert(person("Cy", "Berlin", 10245)).unwrap();
        assert!(people.ensure_index("address.city").unwrap());

        let in_berlin = Filter {
            sort: vec![crate::query::Sort {
                field: "address.zip".to_string(),
                order: SortOrder::Desc,
            }],
            ..filter(vec![cond(
                "address.city",
                Op::Eq,
                Document::String("Berlin".to_string()),
            )])
        };
        assert_eq!(
            people.explain(&in_berlin).unwrap(),
            QueryPlan::Index {
                field: "address.city".to_string()
            }
        );
        assert_eq!(
            people.find(in_berlin.clone()).unwrap(),
            [
                person("Cy", "Berlin", 10245),
                person("Ada", "Berlin", 10115)
            ]
        );

        // Moving to another city moves the index entry too.
        let (cy, _) = people.find_with_ids(in_berlin.clone()).unwrap().remove(0);
        people.update(&cy, person("Cy", "Hamburg", 20095)).unwrap();
        assert_eq!(
            people.find(in_berlin).unwrap(),
            [person("Ada", "Berlin", 10115)]
        );
    }

    /// `None` on the typed path, whether stored as null, left out by serde,
    /// or never written because the field is newer than the document:
    /// `== null` finds all three, through the index (SPEC §32).
    #[test]
    fn null_and_missing_fields_are_found_alike_through_the_index() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Member {
            name: String,
            nick: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            team: Option<String>,
        }
        let member = |name: &str, nick: Option<&str>, team: Option<&str>| Member {
            name: name.to_string(),
            nick: nick.map(String::from),
            team: team.map(String::from),
        };

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        // Written before `Member` had `nick`: no such key at all.
        db.collection::<User>("members")
            .insert(user("Old", 99))
            .unwrap();
        let members = db.collection::<Member>("members");
        members.insert(member("Ada", None, None)).unwrap();
        members
            .insert(member("Bob", Some("B"), Some("red")))
            .unwrap();
        members.insert(member("Cy", Some("C"), None)).unwrap();
        assert!(members.ensure_index("nick").unwrap());
        assert!(members.ensure_index("team").unwrap());

        let names = |field: &str, op: Op| {
            let f = filter(vec![cond(field, op, Document::Null)]);
            let mut names: Vec<String> = db
                .collection::<Document>("members")
                .find(f)
                .unwrap()
                .into_iter()
                .map(|doc| match crate::query::field_value(&doc, "name") {
                    Some(Document::String(name)) => name.clone(),
                    other => panic!("no name: {other:?}"),
                })
                .collect();
            names.sort();
            names
        };
        let nick_is_null = filter(vec![cond("nick", Op::Eq, Document::Null)]);
        assert_eq!(
            members.explain(&nick_is_null).unwrap(),
            QueryPlan::Index {
                field: "nick".to_string()
            }
        );
        assert_eq!(names("nick", Op::Eq), ["Ada", "Old"]);
        assert_eq!(names("nick", Op::Ne), ["Bob", "Cy"]);
        // `team` is never stored as null, only left out.
        assert_eq!(names("team", Op::Eq), ["Ada", "Cy", "Old"]);
        assert_eq!(names("team", Op::Ne), ["Bob"]);

        // Setting and clearing a field moves its index entry.
        let (bob, _) = members
            .find_with_ids(filter(vec![cond(
                "team",
                Op::Eq,
                Document::String("red".to_string()),
            )]))
            .unwrap()
            .remove(0);
        members.update(&bob, member("Bob", None, None)).unwrap();
        assert_eq!(names("nick", Op::Eq), ["Ada", "Bob", "Old"]);
        assert_eq!(names("team", Op::Eq), ["Ada", "Bob", "Cy", "Old"]);
    }

    // --- Sorting through an index (SPEC §34) ---

    /// A sorted filter, mostly limited, sometimes with conditions: a range
    /// on `v`, an `Eq` on `w` (which beats reading in order), or ones no
    /// index answers. Mostly sorted by `v`; sometimes by `w`, whose three
    /// values tie a lot — with a range on `v` and no limit, candidates
    /// come in `v`'s order, and only the id order of ties makes the
    /// result match.
    ///
    /// Sometimes sorted by `_id` (SPEC §96), first or after `v`, and
    /// compared with one of `ids` or an id no document has: one or
    /// two comparisons, `ne` or one with a number, none of which an
    /// `Eq` makes a lookup by id.
    fn random_sorted_filter(rng: &mut XorShift, ids: &[DocId]) -> Filter {
        let ops = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
        let some_id = |rng: &mut XorShift| match rng.below(4) {
            0 => Document::Id(DocId(
                rng.next().to_be_bytes().repeat(2).try_into().unwrap(),
            )),
            _ => Document::Id(ids[rng.below(ids.len())]),
        };
        let ranges = [Op::Lt, Op::Lte, Op::Gt, Op::Gte, Op::Ne];
        let mut conditions = match rng.below(8) {
            0 => vec![cond("v", ops[rng.below(5)].clone(), random_value(rng))],
            1 => vec![cond("w", Op::Eq, Document::Int(rng.below(3) as i64))],
            2 => vec![cond("v", Op::Ne, random_value(rng))],
            3 => vec![cond("pad", Op::Eq, Document::String(String::new()))],
            4 => (0..1 + rng.below(2))
                .map(|_| cond("_id", ranges[rng.below(5)].clone(), some_id(rng)))
                .collect(),
            5 => vec![cond("_id", ranges[rng.below(5)].clone(), Document::Int(3))],
            _ => vec![],
        };
        if rng.below(6) == 0 {
            conditions.push(cond("_id", ranges[rng.below(4)].clone(), some_id(rng)));
        }
        let order = [SortOrder::Asc, SortOrder::Desc][rng.below(2)];
        let field = ["v", "v", "w", "_id", "_id"][rng.below(5)].to_string();
        let limits = [
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(5),
            Some(20),
            Some(60),
            Some(1000),
        ];
        let skips = [0, 0, 0, 1, 3, 25, 1000];
        let mut sort = vec![crate::query::Sort { field, order }];
        if rng.below(4) == 0 {
            // `_id` after `w`, which ties a lot, or keys after `_id`,
            // which never ties.
            let field = match sort[0].field.as_str() {
                "_id" => "w",
                _ => "_id",
            };
            sort.push(crate::query::Sort {
                field: field.to_string(),
                order: [SortOrder::Asc, SortOrder::Desc][rng.below(2)],
            });
        }
        Filter {
            conditions,
            sort,
            skip: skips[rng.below(skips.len())],
            limit: limits[rng.below(limits.len())],
        }
    }

    /// Every sorted, limited find must return exactly what sorting a full
    /// scan in memory returns — same documents, same order, ties in id
    /// order — whichever plan it runs.
    fn assert_sorted_finds_agree_with_memory(docs: &Collection<Document>, rng: &mut XorShift) {
        assert_consistent(docs.db());
        let mut all = docs.find_with_ids(Filter::default()).unwrap();
        all.sort_by_key(|(id, _doc)| *id);
        let ids: Vec<DocId> = all.iter().map(|(id, _)| *id).collect();
        let mut plans = std::collections::HashSet::new();
        for _ in 0..500 {
            let f = random_sorted_filter(rng, &ids);
            let expected: Vec<DocId> = f
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let plan = explained(docs, &f);
            let found: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(found, expected, "{f:?} via {plan:?}");
            // Typed (SPEC §99): matched on fields, deserialized last.
            assert_eq!(typed_ids(docs, &f), expected, "{f:?} via {plan:?}");
            plans.insert(format!("{plan:?}"));
        }
        for plan in [
            r#"IndexOrder { field: "v" }"#,
            r#"IndexOrder { field: "w" }"#,
            r#"IndexOrder { field: "_id" }"#,
            r#"Index { field: "v" }"#,
            r#"Index { field: "w" }"#,
            "Scan",
        ] {
            assert!(plans.contains(plan), "{plan} never ran: {plans:?}");
        }
    }

    #[test]
    fn sorting_through_an_index_matches_sorting_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut rng = XorShift(0x0BAD_5EED_1234_5678);
        {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            docs.ensure_index("v").unwrap();
            docs.ensure_index("w").unwrap();
            let mut ids = Vec::new();
            for _ in 0..12 {
                let ops = (0..25)
                    .map(|_| {
                        let id = db.id_gen().generate();
                        ids.push(id);
                        WriteOp::Insert("docs".into(), id, random_document(&mut rng))
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            let updates = (0..40)
                .map(|_| {
                    let id = ids[rng.below(ids.len())];
                    WriteOp::Update("docs".into(), id, random_document(&mut rng))
                })
                .collect();
            db.write_batch(updates).unwrap();
            let deleted = (0..30).map(|_| ids.swap_remove(rng.below(ids.len())));
            let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
            db.write_batch(ops.collect()).unwrap();
            assert_sorted_finds_agree_with_memory(&docs, &mut rng);
        }
        let db = Database::open(&path).unwrap();
        assert_sorted_finds_agree_with_memory(&db.collection("docs"), &mut rng);
    }

    /// 1,000 documents `{n, group}`, `n` from 0 and `group` its last
    /// digit, both indexed — and their ids, in id order.
    fn thousand_by_id(db: &Database) -> (Collection<Document>, Vec<DocId>) {
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("n").unwrap();
        docs.ensure_index("group").unwrap();
        let mut ids = Vec::new();
        for batch in 0..10 {
            let ops = (0..100)
                .map(|i| {
                    let n = Document::Int(batch * 100 + i);
                    let group = Document::Int(i % 10);
                    let id = db.id_gen().generate();
                    ids.push(id);
                    WriteOp::Insert("docs".into(), id, object(vec![("n", n), ("group", group)]))
                })
                .collect();
            db.write_batch(ops).unwrap();
        }
        ids.sort();
        (docs, ids)
    }

    /// SPEC §96: a sort by `_id` with a limit walks the primary index,
    /// either way, from where its `_id` comparisons with an id start it,
    /// and reads only what it hands out and skips — and, where the filter
    /// names nothing but `_id`, only what it hands out (SPEC §100).
    #[test]
    fn a_sort_by_id_walks_the_primary_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let (docs, ids) = thousand_by_id(&db);
        // `(entries, read)`: entries walked, documents read. Fewer read
        // than walked: the walk covered the filter.
        let walk =
            |f: Filter, bounds: Vec<Condition>, backward: bool, (entries, read): (u64, u64)| {
                let run = docs.explain_run(f.clone()).unwrap();
                assert_eq!(
                    run.plan,
                    QueryPlan::IndexOrder {
                        field: "_id".into()
                    },
                    "{f:?}"
                );
                let index = "_id".to_string();
                let covering = f.conditions.iter().all(|c| match c {
                    Condition::Compare { field, .. } => field == "_id",
                    _ => false,
                });
                assert!(covering || entries == read);
                let expected = crate::explain::IndexRead {
                    index,
                    bounds,
                    backward,
                    entries,
                    covering,
                };
                assert_eq!(run.reads, [expected], "{f:?}");
                assert_eq!(run.stats.documents_examined, read, "{f:?}");
                let keys = f.sort.len();
                let sort = SortWork {
                    from_index: keys,
                    in_memory: 0,
                };
                assert_eq!(run.sort, Some(sort));
                let found = docs.find_with_ids(f).unwrap();
                found.into_iter().map(|(id, _)| id).collect::<Vec<_>>()
            };
        let first = Filter::new().sort_asc("_id").limit(20);
        assert_eq!(walk(first, vec![], false, (20, 20)), ids[..20]);
        let last = Filter::new().sort_desc("_id").limit(20);
        let mut newest = ids[980..].to_vec();
        newest.reverse();
        assert_eq!(walk(last, vec![], true, (20, 20)), newest);
        // Keyset paging: the 10 after the 500th.
        let after = Condition::gt("_id", ids[500]);
        let next = Filter::new().sort_asc("_id").limit(10).and(after.clone());
        assert_eq!(walk(next, vec![after], false, (10, 10)), ids[501..511]);
        // Up to and with the 500th, backwards, and a stretch between two.
        let upto = Condition::lte("_id", ids[500]);
        let back = Filter::new().sort_desc("_id").limit(3).and(upto.clone());
        assert_eq!(
            walk(back, vec![upto], true, (3, 3)),
            [ids[500], ids[499], ids[498]]
        );
        let (from, before) = (Condition::gte("_id", ids[7]), Condition::lt("_id", ids[9]));
        let between = Filter::new()
            .sort_asc("_id")
            .limit(5)
            .and(from.clone())
            .and(before.clone());
        assert_eq!(walk(between, vec![from, before], false, (2, 2)), ids[7..9]);
        // Page 3 of 50: the 100 skipped are walked, not read (SPEC §100).
        let page = Filter::new().sort_asc("_id").skip(100).limit(50);
        assert_eq!(walk(page, vec![], false, (150, 50)), ids[100..150]);
        // Keys after `_id` have nothing to order: no two ids tie.
        let more = Filter::new().sort_asc("_id").then_desc("n").limit(4);
        assert_eq!(walk(more, vec![], false, (4, 4)), ids[..4]);
        // Other conditions are checked, up to the third match.
        let all = docs.find(Filter::new().sort_asc("_id")).unwrap();
        let high = Filter::new().gt("n", 994);
        let third = (all.iter().enumerate())
            .filter(|(_, doc)| high.matches(doc))
            .nth(2)
            .unwrap()
            .0;
        let range = Filter::new().sort_asc("_id").limit(3).gt("n", 994);
        let found = walk(range, vec![], false, (third as u64 + 1, third as u64 + 1));
        assert_eq!(found.len(), 3);
        // Comparing `_id` with a number, or `ne`, doesn't narrow; the
        // entries reject what doesn't match, unread.
        let number = Filter::new().sort_asc("_id").limit(3).gt("_id", 5);
        assert!(walk(number, vec![], false, (1000, 0)).is_empty());
        let ne = Filter::new().sort_asc("_id").limit(3).ne("_id", ids[1]);
        assert_eq!(walk(ne, vec![], false, (4, 3)), [ids[0], ids[2], ids[3]]);
        // A collection that doesn't exist walks nothing.
        let none = db.collection::<Document>("none");
        let sorted = Filter::new().sort_desc("_id").limit(3);
        assert!(none.find(sorted).unwrap().is_empty());
    }

    /// SPEC §99.3: a read decodes whole only the documents it hands out —
    /// after the filter, a sort in memory, `skip` and `limit` — and `count`
    /// and `delete_many` none. Untyped, candidates from an index range are
    /// decoded whole once and checked on that (§99.2).
    #[test]
    fn only_what_is_handed_out_is_decoded_whole() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let typed = db.collection::<IdOnly>("docs");
        let ops = (0..1000)
            .map(|i| {
                let doc = object(vec![
                    ("n", Document::Int(i)),
                    ("pad", "x".repeat(50).into()),
                ]);
                WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();
        let decoded = |read: &dyn Fn() -> usize| {
            crate::data::DECODED.with(|n| n.set(0));
            let handed_out = read();
            (handed_out, crate::data::DECODED.with(|n| n.get()))
        };
        // Sorted in memory by an unindexed field: 1,000 checked on `n`.
        let top = || Filter::new().sort_desc("n").limit(20);
        assert_eq!(decoded(&|| docs.find(top()).unwrap().len()), (20, 20));
        assert_eq!(decoded(&|| typed.find(top()).unwrap().len()), (20, 20));
        let streamed = || typed.cursor(Filter::new().gt("n", 900)).unwrap().count();
        assert_eq!(decoded(&streamed), (99, 99));
        // Page 3 of 50: 50 read and decoded; the 100 skipped decided by
        // their entries (SPEC §100).
        let page = || Filter::new().sort_asc("_id").skip(100).limit(50);
        assert_eq!(decoded(&|| docs.find(page()).unwrap().len()), (50, 50));
        assert_eq!(records_read(|| drop(typed.find(page()).unwrap())), 50);
        let over = || Filter::new().gt("n", 499);
        assert_eq!(decoded(&|| docs.count(over()).unwrap()), (500, 0));
        assert_eq!(
            decoded(&|| docs.delete_many(Filter::new().lt("n", 10)).unwrap()),
            (10, 0)
        );
        // From an index range, untyped, a filter its entries don't decide:
        // each candidate once, whole.
        docs.ensure_index("n").unwrap();
        let high = || Filter::new().gte("n", 990).ne("n", 995).ne("pad", "");
        assert_eq!(decoded(&|| docs.find(high()).unwrap().len()), (9, 10));
        assert_eq!(decoded(&|| typed.find(high()).unwrap().len()), (9, 9));
        // One they decide: only the matches (SPEC §100).
        let decided = || Filter::new().gte("n", 990).ne("n", 995);
        assert_eq!(decoded(&|| docs.find(decided()).unwrap().len()), (9, 9));
    }

    /// SPEC §96.2: the walk by `_id` gives way as a walk on a secondary
    /// index does (§34.2) — to ids named, to an `Eq` or an OR on indexed
    /// fields, and without a limit — but not to a range.
    #[test]
    fn finding_by_id_or_by_value_beats_the_walk_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let (docs, ids) = thousand_by_id(&db);
        let by_id = |f: Filter| docs.explain(&f.sort_asc("_id").limit(5)).unwrap();
        let field = |name: &str| name.to_string();
        assert_eq!(by_id(Filter::new().eq("_id", ids[3])), QueryPlan::ById);
        // Two ids looked up, not a walk the OR can't narrow.
        let two = Filter::new().any_of([Condition::id(ids[3]), Condition::id(ids[900])]);
        let run = docs.explain_run(two.sort_asc("_id").limit(5)).unwrap();
        assert_eq!(run.plan, QueryPlan::ById);
        assert_eq!(run.stats.documents_examined, 2);
        let sort = SortWork {
            from_index: 0,
            in_memory: 1,
        };
        assert_eq!(run.sort, Some(sort));
        let group = Filter::new().eq("group", 3);
        assert_eq!(
            by_id(group),
            QueryPlan::Index {
                field: field("group")
            }
        );
        let either = [Condition::eq("group", 1), Condition::eq("n", 2)];
        let fields = vec![field("group"), field("n")];
        assert_eq!(
            by_id(Filter::new().any_of(either)),
            QueryPlan::IndexUnion { fields }
        );
        let range = Filter::new().gt("n", 10);
        assert_eq!(
            by_id(range),
            QueryPlan::IndexOrder {
                field: field("_id")
            }
        );
        let unlimited = Filter::new().sort_asc("_id");
        assert_eq!(docs.explain(&unlimited).unwrap(), QueryPlan::Scan);
    }

    /// A document for compound indexes (SPEC §43): `a` has few values (or
    /// is missing, null, or an array), `b` anything `random_value` makes — arrays,
    /// NaN, huge numbers, long strings included — `c` a small number.
    fn random_compound_document(rng: &mut XorShift) -> Document {
        let mut fields = Vec::new();
        match rng.below(10) {
            0 => {}
            1 => fields.push(("a", Document::Null)),
            // Sorts after everything (§34.1); a compound index holds it.
            2 => fields.push(("a", Document::Array(vec![Document::Int(1)]))),
            _ => fields.push(("a", Document::Int(rng.below(3) as i64))),
        }
        if rng.below(8) != 0 {
            fields.push(("b", random_value(rng)));
        }
        fields.push(("c", Document::Int(rng.below(3) as i64)));
        let pad = [0, 0, 1500, 4500][rng.below(4)];
        fields.push(("pad", Document::String("y".repeat(pad))));
        object(fields)
    }

    fn random_compound_filter(rng: &mut XorShift) -> Filter {
        let ops = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
        let mut conditions = Vec::new();
        for _ in 0..rng.below(4) {
            conditions.push(match rng.below(11) {
                0 | 1 => cond("a", Op::Eq, Document::Int(rng.below(4) as i64)),
                2 => cond(
                    "a",
                    ops[rng.below(5)].clone(),
                    Document::Int(rng.below(4) as i64),
                ),
                3 => cond("b", ops[rng.below(5)].clone(), random_value(rng)),
                4 => cond("c", Op::Eq, Document::Int(rng.below(3) as i64)),
                5 => cond("a", Op::Eq, Document::Null),
                6 => cond(["a", "b"][rng.below(2)], Op::Ne, Document::Null),
                7 => cond("b", Op::Eq, Document::Int(rng.below(11) as i64 - 5)),
                // Shape, not value (SPEC §45): never bounds an index.
                8 => match rng.below(2) {
                    0 => Condition::exists(["a", "b"][rng.below(2)]),
                    _ => Condition::missing(["a", "b"][rng.below(2)]),
                },
                9 => Condition::size(
                    ["a", "b"][rng.below(2)],
                    [Op::Eq, Op::Ne, Op::Gt][rng.below(3)].clone(),
                    rng.below(2),
                ),
                _ => cond("b", Op::Ne, random_value(rng)),
            });
        }
        // Up to three sort keys (SPEC §47), each field once, each way;
        // mostly one direction for all, so an index can serve several.
        let mut fields = vec!["a", "b", "c"];
        let order = [SortOrder::Asc, SortOrder::Desc][rng.below(2)];
        let sort = (0..rng.below(4))
            .map(|_| crate::query::Sort {
                field: fields.remove(rng.below(fields.len())).to_string(),
                order: match rng.below(4) {
                    0 => [SortOrder::Asc, SortOrder::Desc][rng.below(2)],
                    _ => order,
                },
            })
            .collect();
        let limits = [None, Some(0), Some(1), Some(3), Some(10), Some(1000)];
        let skips = [0, 0, 0, 1, 4, 30];
        Filter {
            conditions,
            sort,
            skip: skips[rng.below(skips.len())],
            limit: limits[rng.below(limits.len())],
        }
    }

    /// Every random filter on compound indexes `(a, b)` and `(a, b, c)`
    /// must find what sorting a scan in memory finds — same documents,
    /// and for a sorted filter the same order — through inserts, updates
    /// that move documents, deletes and a reopen; the compound plans must
    /// actually run.
    #[test]
    fn compound_finds_match_full_scans_through_every_kind_of_write() {
        let plain = IndexOptions::default();
        compound_finds_match_full_scans(
            &[(&["a", "b"], plain), (&["a", "b", "c"], plain)],
            &[
                r#"Index { field: "(a, b)" }"#,
                r#"IndexOrder { field: "(a, b)" }"#,
                r#"IndexOrder { field: "(a, b, c)" }"#,
            ],
        );
    }

    /// Sparse, a compound index leaves out a document null in all of its
    /// fields, and a one-field index every null (SPEC §44).
    #[test]
    fn sparse_compound_finds_match_full_scans_through_every_kind_of_write() {
        compound_finds_match_full_scans(
            &[
                (&["a", "b"], SPARSE),
                (&["a", "b", "c"], SPARSE),
                (&["b"], SPARSE),
            ],
            &[
                r#"Index { field: "(a, b)" }"#,
                r#"IndexOrder { field: "(a, b)" }"#,
                r#"IndexOrder { field: "(a, b, c)" }"#,
                r#"Index { field: "b" }"#,
                r#"IndexOrder { field: "b" }"#,
                "Scan",
            ],
        );
    }

    fn compound_finds_match_full_scans(
        indexes: &[(&[&str], IndexOptions)],
        expected_plans: &[&str],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut rng = XorShift(0xBB67_AE85_84CA_A73B);
        let check = |docs: &Collection<Document>, rng: &mut XorShift| {
            assert_consistent(docs.db());
            let mut all = docs.find_with_ids(Filter::default()).unwrap();
            all.sort_by_key(|(id, _)| *id);
            let mut plans = std::collections::HashSet::new();
            for _ in 0..400 {
                let f = random_compound_filter(rng);
                let ids = |found: Vec<(DocId, Document)>| {
                    let mut ids: Vec<DocId> = found.into_iter().map(|(id, _)| id).collect();
                    if f.sort.is_empty() {
                        ids.sort();
                    }
                    ids
                };
                let expected = ids(f.apply_to(all.clone(), |(_, doc)| doc));
                let plan = explained(docs, &f);
                let found = ids(docs.find_with_ids(f.clone()).unwrap());
                // Typed (SPEC §99), the same documents the same way.
                let typed = typed_ids(docs, &f).into_iter();
                assert_eq!(ids(typed.map(|id| (id, Document::Null)).collect()), found);
                if f.sort.is_empty() && (f.limit.is_some() || f.skip > 0) {
                    // Which documents a skip or limit without a sort keeps
                    // isn't promised: any that match, as many as they allow.
                    let unlimited = Filter {
                        skip: 0,
                        limit: None,
                        ..f.clone()
                    };
                    let matching = ids(unlimited.apply_to(all.clone(), |(_, doc)| doc));
                    assert_eq!(found.len(), expected.len(), "{f:?} via {plan:?}");
                    assert!(found.iter().all(|id| matching.contains(id)), "{f:?}");
                } else {
                    assert_eq!(found, expected, "{f:?} via {plan:?}");
                }
                plans.insert(format!("{plan:?}"));
            }
            for plan in expected_plans {
                assert!(plans.contains(*plan), "{plan} never ran: {plans:?}");
            }
        };
        {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            let mut ids = Vec::new();
            let mut insert = |rng: &mut XorShift, count| {
                for _ in 0..count / 25 {
                    let ops = (0..25)
                        .map(|_| {
                            let id = db.id_gen().generate();
                            ids.push(id);
                            WriteOp::Insert("docs".into(), id, random_compound_document(rng))
                        })
                        .collect();
                    db.write_batch(ops).unwrap();
                }
            };
            insert(&mut rng, 150);
            for (fields, options) in indexes {
                assert!(docs.ensure_index_with(*fields, *options).unwrap());
            }
            insert(&mut rng, 150);
            for _ in 0..6 {
                let ops = (0..25)
                    .map(|_| {
                        let id = ids[rng.below(ids.len())];
                        let new = match docs.get(&id).unwrap() {
                            Some(Document::Object(mut fields)) if rng.below(2) == 0 => {
                                let pad = "z".repeat([0, 4500, 9000][rng.below(3)]);
                                fields.insert("pad".into(), Document::String(pad));
                                Document::Object(fields)
                            }
                            _ => random_compound_document(&mut rng),
                        };
                        WriteOp::Update("docs".into(), id, new)
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            let deleted = (0..60).map(|_| ids.swap_remove(rng.below(ids.len())));
            let ops = deleted.map(|id| WriteOp::Delete("docs".into(), id));
            db.write_batch(ops.collect()).unwrap();
            check(&docs, &mut rng);
        }
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        let names: Vec<String> = indexes
            .iter()
            .map(|(fields, _)| match fields {
                [field] => field.to_string(),
                fields => format!("({})", fields.join(", ")),
            })
            .collect();
        assert_eq!(docs.index_names().unwrap(), names);
        check(&docs, &mut rng);
    }

    /// `explain`'s plan for `f`, after checking `explain_run` against it
    /// and against `find` (SPEC §94.7): the same plan; as many returned
    /// and as many documents read as `find` returns and reads; counts
    /// that agree with each other and with the plan; and every document
    /// found within the bounds of a read.
    fn explained(docs: &Collection<Document>, f: &Filter) -> QueryPlan {
        let plan = docs.explain(f).unwrap();
        let run = docs.explain_run(f.clone()).unwrap();
        assert_eq!(run.plan, plan, "{f:?}");
        let mut found = Vec::new();
        let read = records_read(|| found = docs.find_with_ids(f.clone()).unwrap());
        let stats = run.stats;
        assert_eq!(stats.documents_returned, found.len() as u64, "{f:?}");
        assert_eq!(stats.documents_examined, read as u64, "{f:?} via {plan:?}");
        let entries: u64 = run.reads.iter().map(|read| read.entries).sum();
        assert_eq!(stats.keys_examined, entries);
        assert!(
            stats.keys_examined >= stats.documents_examined,
            "{f:?}: {run:?}"
        );
        // Where entries decide (SPEC §100), a match need not be read;
        // elsewhere every match was.
        match run.reads.iter().any(|read| read.covering) {
            true => assert!(stats.documents_matched <= stats.keys_examined),
            false => assert!(
                stats.documents_matched <= stats.documents_examined,
                "{f:?}: {run:?}"
            ),
        }
        // A read covers exactly where its index holds what the filter
        // names, and isn't multikey.
        let indexes = docs.indexes().unwrap();
        for (i, read) in run.reads.iter().enumerate() {
            let fields: Vec<String> = match read.index.as_str() {
                "_id" => Vec::new(),
                name => indexes
                    .iter()
                    .find(|index| index.name() == name)
                    .map(|index| index.fields().to_vec())
                    .unwrap(),
            };
            let multi = fields.iter().any(|field| crate::query::is_multi(field));
            let covers = !multi && f.covered_by(&fields, false).is_some();
            let several = run
                .reads
                .iter()
                .any(|other| other.index != read.index && other.index != "_id");
            // The scan after a walk for what its index lacks reads
            // documents (§34.2).
            let after_walk = i > 0 && read.index == "_id";
            assert_eq!(
                read.covering,
                covers && !several && !after_walk,
                "{f:?}: {run:?}"
            );
        }
        let unlimited = Filter {
            skip: 0,
            limit: None,
            ..f.clone()
        };
        let matching = docs.count(unlimited).unwrap() as u64;
        match plan {
            // Stops once it has skip plus limit.
            QueryPlan::IndexOrder { .. } => {
                let wanted = match f.limit.unwrap() {
                    0 => 0, // reads nothing at all
                    limit => (f.skip as u64).saturating_add(limit as u64),
                };
                assert!(
                    stats.documents_matched >= matching.min(wanted),
                    "{f:?}: {run:?}"
                );
                assert!(stats.documents_matched <= matching, "{f:?}: {run:?}");
            }
            _ => assert_eq!(stats.documents_matched, matching, "{f:?}: {run:?}"),
        }
        let names: Vec<&str> = run.reads.iter().map(|read| read.index.as_str()).collect();
        match &plan {
            QueryPlan::Scan | QueryPlan::ById => {
                assert!(names.is_empty() || names == ["_id"], "{f:?}: {run:?}");
            }
            QueryPlan::Index { field } => assert_eq!(names, [field.as_str()]),
            QueryPlan::IndexUnion { fields } => assert_eq!(names, *fields),
            QueryPlan::IndexOrder { .. } if f.limit == Some(0) => assert!(names.is_empty()),
            QueryPlan::IndexOrder { field } => {
                assert_eq!(names[0], field);
                assert!(
                    names[1..].is_empty() || names[1..] == ["_id"],
                    "{f:?}: {run:?}"
                );
                let descending = f.sort[0].order == SortOrder::Desc;
                assert_eq!(run.reads[0].backward, descending);
            }
        }
        if !matches!(plan, QueryPlan::Scan | QueryPlan::IndexOrder { .. }) {
            for read in &run.reads {
                assert!(!read.bounds.is_empty(), "{f:?}: {run:?}");
            }
        }
        // A document found is in some read: it meets all of its bounds.
        for (_id, doc) in &found {
            assert!(
                run.reads
                    .iter()
                    .any(|read| read.bounds.iter().all(|bound| bound.matches(doc))),
                "{doc:?} outside every read of {run:?}"
            );
        }
        match &run.sort {
            None => assert!(f.sort.is_empty()),
            Some(sort) => {
                assert_eq!(sort.from_index + sort.in_memory, f.sort.len());
                let ordered = matches!(plan, QueryPlan::IndexOrder { .. });
                assert_eq!(sort.from_index > 0, ordered, "{f:?}: {run:?}");
            }
        }
        plan
    }

    /// Only a document's id: any document reads as one, every other
    /// field skipped (SPEC §99.4).
    #[derive(Serialize, Deserialize)]
    struct IdOnly {
        #[serde(rename = "_id")]
        id: DocId,
    }

    /// The ids `docs` finds for `f` through the typed path, from `find`
    /// and from `cursor`, which must agree.
    fn typed_ids(docs: &Collection<Document>, f: &Filter) -> Vec<DocId> {
        let typed = docs.db().collection::<IdOnly>(&docs.name);
        let found: Vec<DocId> = typed
            .find(f.clone())
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        let streamed: Vec<DocId> = typed
            .cursor(f.clone())
            .unwrap()
            .map(|d| d.unwrap().id)
            .collect();
        if !f.sort.is_empty() || (f.limit.is_none() && f.skip == 0) {
            let sorted = |mut ids: Vec<DocId>| {
                if f.sort.is_empty() {
                    ids.sort();
                }
                ids
            };
            assert_eq!(sorted(streamed), sorted(found.clone()), "{f:?}");
        }
        found
    }

    fn records_read(f: impl FnOnce()) -> usize {
        crate::data::RECORDS_READ.with(|n| n.set(0));
        f();
        crate::data::RECORDS_READ.with(|n| n.get())
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Task {
        status: String,
        created: i64,
        tenant: String,
    }

    /// The query compound indexes are for (SPEC §43): the oldest 20
    /// queued tasks — found by status and read in order of creation at
    /// once, 20 documents read of 3000.
    #[test]
    fn the_oldest_queued_tasks_are_read_through_status_and_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let tasks = db.collection::<Task>("tasks");
        let mut batch = db.batch();
        for n in 0..3000i64 {
            let status = ["Queued", "Running", "Done"][(n * 7 % 3) as usize];
            let task = Task {
                status: status.into(),
                // Not in insertion order.
                created: (n * 7919) % 3000,
                tenant: format!("t{}", n % 5),
            };
            batch.insert(&tasks, task).unwrap();
        }
        batch.commit().unwrap();
        let oldest_queued = || {
            Filter::new()
                .eq("status", "Queued")
                .sort_asc("created")
                .limit(20)
        };
        let expected = oldest_queued().apply(
            tasks
                .find(Filter::new())
                .unwrap()
                .into_iter()
                .map(|t| crate::serde_bridge::to_document(&t).unwrap()),
        );

        tasks.ensure_index("status").unwrap();
        tasks.ensure_index("created").unwrap();
        let before = records_read(|| {
            tasks.find(oldest_queued()).unwrap();
        });
        assert!(before > 900, "{before}");

        tasks.ensure_index(["status", "created"]).unwrap();
        assert_eq!(
            tasks.explain(&oldest_queued()).unwrap(),
            QueryPlan::IndexOrder {
                field: "(status, created)".into()
            }
        );
        let mut found = Vec::new();
        let reads = records_read(|| found = tasks.find(oldest_queued()).unwrap());
        assert_eq!(reads, 20);
        let found: Vec<Document> = found
            .iter()
            .map(|t| crate::serde_bridge::to_document(t).unwrap())
            .collect();
        assert_eq!(found, expected);

        // Newest first, and a range on `created` within the status.
        let newest = Filter::new()
            .eq("status", "Done")
            .lt("created", 100)
            .sort_desc("created")
            .limit(5);
        let created: Vec<i64> = tasks
            .find(newest)
            .unwrap()
            .iter()
            .map(|t| t.created)
            .collect();
        let mut all_done: Vec<i64> = tasks
            .find(Filter::new().eq("status", "Done"))
            .unwrap()
            .iter()
            .map(|t| t.created)
            .filter(|c| *c < 100)
            .collect();
        all_done.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(created, all_done[..5]);
        assert_consistent(&db);
    }

    /// Values no one-field index holds (arrays here) sort after all the
    /// others, found by a scan after the index (SPEC §34.2) — and among
    /// themselves by the next sort key (SPEC §47.3), not in id order:
    /// the scan reads them all before taking the limit.
    #[test]
    fn unordered_values_after_the_index_are_sorted_by_the_next_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        for w in 0..15i64 {
            let v = match w < 5 {
                true => Document::Int(w),
                false => Document::Array(vec![Document::Int(w)]),
            };
            docs.insert(object(vec![("v", v), ("w", Document::Int(w))]))
                .unwrap();
        }
        let f = Filter::new().sort_asc("v").then_desc("w").limit(8);
        assert_eq!(
            docs.explain(&f).unwrap(),
            QueryPlan::IndexOrder { field: "v".into() }
        );
        let w = |found: Vec<Document>| -> Vec<Document> {
            found
                .iter()
                .map(|d| crate::query::value_or_null(d, "w").clone())
                .collect()
        };
        let all = docs.find(Filter::new()).unwrap();
        let expected = w(f.apply(all));
        assert_eq!(expected, [0, 1, 2, 3, 4, 14, 13, 12].map(Document::Int));
        assert_eq!(w(docs.find(f).unwrap()), expected);
    }

    /// Sorting by several fields (SPEC §47) through `(status, created)`:
    /// both keys in one direction are read in order, 20 documents read
    /// for 20 results; with the second key the other way, the index gives
    /// the status order and each status is sorted in memory; the index on
    /// `created` alone serves a sort by `created, tenant` — ties by
    /// tenant, in memory. Always what sorting everything finds.
    #[test]
    fn tasks_sort_by_status_then_by_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let tasks = db.collection::<Task>("tasks");
        let mut batch = db.batch();
        for n in 0..3000i64 {
            let task = Task {
                status: ["Queued", "Running", "Done"][(n * 7 % 3) as usize].into(),
                // Each value 5 times, so `created` has ties.
                created: (n * 7919) % 600,
                tenant: format!("t{}", n * 13 % 5),
            };
            batch.insert(&tasks, task).unwrap();
        }
        batch.commit().unwrap();
        tasks.ensure_index(["status", "created"]).unwrap();
        tasks.ensure_index("created").unwrap();
        let all: Vec<Document> = tasks
            .find(Filter::new())
            .unwrap()
            .iter()
            .map(|t| crate::serde_bridge::to_document(t).unwrap())
            .collect();
        let check = |f: Filter, plan: &str, max_reads: usize| {
            assert_eq!(
                tasks.explain(&f).unwrap(),
                QueryPlan::IndexOrder { field: plan.into() },
                "{f:?}"
            );
            let mut found = Vec::new();
            let reads = records_read(|| found = tasks.find(f.clone()).unwrap());
            assert!(reads <= max_reads, "{f:?}: {reads} reads");
            let found: Vec<Document> = found
                .iter()
                .map(|t| crate::serde_bridge::to_document(t).unwrap())
                .collect();
            assert_eq!(found, f.apply(all.clone()), "{f:?}");
        };
        let by_both = Filter::new()
            .sort_asc("status")
            .then_asc("created")
            .limit(20);
        check(by_both, "(status, created)", 20);
        let by_both_desc = Filter::new()
            .sort_desc("status")
            .then_desc("created")
            .limit(20);
        check(by_both_desc, "(status, created)", 20);
        // "Done" first, newest first: the 1000 done tasks are sorted.
        let newest_per_status = Filter::new()
            .sort_asc("status")
            .then_desc("created")
            .limit(20);
        check(newest_per_status, "(status, created)", 1000);
        // Five tasks per `created`: each five sorted by tenant.
        let by_created_tenant = Filter::new()
            .sort_asc("created")
            .then_desc("tenant")
            .limit(12);
        check(by_created_tenant, "created", 15);
    }

    /// Unique in all its fields at once (SPEC §43.4): an email may repeat
    /// across tenants, not within one; a null in any field exempts, as in
    /// SQL.
    #[test]
    fn a_unique_compound_index_refuses_only_the_same_values_in_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let user = |tenant: Document, email: &str| {
            object(vec![("tenant", tenant), ("email", email.into())])
        };
        users
            .ensure_index_with(["tenant", "email"], crate::IndexOptions::new().unique())
            .unwrap();
        let ada = users.insert(user("t1".into(), "ada@x")).unwrap();
        users.insert(user("t2".into(), "ada@x")).unwrap();
        users.insert(user("t1".into(), "bob@x")).unwrap();
        match users.insert(user("t1".into(), "ada@x")) {
            Err(crate::Error::DuplicateValue {
                existing, field, ..
            }) => {
                assert_eq!((existing, field.as_str()), (ada, "(tenant, email)"));
            }
            other => panic!("expected DuplicateValue, got {other:?}"),
        }
        // Equal as `Eq` sees it: `1` and `1.0`.
        users.insert(user(1.into(), "cy@x")).unwrap();
        assert!(users.insert(user(Document::Float(1.0), "cy@x")).is_err());
        // A null, or a missing field, in any of them: exempt.
        for _ in 0..2 {
            users.insert(user(Document::Null, "ada@x")).unwrap();
            users
                .insert(object(vec![("email", "ada@x".into())]))
                .unwrap();
        }
        // Ada moves to t3; then t1's ada@x is free again.
        assert!(users.update(&ada, user("t3".into(), "ada@x")).unwrap());
        users.insert(user("t1".into(), "ada@x")).unwrap();
        assert_consistent(&db);

        // Built over documents that share both values: refused.
        let other = db.collection::<Document>("other");
        other.insert(user("t".into(), "e")).unwrap();
        other.insert(user("t".into(), "e")).unwrap();
        assert!(matches!(
            other.ensure_index_with(["tenant", "email"], crate::IndexOptions::new().unique()),
            Err(crate::Error::DuplicateValue { .. })
        ));
        assert_eq!(other.index_names().unwrap(), Vec::<String>::new());
    }

    /// Strings longer than a compound key's share for them (501 bytes of
    /// two fields) are cut in the keys; conditions on them must be cut the
    /// same way to find them — `Eq`, ranges, and the sorted read.
    #[test]
    fn long_strings_in_a_compound_index_are_found_by_every_condition() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let long = |end: &str| Document::String("x".repeat(800) + end);
        for (a, end) in [(1, "a"), (1, "b"), (1, "c"), (2, "b")] {
            docs.insert(object(vec![("a", a.into()), ("b", long(end))]))
                .unwrap();
        }
        docs.ensure_index(["a", "b"]).unwrap();
        let ends = |f: Filter| -> Vec<String> {
            docs.find(f)
                .unwrap()
                .iter()
                .map(|doc| match crate::query::field_value(doc, "b") {
                    Some(Document::String(b)) => b[800..].to_string(),
                    other => panic!("{other:?}"),
                })
                .collect()
        };
        let a1 = || Filter::new().eq("a", 1);
        assert_eq!(ends(a1().eq("b", long("b"))), ["b"]);
        assert_eq!(ends(a1().gt("b", long("a")).sort_asc("b")), ["b", "c"]);
        assert_eq!(ends(a1().gte("b", long("b")).sort_asc("b")), ["b", "c"]);
        assert_eq!(ends(a1().lt("b", long("c")).sort_asc("b")), ["a", "b"]);
        assert_eq!(ends(a1().lte("b", long("b")).sort_desc("b")), ["b", "a"]);
        let newest = a1().sort_desc("b").limit(2);
        assert_eq!(
            docs.explain(&newest).unwrap(),
            QueryPlan::IndexOrder {
                field: "(a, b)".into()
            }
        );
        assert_eq!(ends(newest), ["c", "b"]);
        assert_eq!(
            docs.explain(&a1().gt("b", long("a"))).unwrap(),
            QueryPlan::Index {
                field: "(a, b)".into()
            }
        );
    }

    #[test]
    fn compound_indexes_are_checked_created_dropped_exported_and_imported() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let too_many = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
        for bad in [
            &[][..],
            &too_many[..],
            &["a", "tags[*]"],
            &["a", "a"],
            &["a", "_id"],
            &["a", ""],
        ] {
            assert!(docs.ensure_index(bad).is_err(), "{bad:?}");
        }
        assert!(docs.ensure_index(&too_many[..8]).unwrap());
        assert!(docs.ensure_index(["a", "b"]).unwrap());
        assert!(!docs.ensure_index(["a", "b"]).unwrap());
        // Another order is another index.
        assert!(docs.ensure_index(["b", "a"]).unwrap());
        assert!(
            docs.ensure_index_with(["a", "c"], crate::IndexOptions::new().unique())
                .unwrap()
        );
        assert!(
            docs.ensure_index_with(["a", "b"], crate::IndexOptions::new().unique())
                .is_err(),
            "exists, not unique"
        );
        docs.insert(object(vec![
            ("a", 1.into()),
            ("b", 2.into()),
            ("c", 3.into()),
        ]))
        .unwrap();
        assert_eq!(
            docs.index_names().unwrap(),
            ["(a, b, c, d, e, f, g, h)", "(a, b)", "(b, a)", "(a, c)"]
        );
        assert_eq!(docs.unique_indexes().unwrap(), ["(a, c)"]);

        let mut export = Vec::new();
        db.export(&mut export).unwrap();
        let text = String::from_utf8(export.clone()).unwrap();
        assert!(text.contains(r#"["a","b"]"#), "{text}");
        assert!(
            text.contains(r#"{"fields":["a","c"],"unique":true}"#),
            "{text}"
        );
        let copy = Database::open(dir.path().join("copy.trunkdb")).unwrap();
        copy.import(export.as_slice()).unwrap();
        let copied = copy.collection::<Document>("docs");
        assert_eq!(copied.index_names().unwrap(), docs.index_names().unwrap());
        assert_eq!(copied.unique_indexes().unwrap(), ["(a, c)"]);
        assert_consistent(&copy);

        assert!(docs.drop_index(["b", "a"]).unwrap());
        assert!(!docs.drop_index(["b", "a"]).unwrap());
        assert!(!docs.drop_index("b").unwrap(), "no index on b alone");
        assert_eq!(docs.index_names().unwrap().len(), 3);
        assert_consistent(&db);
    }

    /// SPEC §72: `skip` passes over matches after the sort, before the
    /// limit — in `find`, `count`, `find_one`, a cursor and `delete_many`;
    /// through an index, the reading stops after skip plus limit.
    #[test]
    fn skip_and_limit_page_through_the_matches() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        let mut batch = db.batch();
        for age in 0..1000 {
            batch
                .insert(&users, user(&format!("user {age}"), age))
                .unwrap();
        }
        batch.commit().unwrap();
        let page = |p: usize| by_age(SortOrder::Asc).skip(p * 20).limit(20);
        let ages = |found: &[User]| found.iter().map(|u| u.age).collect::<Vec<_>>();

        let mut found = Vec::new();
        let reads = records_read(|| found = users.find(page(2)).unwrap());
        assert_eq!((reads, ages(&found)), (1000, (40..60).collect()));

        // Walked as far as skip plus limit; the skipped decided by their
        // entries, unread (SPEC §100).
        users.ensure_index("age").unwrap();
        let reads = records_read(|| found = users.find(page(2)).unwrap());
        assert_eq!(
            (reads, ages(&found)),
            (20, (40..60).collect()),
            "limit read"
        );
        let desc = by_age(SortOrder::Desc).skip(995).limit(20);
        assert_eq!(ages(&users.find(desc).unwrap()), [4, 3, 2, 1, 0]);
        assert!(users.find(page(50)).unwrap().is_empty());
        let beyond = by_age(SortOrder::Asc).skip(usize::MAX).limit(20);
        assert!(users.find(beyond).unwrap().is_empty());
        let unlimited = by_age(SortOrder::Asc).skip(990).limit(None);
        assert_eq!(
            ages(&users.find(unlimited).unwrap()),
            (990..1000).collect::<Vec<_>>()
        );

        // `count` counts what `find` returns; without skip and limit, the total.
        assert_eq!(users.count(page(2)).unwrap(), 20);
        assert_eq!(users.count(page(49)).unwrap(), 20);
        assert_eq!(users.count(Filter::new().skip(990)).unwrap(), 10);
        assert_eq!(users.count(Filter::new().skip(2000)).unwrap(), 0);
        let under_ten = filter(vec![cond("age", Op::Lt, Document::Int(10))]);
        assert_eq!(users.count(under_ten.clone().skip(4).limit(5)).unwrap(), 5);
        assert_eq!(users.count(under_ten.clone().skip(8).limit(5)).unwrap(), 2);

        // `find_one` is the first after the skipped.
        let seventh = users.find_one(by_age(SortOrder::Asc).skip(7)).unwrap();
        assert_eq!(seventh.map(|u| u.age), Some(7));

        // An unsorted cursor streams, and passes over the first matches.
        let all: Vec<i64> = users
            .cursor(under_ten.clone())
            .unwrap()
            .map(|r| r.unwrap().age)
            .collect();
        let mut rest: Vec<i64> = Vec::new();
        let reads = records_read(|| {
            rest = users
                .cursor(under_ten.clone().skip(3).limit(4))
                .unwrap()
                .map(|r| r.unwrap().age)
                .collect();
        });
        assert_eq!((all.len(), &rest[..]), (10, &all[3..7]));
        // The entries decide: the three skipped aren't read (SPEC §100).
        assert_eq!(reads, 4);

        // `delete_many` deletes the page `find` returns.
        assert_eq!(
            users
                .delete_many(by_age(SortOrder::Desc).skip(1).limit(3))
                .unwrap(),
            3
        );
        let left = ages(&users.find(by_age(SortOrder::Desc).limit(3)).unwrap());
        assert_eq!(left, [999, 995, 994]);
        assert_consistent(&db);
    }

    #[test]
    fn a_limit_stops_the_reading_when_an_index_gives_the_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        let mut batch = db.batch();
        for age in 0..1000 {
            batch
                .insert(&users, user(&format!("user {age}"), age))
                .unwrap();
        }
        batch.commit().unwrap();
        let oldest = |limit| Filter {
            limit,
            ..by_age(SortOrder::Desc)
        };

        // Without an index every document is read, then sorted.
        let reads = records_read(|| assert_eq!(users.find(oldest(Some(5))).unwrap().len(), 5));
        assert_eq!(reads, 1000);

        users.ensure_index("age").unwrap();
        assert_eq!(
            users.explain(&oldest(Some(5))).unwrap(),
            QueryPlan::IndexOrder {
                field: "age".to_string()
            }
        );
        let mut found = Vec::new();
        let reads = records_read(|| found = users.find(oldest(Some(5))).unwrap());
        assert_eq!(reads, 5);
        let ages: Vec<i64> = found.iter().map(|u| u.age).collect();
        assert_eq!(ages, [999, 998, 997, 996, 995]);

        // `find_one` with a sort reads one.
        let mut first = None;
        let reads = records_read(|| first = users.find_one(by_age(SortOrder::Asc)).unwrap());
        assert_eq!((reads, first.map(|u| u.age)), (1, Some(0)));

        // A range on the sort field narrows the walk, and rules out the
        // values no index holds — so running out of matches doesn't
        // scan for them.
        let young = Filter {
            limit: Some(50),
            ..filter(vec![cond("age", Op::Lt, Document::Int(10))])
        };
        let young = Filter {
            sort: by_age(SortOrder::Asc).sort,
            ..young
        };
        // The range includes its bound, age 10 (§28.4), whose entry
        // rejects it unread (SPEC §100).
        let reads = records_read(|| assert_eq!(users.find(young).unwrap().len(), 10));
        assert_eq!(reads, 10);

        // Without such a range, running out means one scan for them —
        // here a document whose age is an array, sorted after the rest.
        let docs = db.collection::<Document>("users");
        docs.insert(object(vec![("age", Document::Array(vec![]))]))
            .unwrap();
        let all = docs
            .find(Filter {
                limit: Some(2000),
                ..by_age(SortOrder::Desc)
            })
            .unwrap();
        assert_eq!(all.len(), 1001);
        assert!(matches!(
            crate::query::field_value(all.last().unwrap(), "age"),
            Some(Document::Array(_))
        ));
        // Without a limit, nothing stops early: the plan is a scan.
        assert_eq!(users.explain(&oldest(None)).unwrap(), QueryPlan::Scan);
    }

    /// The builder end to end (SPEC §35): conditions, a sort, a limit and
    /// an `Option` passed as it is — `None` finds what serde stored as
    /// `None`.
    #[test]
    fn a_built_filter_finds_through_a_typed_collection() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Member {
            name: String,
            age: i64,
            nick: Option<String>,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let members = db.collection::<Member>("members");
        for (name, age, nick) in [("Ada", 36, None), ("Bob", 41, Some("B")), ("Cy", 17, None)] {
            let nick = nick.map(String::from);
            members
                .insert(Member {
                    name: name.into(),
                    age,
                    nick,
                })
                .unwrap();
        }
        members.ensure_index("age").unwrap();
        let names = |filter| -> Vec<String> {
            members
                .find(filter)
                .unwrap()
                .into_iter()
                .map(|m| m.name)
                .collect()
        };

        let wanted_nick: Option<&str> = None;
        assert_eq!(
            names(Filter::new().eq("nick", wanted_nick).sort_asc("name")),
            ["Ada", "Cy"]
        );
        assert_eq!(
            names(Filter::new().gte("age", 18).sort_desc("age").limit(1)),
            ["Bob"]
        );
        assert_eq!(
            names(Filter::new().contains("name", "a").is_null("nick")),
            ["Ada"]
        );
        let oldest_adult = Filter::new().gte("age", 18).sort_desc("age").limit(1);
        assert_eq!(
            members.explain(&oldest_adult).unwrap(),
            QueryPlan::IndexOrder {
                field: "age".to_string()
            }
        );
    }

    // --- OR, AND and NOT (SPEC §36) ---

    /// A random condition up to `depth` levels deep: comparisons with
    /// every operator on the indexed `v` and `w`, the unindexed `pad` and
    /// a field no document has — combined by ANDs, ORs (empty ones too)
    /// and NOTs.
    fn random_condition(rng: &mut XorShift, depth: usize) -> Condition {
        if depth == 0 || rng.below(3) == 0 {
            let ops = [
                Op::Eq,
                Op::Ne,
                Op::Lt,
                Op::Lte,
                Op::Gt,
                Op::Gte,
                Op::Contains,
            ];
            let op = ops[rng.below(ops.len())].clone();
            return match rng.below(6) {
                0..=2 => cond("v", op, random_value(rng)),
                3 => cond("w", op, Document::Int(rng.below(4) as i64)),
                4 => cond("pad", op, Document::String(String::new())),
                _ => cond("nowhere", op, random_value(rng)),
            };
        }
        let children = (0..rng.below(4))
            .map(|_| random_condition(rng, depth - 1))
            .collect::<Vec<_>>();
        match rng.below(5) {
            0 => Condition::All(children),
            1 => !random_condition(rng, depth - 1),
            _ => Condition::Any(children),
        }
    }

    /// Every random nested filter must find, count and stream exactly
    /// what checking every document finds — whichever plan it runs,
    /// also when it names documents by id.
    fn assert_nested_filters_agree_with_a_scan(docs: &Collection<Document>, rng: &mut XorShift) {
        assert_consistent(docs.db());
        let mut all = docs.find_with_ids(Filter::default()).unwrap();
        all.sort_by_key(|(id, _doc)| *id);
        let mut plans = std::collections::HashSet::new();
        for _ in 0..400 {
            let mut f = Filter::new();
            for _ in 0..1 + rng.below(2) {
                f = f.and(random_condition(rng, 3));
            }
            if rng.below(4) == 0 {
                // Documents named by id (SPEC §61): one or an OR of a few,
                // now and then one that doesn't exist.
                let named = (0..1 + rng.below(3))
                    .map(|_| match rng.below(5) {
                        0 => Condition::eq("_id", DocId([0xAB; 16])),
                        _ => Condition::eq("_id", all[rng.below(all.len())].0),
                    })
                    .collect::<Vec<_>>();
                f = f.and(Condition::Any(named));
            }
            if rng.below(3) == 0 {
                f = f.sort_desc("v").limit([1, 5, 50][rng.below(3)]);
                // Ties in `v` by `w` (SPEC §47): the index on `v` gives
                // the order of `v` only, `w` is sorted in memory.
                if rng.below(2) == 0 {
                    f = f.then_by("w", [SortOrder::Asc, SortOrder::Desc][rng.below(2)]);
                }
            }
            let ids = |found: Vec<(DocId, Document)>| -> Vec<DocId> {
                found.into_iter().map(|(id, _)| id).collect()
            };
            let mut expected = ids(f.apply_to(all.clone(), |(_, doc)| doc));
            let plan = explained(docs, &f);
            let mut found = ids(docs.find_with_ids(f.clone()).unwrap());
            let streamed = docs
                .cursor(f.clone())
                .unwrap()
                .collect::<crate::Result<Vec<_>>>();
            // A cursor hands out documents, each with its `_id` (SPEC §74).
            let mut streamed: Vec<DocId> = streamed
                .unwrap()
                .iter()
                .map(|doc| match doc {
                    Document::Object(fields) => match fields["_id"] {
                        Document::Id(id) => id,
                        _ => panic!("no id in {doc}"),
                    },
                    _ => panic!("not an object: {doc}"),
                })
                .collect();
            if f.sort.is_empty() {
                expected.sort();
                found.sort();
                streamed.sort();
            }
            assert_eq!(found, expected, "{f:?} via {plan:?}");
            assert_eq!(streamed, expected, "cursor: {f:?} via {plan:?}");
            let counted = docs
                .count(Filter {
                    sort: Vec::new(),
                    ..f.clone()
                })
                .unwrap();
            assert_eq!(counted, expected.len(), "count: {f:?} via {plan:?}");
            plans.insert(std::mem::discriminant(&plan));
        }
        assert_eq!(
            plans.len(),
            5,
            "scan, index, union, order and by id all ran"
        );
    }

    #[test]
    fn nested_filters_find_what_a_scan_finds_on_every_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut rng = XorShift(0x5151_7A7A_0F0F_3C3C);
        {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            docs.ensure_index("v").unwrap();
            docs.ensure_index("w").unwrap();
            for _ in 0..10 {
                let ops = (0..25)
                    .map(|_| {
                        let doc = random_document(&mut rng);
                        WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
                    })
                    .collect();
                db.write_batch(ops).unwrap();
            }
            assert_nested_filters_agree_with_a_scan(&docs, &mut rng);
        }
        let db = Database::open(&path).unwrap();
        assert_nested_filters_agree_with_a_scan(&db.collection("docs"), &mut rng);
    }

    /// SPEC §100: where the entries decide, a document is read only to be
    /// handed out — and where one entry can't, that document alone.
    #[test]
    fn where_entries_decide_only_what_is_handed_out_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("tasks");
        let ops = (0..2000)
            .map(|i| {
                let doc = object(vec![
                    ("tenant", format!("t{}", i % 10).into()),
                    ("status", ["Queued", "Done"][i as usize % 2].into()),
                    ("created", Document::Int(i)),
                    ("tries", Document::Int(i % 7)),
                ]);
                WriteOp::Insert("tasks".into(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();
        docs.ensure_index("tenant").unwrap();
        docs.ensure_index(["status", "created"]).unwrap();
        let reads = |read: &dyn Fn() -> usize| {
            let mut n = 0;
            let read = records_read(|| n = read());
            (n, read)
        };
        let tenant = || Filter::new().eq("tenant", "t3");
        let found = |f: Filter| reads(&|| docs.find(f.clone()).unwrap().len());
        // The total behind a page: no document read.
        assert_eq!(reads(&|| docs.count(tenant()).unwrap()), (200, 0));
        let queued = Filter::new().eq("status", "Queued").gte("created", 100);
        assert_eq!(reads(&|| docs.count(queued.clone()).unwrap()), (950, 0));
        // Page 50 of 20, in index order and in `_id` order: 20 read.
        let page = |f: Filter| f.skip(49 * 20).limit(20);
        let by_created = page(Filter::new().eq("status", "Queued").sort_asc("created"));
        assert_eq!(found(by_created), (20, 20));
        assert_eq!(found(page(Filter::new().sort_asc("_id"))), (20, 20));
        // Sorted in memory by `_id`, which every key holds: 20 read.
        let newest = tenant().sort_desc("_id").limit(20);
        assert_eq!(found(newest), (20, 20));
        // Sorted by a key the index lacks: of the 400 in the range, the
        // entries reject t4's 200 unread, and the matches are read for
        // the key.
        let t3_of_two = Filter::new()
            .gte("tenant", "t3")
            .lte("tenant", "t4")
            .ne("tenant", "t4");
        assert_eq!(found(t3_of_two.sort_asc("tries").limit(5)), (5, 200));
        // A condition on a field the index lacks: every candidate read.
        let busy = tenant().gt("tries", 4);
        let many = (3..2000).step_by(10).filter(|i| i % 7 > 4).count();
        assert_eq!(found(busy.clone()), (many, 200));
        assert_eq!(reads(&|| docs.count(busy.clone()).unwrap()), (many, 200));
        // An unsorted cursor passes over the skipped unread.
        let streamed = || docs.cursor(tenant().skip(150)).unwrap().count();
        assert_eq!(reads(&streamed), (50, 50));

        // Values an entry can't tell exactly: their documents alone read.
        for (i, value) in [
            Document::from("t3".to_string() + &"x".repeat(2000)),
            Document::Int(1 << 60),
        ]
        .into_iter()
        .enumerate()
        {
            docs.insert(object(vec![
                ("tenant", value.clone()),
                ("tries", Document::Int(i as i64)),
            ]))
            .unwrap();
            let at_least = Filter::new().gte("tenant", value.clone());
            let expected = docs.find(at_least.clone()).unwrap().len();
            // The walk on `tenant` reads the inexact group whole, the
            // first here, though skipped (§34.2).
            let inexact = Filter::new()
                .gte("tenant", value)
                .sort_asc("tenant")
                .limit(5)
                .skip(1);
            assert_eq!(
                reads(&|| docs.count(at_least.clone()).unwrap()),
                (expected, 1)
            );
            assert_eq!(found(inexact).1, (expected - 1).min(5) + 1, "{i}");
        }
        assert_consistent(&db);
    }

    /// SPEC §101: runs of one value longer than a leaf, ties across leaf
    /// boundaries, a cut string and a big number inside a run, emptied
    /// leaves: `find`, `count` and a cursor give what checking every
    /// document gives, at every skip, in both directions.
    #[test]
    fn whole_leaves_decide_as_the_documents_would() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("g").unwrap();
        docs.ensure_index("n").unwrap();
        docs.ensure_index(["g", "n"]).unwrap();
        let mut rng = XorShift(0x0101_1EAF_0101_1EAF);
        let ops = (0..6000i64)
            .map(|i| {
                let g = match rng.below(400) {
                    0 => Document::from("b".to_string() + &"x".repeat(3000)),
                    1 => Document::Int((1 << 60) + i),
                    k => ["a", "b", "c"][k % 3].into(),
                };
                // Ties of three, which a leaf may split.
                let doc = object(vec![("g", g), ("n", Document::Int(i / 3))]);
                WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();
        // A stretch emptied, leaving empty leaves behind (§10.5).
        let stretch = Filter::new().gte("n", 700).lt("n", 1100);
        docs.delete_many(stretch).unwrap();
        let mut all = docs.find_with_ids(Filter::default()).unwrap();
        all.sort_by_key(|(id, _doc)| *id);
        let conditions = [
            Filter::new(),
            Filter::new().eq("g", "b"),
            Filter::new().ne("g", "b"),
            Filter::new().gte("g", "b"),
            Filter::new().lt("g", "c"),
            Filter::new().eq("g", "a").gte("n", 300).lt("n", 1500),
            Filter::new().gte("n", 500).lt("n", 1800),
            Filter::new().eq("g", "c").ne("n", 100),
        ];
        let sorts: [&[(&str, SortOrder)]; 6] = [
            &[],
            &[("n", SortOrder::Asc)],
            &[("n", SortOrder::Desc)],
            &[("_id", SortOrder::Asc)],
            &[("_id", SortOrder::Desc)],
            &[("g", SortOrder::Asc)],
        ];
        for _ in 0..300 {
            let mut f = conditions[rng.below(conditions.len())].clone();
            for (field, order) in sorts[rng.below(sorts.len())] {
                f = f.then_by(*field, *order);
            }
            let skip = match rng.below(3) {
                0 => rng.below(20),
                _ => rng.below(5000),
            };
            let f = f.skip(skip).limit([None, Some(1), Some(7)][rng.below(3)]);
            let expected: Vec<DocId> = f
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let plan = explained(&docs, &f);
            assert_eq!(docs.count(f.clone()).unwrap(), expected.len(), "{f:?}");
            let found: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            if f.sort.is_empty() {
                assert_eq!(found.len(), expected.len(), "{f:?} via {plan:?}");
            } else {
                assert_eq!(found, expected, "{f:?} via {plan:?}");
                assert_eq!(typed_ids(&docs, &f), expected, "typed: {f:?}");
            }
        }
        assert_consistent(&db);
    }

    /// SPEC §101.5: a skip passes over a leaf whole only where the group
    /// its last entry is in is exact. Numbers past 2^60 share an encoding
    /// 256 apart, so each such group spans several leaves, and is sorted
    /// in memory by the true values — here against id order.
    #[test]
    fn a_skip_keeps_an_inexact_group_across_leaves_in_its_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index(["g", "n"]).unwrap();
        let big = |k: i64| Document::Int((1 << 60) + k);
        let ns = (0..50).map(Document::Int).chain((0..900).rev().map(big));
        let ops = ns
            .map(|n| {
                let doc = object(vec![("g", "b".into()), ("n", n)]);
                WriteOp::Insert("docs".into(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();
        let all = docs.find_with_ids(Filter::default()).unwrap();
        for skip in (0..950).step_by(7) {
            let f = Filter::new().eq("g", "b").sort_asc("n").skip(skip).limit(3);
            let expected: Vec<DocId> = f
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let found: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(found, expected, "skip {skip}");
        }
    }

    /// SPEC §101: a count and a deep page copy no key, and a run of one
    /// value is decided once a leaf at most.
    #[test]
    fn a_walk_copies_and_decides_only_what_it_must() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("tasks");
        let ops = (0..5000)
            .map(|i| {
                let doc = object(vec![
                    ("tenant", format!("t{}", i % 5).into()),
                    ("status", ["Queued", "Done"][i as usize % 2].into()),
                    ("created", Document::Int(i)),
                ]);
                WriteOp::Insert("tasks".into(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();
        docs.ensure_index("tenant").unwrap();
        docs.ensure_index(["status", "created"]).unwrap();
        let counted = |run: &dyn Fn() -> usize| {
            let copied = crate::index::KEYS_COPIED.with(|n| n.replace(0));
            let decided = crate::query::DECISIONS.with(|n| n.replace(0));
            let found = run();
            let copied = crate::index::KEYS_COPIED.with(|n| n.replace(copied));
            let decided = crate::query::DECISIONS.with(|n| n.replace(decided));
            (found, copied, decided)
        };
        let entries = |f: Filter| {
            let run = docs.explain_run(f).unwrap();
            run.reads[0].entries as usize
        };
        let tenant = Filter::new().eq("tenant", "t3");
        let (n, copied, decided) = counted(&|| docs.count(tenant.clone()).unwrap());
        assert_eq!((n, copied), (1000, 0));
        // A leaf holds well over ten entries of this index.
        assert!(decided <= entries(tenant.clone()) / 10, "{decided}");
        assert_eq!(
            counted(&|| docs.count(Filter::new()).unwrap()),
            (5000, 0, 0)
        );
        let page = Filter::new()
            .eq("status", "Done")
            .sort_asc("created")
            .skip(1000)
            .limit(20);
        let (n, copied, _) = counted(&|| docs.find(page.clone()).unwrap().len());
        assert_eq!((n, copied), (20, 0));
        let by_id = Filter::new().sort_desc("_id").skip(3000).limit(20);
        let (n, copied, _) = counted(&|| docs.find(by_id.clone()).unwrap().len());
        assert_eq!((n, copied), (20, 0));
    }

    /// Conditions mostly on what the indexes of
    /// `entries_decide_as_the_documents_would` hold — `a`, `b`, `o.p`
    /// and `_id` — now and then on `c`, which none holds.
    fn covered_condition(rng: &mut XorShift, depth: usize, ids: &[DocId]) -> Condition {
        if depth == 0 || rng.below(3) == 0 {
            let ops = [
                Op::Eq,
                Op::Ne,
                Op::Lt,
                Op::Lte,
                Op::Gt,
                Op::Gte,
                Op::Contains,
            ];
            let op = ops[rng.below(ops.len())].clone();
            return match rng.below(9) {
                0..=2 => cond("b", op, random_value(rng)),
                3 => cond("a", op, Document::Int(rng.below(4) as i64 - 1)),
                4 | 5 => cond("o.p", op, random_value(rng)),
                6 | 7 => cond("_id", op, Document::Id(ids[rng.below(ids.len())])),
                _ => cond("c", op, Document::Int(rng.below(3) as i64)),
            };
        }
        let children = (0..rng.below(4))
            .map(|_| covered_condition(rng, depth - 1, ids))
            .collect::<Vec<_>>();
        match rng.below(5) {
            0 => Condition::All(children),
            1 => !covered_condition(rng, depth - 1, ids),
            _ => Condition::Any(children),
        }
    }

    /// SPEC §100: where index entries decide, `find`, `count` and a
    /// cursor, typed and untyped, give what checking every document gives
    /// — on one-field, nested, compound and sparse indexes, with `_id` in
    /// the conditions and the sort, with values that don't decide (cut
    /// strings, numbers past 2^53, and arrays, objects and NaN in a
    /// compound key), and skips of every size; and `delete_many` and
    /// `update_fields` change exactly what `find` returns.
    #[test]
    fn entries_decide_as_the_documents_would() {
        for sparse in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
            let docs = db.collection::<Document>("docs");
            let options = match sparse {
                true => IndexOptions::new().sparse(),
                false => IndexOptions::new(),
            };
            docs.ensure_index_with("b", options).unwrap();
            docs.ensure_index_with(["a", "b"], options).unwrap();
            docs.ensure_index_with("o.p", options).unwrap();
            let mut rng = XorShift(0x0100_C0DE_0100_C0DE ^ sparse as u64);
            let ops = (0..300)
                .map(|_| {
                    let mut fields = Vec::new();
                    match rng.below(5) {
                        0 => {}
                        1 => fields.push(("a", Document::Null)),
                        _ => fields.push(("a", Document::Int(rng.below(3) as i64))),
                    }
                    if rng.below(6) != 0 {
                        fields.push(("b", random_value(&mut rng)));
                    }
                    match rng.below(4) {
                        0 => {}
                        1 => fields.push(("o", Document::Int(1))),
                        _ => fields.push(("o", object(vec![("p", random_value(&mut rng))]))),
                    }
                    fields.push(("c", Document::Int(rng.below(3) as i64)));
                    WriteOp::Insert("docs".into(), db.id_gen().generate(), object(fields))
                })
                .collect();
            db.write_batch(ops).unwrap();
            let mut all = docs.find_with_ids(Filter::default()).unwrap();
            all.sort_by_key(|(id, _doc)| *id);
            let all = all;
            let ids: Vec<DocId> = all.iter().map(|(id, _)| *id).collect();
            let sorts: [&[(&str, SortOrder)]; 7] = [
                &[],
                &[("b", SortOrder::Asc)],
                &[("b", SortOrder::Desc), ("_id", SortOrder::Desc)],
                &[("a", SortOrder::Asc), ("b", SortOrder::Asc)],
                &[("_id", SortOrder::Desc)],
                &[("o.p", SortOrder::Asc)],
                &[("c", SortOrder::Asc), ("b", SortOrder::Desc)],
            ];
            let random_filter = |rng: &mut XorShift| {
                let mut f = Filter::new();
                for _ in 0..rng.below(3) {
                    f = f.and(covered_condition(rng, 2, &ids));
                }
                if rng.below(10) == 0 {
                    f = f.exists("b"); // never decided by an entry
                }
                for (field, order) in sorts[rng.below(sorts.len())] {
                    f = f.then_by(*field, *order);
                }
                f.skip([0, 0, 1, 5, 40, 1000][rng.below(6)])
                    .limit([None, Some(0), Some(1), Some(5), Some(30)][rng.below(5)])
            };
            let (mut covered, mut skipped_unread) = (0, 0);
            for _ in 0..600 {
                let f = random_filter(&mut rng);
                let unlimited = Filter {
                    skip: 0,
                    limit: None,
                    ..f.clone()
                };
                let matching: std::collections::HashSet<DocId> = unlimited
                    .apply_to(all.clone(), |(_, doc)| doc)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                let expected: Vec<DocId> = f
                    .apply_to(all.clone(), |(_, doc)| doc)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                let plan = explained(&docs, &f);
                let run = docs.explain_run(f.clone()).unwrap();
                if run.reads.iter().any(|read| read.covering) {
                    covered += 1;
                    if f.skip > 0 && run.stats.documents_examined <= run.stats.documents_returned {
                        skipped_unread += 1;
                    }
                }
                let found: Vec<DocId> = docs
                    .find_with_ids(f.clone())
                    .unwrap()
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                let typed = typed_ids(&docs, &f);
                let streamed: Vec<DocId> = docs
                    .cursor(f.clone())
                    .unwrap()
                    .map(|doc| match doc.unwrap() {
                        Document::Object(fields) => match fields["_id"] {
                            Document::Id(id) => id,
                            _ => panic!("no id"),
                        },
                        doc => panic!("not an object: {doc}"),
                    })
                    .collect();
                let count = docs.count(f.clone()).unwrap();
                assert_eq!(count, expected.len(), "{f:?} via {plan:?}");
                if f.sort.is_empty() {
                    // Which matches a skip passes over is unspecified
                    // without a sort (§72.1): as many, and all matches.
                    for got in [&found, &typed, &streamed] {
                        assert_eq!(got.len(), expected.len(), "{f:?} via {plan:?}");
                        assert!(got.iter().all(|id| matching.contains(id)), "{f:?}");
                        let unique: std::collections::HashSet<_> = got.iter().collect();
                        assert_eq!(unique.len(), got.len(), "{f:?}");
                    }
                } else {
                    assert_eq!(found, expected, "{f:?} via {plan:?}");
                    assert_eq!(typed, expected, "typed: {f:?} via {plan:?}");
                    assert_eq!(streamed, expected, "cursor: {f:?} via {plan:?}");
                }
            }
            assert!(covered > 150, "{covered} covered");
            assert!(skipped_unread > 20, "{skipped_unread} skipped unread");

            // Writes change what `find` returns, no more and no less.
            for _ in 0..40 {
                let f = random_filter(&mut rng);
                let before: std::collections::HashSet<DocId> = docs
                    .find_with_ids(Filter::new())
                    .unwrap()
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                let doomed: Vec<DocId> = docs
                    .find_with_ids(f.clone())
                    .unwrap()
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                if f.sort.is_empty() && (f.skip > 0 || f.limit.is_some()) {
                    continue; // which ones is unspecified
                }
                match rng.below(2) {
                    0 => {
                        assert_eq!(docs.delete_many(f.clone()).unwrap(), doomed.len(), "{f:?}");
                        let after: std::collections::HashSet<DocId> = docs
                            .find_with_ids(Filter::new())
                            .unwrap()
                            .into_iter()
                            .map(|(id, _)| id)
                            .collect();
                        let mut gone: Vec<DocId> = before.difference(&after).copied().collect();
                        gone.sort();
                        let mut doomed = doomed;
                        doomed.sort();
                        assert_eq!(gone, doomed, "{f:?}");
                    }
                    _ => {
                        let marked = Update::new().set("mark", Document::Int(1));
                        docs.update_fields(Filter::new(), &Update::new().unset("mark"))
                            .unwrap();
                        let changed = docs.update_fields(f.clone(), &marked).unwrap();
                        assert_eq!(changed, doomed.len(), "{f:?}");
                        let mut marked: Vec<DocId> = docs
                            .find_with_ids(Filter::new().eq("mark", 1))
                            .unwrap()
                            .into_iter()
                            .map(|(id, _)| id)
                            .collect();
                        marked.sort();
                        let mut doomed = doomed;
                        doomed.sort();
                        assert_eq!(marked, doomed, "{f:?}");
                    }
                }
            }
            assert_consistent(&db);
        }
    }

    /// A reference to another document, a `DocId` field, has an index
    /// key (SPEC §62): a filter on it reads its range of the index, a
    /// sort on it walks the index in the order the ids were made, a
    /// compound index serves both, a unique index refuses a second
    /// document with the same one, and an array of them is multikey.
    #[test]
    fn an_index_on_a_reference_reads_just_its_documents() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Car {
            owner: DocId,
            seats: i64,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let owners = (0..10).map(|_| db.id_gen().generate()).collect::<Vec<_>>();
        let cars = db.collection::<Car>("cars");
        let mut batch = db.batch();
        for seats in 0..300 {
            let owner = owners[seats as usize % 10];
            batch.insert(&cars, Car { owner, seats }).unwrap();
        }
        batch.commit().unwrap();
        cars.ensure_index("owner").unwrap();

        let owned = Filter::new().eq("owner", owners[3]);
        let field = "owner".to_string();
        assert_eq!(cars.explain(&owned).unwrap(), QueryPlan::Index { field });
        let mut found = Vec::new();
        let reads = records_read(|| found = cars.find(owned.clone()).unwrap());
        assert_eq!((found.len(), reads), (30, 30));
        assert!(found.iter().all(|car| car.owner == owners[3]));

        // Made before the third owner: the first two. The range includes
        // the bound itself, as every range does (`key::range_for`), and
        // the check leaves its 30 out — on the entries, which hold the
        // ids whole, so no car is read (SPEC §100).
        let earlier = Filter::new().lt("owner", owners[2]);
        let reads = records_read(|| assert_eq!(cars.count(earlier.clone()).unwrap(), 60));
        assert_eq!(reads, 0);

        let newest = Filter::new().sort_desc("owner").limit(5);
        let field = "owner".to_string();
        assert_eq!(
            cars.explain(&newest).unwrap(),
            QueryPlan::IndexOrder { field }
        );
        let reads = records_read(|| found = cars.find(newest.clone()).unwrap());
        assert_eq!(reads, 5);
        assert!(found.iter().all(|car| car.owner == owners[9]));

        cars.ensure_index(vec!["owner", "seats"]).unwrap();
        let most_seats = Filter::new()
            .eq("owner", owners[3])
            .sort_desc("seats")
            .limit(3);
        let field = "(owner, seats)".to_string();
        assert_eq!(
            cars.explain(&most_seats).unwrap(),
            QueryPlan::IndexOrder { field }
        );
        let reads = records_read(|| found = cars.find(most_seats.clone()).unwrap());
        let seats = found.iter().map(|car| car.seats).collect::<Vec<_>>();
        assert_eq!((seats, reads), (vec![293, 283, 273], 3));

        let plates = db.collection::<Document>("plates");
        plates
            .ensure_index_with("car", IndexOptions::new().unique())
            .unwrap();
        let plate = |car: DocId| object(vec![("car", Document::Id(car))]);
        plates.insert(plate(owners[0])).unwrap();
        plates.insert(plate(owners[1])).unwrap();
        let twice = plates.insert(plate(owners[0]));
        assert!(
            matches!(twice, Err(crate::Error::DuplicateValue { .. })),
            "{twice:?}"
        );

        let trips = db.collection::<Document>("trips");
        trips.ensure_index("drivers[*]").unwrap();
        for pair in owners.windows(2) {
            let drivers = pair.iter().map(|&id| Document::Id(id)).collect();
            trips
                .insert(object(vec![("drivers", Document::Array(drivers))]))
                .unwrap();
        }
        let drove = Filter::new().eq("drivers[*]", owners[4]);
        let field = "drivers[*]".to_string();
        assert_eq!(trips.explain(&drove).unwrap(), QueryPlan::Index { field });
        assert_eq!(trips.count(drove).unwrap(), 2);
        assert_consistent(&db);
    }

    /// Makes `collection`'s index on `fields` what a build before format
    /// 10 made of it (SPEC §62): an id is left out of a one-field index,
    /// and an "other" in a compound one — as if it were a value no index
    /// orders. Creates the index if it isn't there.
    fn key_ids_the_old_way(
        db: &Database,
        collection: &str,
        fields: &[&str],
        options: IndexOptions,
    ) {
        fn without_ids(doc: Document) -> Document {
            match doc {
                Document::Id(_) => Document::Binary(Vec::new()),
                Document::Object(map) => {
                    Document::Object(map.into_iter().map(|(k, v)| (k, without_ids(v))).collect())
                }
                Document::Array(items) => {
                    Document::Array(items.into_iter().map(without_ids).collect())
                }
                other => other,
            }
        }
        let fields = fields.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        db.transact(|catalog, store| {
            let existing = catalog
                .indexes(collection)
                .iter()
                .find(|i| i.fields == fields);
            let index = match existing.cloned() {
                Some(index) => {
                    BTreeIndex::new(index.root).clear(store)?;
                    index
                }
                None => catalog.create_index(store, collection, &fields, options)?,
            };
            let meta = *catalog.get(collection).unwrap();
            let mut tree = BTreeIndex::new(index.root);
            for (_key, loc) in BTreeIndex::new(meta.index_root).scan(store)? {
                let (id, doc) = data::get_record(store, loc)?;
                for key in index_keys(&without_ids(doc), &index, id) {
                    tree.insert(store, &key, loc)?;
                }
            }
            Ok(())
        })
        .unwrap();
    }

    /// A file from before format 10 (SPEC §62) has indexes without keys
    /// for ids. Opening it rebuilds those, and only those, in place, and
    /// stamps the file 10. A file without secondary indexes needs nothing
    /// and stays as it is, until the first write stamps it.
    /// A read-only open keys the ids an old file's indexes lack as an
    /// open that writes does, but in memory (SPEC §88): reads find them,
    /// the file stays as it was, and is keyed again at the next open.
    #[test]
    fn a_read_only_open_keys_an_older_files_ids_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let owners;
        {
            let db = Database::open(&path).unwrap();
            owners = (0..3).map(|_| db.id_gen().generate()).collect::<Vec<_>>();
            let cars = db.collection::<Document>("cars");
            let mut batch = db.batch();
            for seats in 0..300 {
                let owner = Document::Id(owners[seats as usize % 3]);
                let car = object(vec![("owner", owner), ("seats", Document::Int(seats))]);
                batch.insert(&cars, car).unwrap();
            }
            batch.commit().unwrap();
            key_ids_the_old_way(&db, "cars", &["owner"], IndexOptions::new());
        }
        crate::storage::rewrite_format_version(&path, 9);
        let before = std::fs::read(&path).unwrap();

        let read_only = crate::OpenOptions::default().read_only(true);
        for _ in 0..2 {
            let mut opened = None;
            let reads = records_read(|| {
                opened = Some(Database::open_with(&path, read_only).unwrap());
            });
            assert!(reads >= 300, "keyed at every read-only open");
            let db = opened.unwrap();
            assert!(db.check().unwrap().is_ok());
            let cars = db.collection::<Document>("cars");
            let owned = Filter::new().eq("owner", owners[1]);
            let field = "owner".to_string();
            assert_eq!(cars.explain(&owned).unwrap(), QueryPlan::Index { field });
            assert_eq!(cars.count(owned).unwrap(), 100);
            drop((cars, db));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn opening_an_older_file_keys_the_ids_its_indexes_lack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let owners;
        {
            let db = Database::open(&path).unwrap();
            owners = (0..3).map(|_| db.id_gen().generate()).collect::<Vec<_>>();
            let cars = db.collection::<Document>("cars");
            // Enough that each index takes several pages, all of which a
            // rebuild must free or reuse.
            let mut batch = db.batch();
            for seats in 0..900 {
                let owner = Document::Id(owners[seats as usize % 3]);
                let car = object(vec![("owner", owner), ("seats", Document::Int(seats))]);
                batch.insert(&cars, car).unwrap();
            }
            batch
                .insert(&cars, object(vec![("seats", Document::Int(1000))]))
                .unwrap();
            batch.commit().unwrap();
            cars.ensure_index("seats").unwrap();
            key_ids_the_old_way(&db, "cars", &["owner"], IndexOptions::new());
            key_ids_the_old_way(&db, "cars", &["owner", "seats"], IndexOptions::new());
            let owned = Filter::new().eq("owner", owners[1]);
            assert_eq!(
                cars.count(owned).unwrap(),
                0,
                "an old index, read by this build"
            );
            assert!(!db.check().unwrap().is_ok());

            let state = db.read().unwrap();
            let at = state.snapshot();
            let (documents, stale) = indexes_lacking_ids(at.catalog, &at.store, "cars").unwrap();
            let stale = stale.iter().map(|index| index.name()).collect::<Vec<_>>();
            assert_eq!(
                (documents.len(), stale),
                (901, vec!["owner".to_string(), "(owner, seats)".to_string()])
            );
        }
        crate::storage::rewrite_format_version(&path, 9);

        let mut opened = None;
        let reads = records_read(|| opened = Some(Database::open(&path).unwrap()));
        assert!(reads >= 901, "every document checked, and read to rebuild");
        let db = opened.unwrap();
        assert_eq!(db.file_info().unwrap().format_version, 12);
        assert!(db.check().unwrap().is_ok());
        let cars = db.collection::<Document>("cars");
        assert_eq!(
            cars.index_names().unwrap(),
            ["seats", "owner", "(owner, seats)"]
        );
        let owned = Filter::new().eq("owner", owners[1]);
        let field = "owner".to_string();
        assert_eq!(cars.explain(&owned).unwrap(), QueryPlan::Index { field });
        assert_eq!(cars.count(owned).unwrap(), 300);
        let first = Filter::new()
            .eq("owner", owners[2])
            .sort_asc("seats")
            .limit(2);
        let field = "(owner, seats)".to_string();
        assert_eq!(
            cars.explain(&first).unwrap(),
            QueryPlan::IndexOrder { field }
        );
        let seats = cars
            .find(first)
            .unwrap()
            .into_iter()
            .map(|car| crate::query::value_or_null(&car, "seats").clone());
        assert_eq!(
            seats.collect::<Vec<_>>(),
            [Document::Int(2), Document::Int(5)]
        );
        drop((cars, db));
        let reads = records_read(|| drop(Database::open(&path).unwrap()));
        assert_eq!(reads, 0, "checked once: the file says 12 now");

        // Indexes, none holding an id: nothing to rebuild, but checked,
        // so the file is stamped, and not checked again.
        let unkeyed = dir.path().join("unkeyed.trunkdb");
        let db = Database::open(&unkeyed).unwrap();
        let notes = db.collection::<Document>("notes");
        notes.ensure_index("n").unwrap();
        notes.insert(object(vec![("n", Document::Int(1))])).unwrap();
        drop((notes, db));
        crate::storage::rewrite_format_version(&unkeyed, 9);
        let db = Database::open(&unkeyed).unwrap();
        assert_eq!(db.file_info().unwrap().format_version, 12);
        drop(db);
        let reads = records_read(|| drop(Database::open(&unkeyed).unwrap()));
        assert_eq!(reads, 0);

        // No secondary index: nothing for §62 to check. §95 checks every
        // document, so the open stamps the file all the same.
        let plain = dir.path().join("plain.trunkdb");
        let db = Database::open(&plain).unwrap();
        db.collection::<Document>("notes")
            .insert(object(vec![("n", Document::Int(1))]))
            .unwrap();
        drop(db);
        crate::storage::rewrite_format_version(&plain, 9);
        let db = Database::open(&plain).unwrap();
        assert_eq!(db.file_info().unwrap().format_version, 12);
    }

    /// Stores `doc` in `collection` past the check that it is an object,
    /// as a build before SPEC §95 could.
    fn write_unchecked(db: &Database, collection: &str, doc: Document) -> DocId {
        let id = db.id_gen().generate();
        let op = WriteOp::Insert(collection.to_string(), id, doc);
        db.transact(|catalog, store| apply_unchecked(catalog, store, &op))
            .unwrap();
        id
    }

    fn read_only(path: &std::path::Path) -> crate::Result<Database> {
        Database::open_with(path, crate::OpenOptions::default().read_only(true))
    }

    /// The format version the file on disk says.
    fn format_on_disk(path: &std::path::Path) -> u32 {
        let bytes = std::fs::read(path).unwrap();
        u32::from_le_bytes(bytes[28..32].try_into().unwrap())
    }

    /// SPEC §95: a file of a format before 12 that holds only objects is
    /// one of 12. The open checks every document, and stamps it, so the
    /// check runs once; a read-only open checks too, and stamps nothing.
    #[test]
    fn an_older_file_of_objects_is_stamped_12_at_its_first_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("objects.trunkdb");
        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        for n in 0..300 {
            docs.insert(object(vec![("n", Document::Int(n))])).unwrap();
        }
        // A large one, in an overflow chain.
        docs.insert(object(vec![("big", Document::String("x".repeat(30_000)))]))
            .unwrap();
        db.collection::<Document>("empty")
            .ensure_index("a")
            .unwrap();
        drop((docs, db));
        crate::storage::rewrite_format_version(&path, 11);

        drop(read_only(&path).unwrap());
        assert_eq!(format_on_disk(&path), 11, "a read-only open writes nothing");
        let db = Database::open(&path).unwrap();
        assert_eq!(db.file_info().unwrap().format_version, 12);
        assert_eq!(
            db.collection::<Document>("docs")
                .count(Filter::new())
                .unwrap(),
            301
        );
        drop(db);
        assert_eq!(format_on_disk(&path), 12);

        // An empty file is stamped too.
        let empty = dir.path().join("empty.trunkdb");
        drop(Database::open(&empty).unwrap());
        crate::storage::rewrite_format_version(&empty, 11);
        drop(Database::open(&empty).unwrap());
        assert_eq!(format_on_disk(&empty), 12);
    }

    /// SPEC §95: a file of a format before 12 that holds a document that
    /// isn't an object can't be opened: both opens name the first one
    /// found, and the file stays as it was, byte for byte. `check`
    /// reports one in a file that says 12.
    #[test]
    fn an_older_file_with_a_document_that_is_not_an_object_is_refused() {
        // The collections are checked by name: the one holding it is
        // checked first, or after one holding only objects.
        for (bad, value) in [
            ("aaa", Document::Int(5)),
            ("docs", Document::String("y".repeat(30_000))), // an overflow chain
            ("docs", Document::Array(vec![Document::Int(1)])),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mixed.trunkdb");
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>(bad);
            docs.insert(object(vec![("n", Document::Int(1))])).unwrap();
            db.collection::<Document>("bbb")
                .insert(object(vec![("n", Document::Int(2))]))
                .unwrap();
            let id = write_unchecked(&db, bad, value);
            docs.insert(object(vec![("n", Document::Int(3))])).unwrap();
            let report = db.check().unwrap();
            assert!(
                report
                    .problems
                    .iter()
                    .any(|p| p.contains("isn't an object")),
                "{report:#?}"
            );
            drop((docs, db));
            crate::storage::rewrite_format_version(&path, 11);
            let before = std::fs::read(&path).unwrap();

            for opened in [Database::open(&path), read_only(&path)] {
                let Err(crate::Error::Format(crate::FormatError::NotAnObject {
                    collection,
                    id: found,
                })) = opened
                else {
                    panic!("expected NotAnObject, got {:?}", opened.err());
                };
                assert_eq!((collection.as_str(), found), (bad, id));
            }
            assert_eq!(std::fs::read(&path).unwrap(), before, "nothing changed");
        }
    }

    /// Ids never collided in a unique index before format 10 (SPEC §62):
    /// if two documents share one, rebuilding it fails the open, and the
    /// file is left as it was — an older build can still open it.
    #[test]
    fn an_older_unique_index_with_shared_ids_fails_the_open_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        {
            let db = Database::open(&path).unwrap();
            let car = Document::Id(db.id_gen().generate());
            let plates = db.collection::<Document>("plates");
            plates.insert(object(vec![("car", car.clone())])).unwrap();
            plates.insert(object(vec![("car", car)])).unwrap();
            key_ids_the_old_way(&db, "plates", &["car"], IndexOptions::new().unique());
        }
        crate::storage::rewrite_format_version(&path, 9);
        let before = std::fs::read(&path).unwrap();
        for _ in 0..2 {
            let failed = Database::open(&path).err();
            assert!(
                matches!(failed, Some(crate::Error::DuplicateValue { .. })),
                "{failed:?}"
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// `eq("_id", id)` looks the document up in the primary index, as
    /// `get` does, instead of reading every document (SPEC §61): alone,
    /// next to other conditions, as an OR of several ids, and ahead of an
    /// index that could serve the sort.
    #[test]
    fn a_filter_on_the_id_reads_just_those_documents() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Car {
            #[serde(rename = "_id")]
            id: Option<DocId>,
            seats: i64,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let cars = db.collection::<Car>("cars");
        let mut batch = db.batch();
        let ids = (0..300)
            .map(|seats| batch.insert(&cars, Car { id: None, seats }).unwrap())
            .collect::<Vec<_>>();
        batch.commit().unwrap();
        cars.ensure_index("seats").unwrap();
        let car = |i: usize| Car {
            id: Some(ids[i]),
            seats: i as i64,
        };

        let one = Filter::new().eq("_id", ids[100]);
        assert_eq!(cars.explain(&one).unwrap(), QueryPlan::ById);
        let mut found = Vec::new();
        let reads = records_read(|| found = cars.find(one.clone()).unwrap());
        assert_eq!((&found[..], reads), (&[car(100)][..], 1));
        assert_eq!(cars.find_one(one.clone()).unwrap(), Some(car(100)));
        assert_eq!(cars.count(one.clone()).unwrap(), 1);

        // The other conditions still count, and an unknown id is no match.
        let both = one.clone().eq("seats", 101);
        assert_eq!(cars.explain(&both).unwrap(), QueryPlan::ById);
        assert!(cars.find(both).unwrap().is_empty());
        let unknown = Filter::new().eq("_id", DocId([0xAB; 16]));
        let reads = records_read(|| assert!(cars.find(unknown).unwrap().is_empty()));
        assert_eq!(reads, 0);

        // Several ids, one of them twice: each document once, sorted
        // without walking the index on `seats`.
        let some = Filter::new()
            .any_of([
                Condition::eq("_id", ids[7]),
                Condition::eq("_id", ids[250]),
                Condition::eq("_id", ids[7]),
            ])
            .sort_desc("seats")
            .limit(10);
        assert_eq!(cars.explain(&some).unwrap(), QueryPlan::ById);
        let reads = records_read(|| found = cars.find(some.clone()).unwrap());
        assert_eq!((&found[..], reads), (&[car(250), car(7)][..], 2));
        let streamed = cars.cursor(some.clone()).unwrap();
        let streamed = streamed.map(|r| r.map(|car| car.seats));
        assert_eq!(
            streamed.collect::<crate::Result<Vec<_>>>().unwrap(),
            [250, 7]
        );

        // Writes find their documents the same way.
        let raised = cars.update_many(one.clone(), |car| car.seats += 1000);
        assert_eq!(raised.unwrap(), 1);
        assert_eq!(cars.get(&ids[100]).unwrap().unwrap().seats, 1100);
        assert_eq!(cars.delete_many(some).unwrap(), 2);
        assert_eq!(cars.count(Filter::new()).unwrap(), 298);

        // An id's string is not an id (SPEC §59.5): no lookup, no match.
        let text = Filter::new().eq("_id", ids[100].to_string());
        assert_eq!(cars.explain(&text).unwrap(), QueryPlan::Scan);
        assert!(cars.find(text).unwrap().is_empty());
    }

    /// `Filter::id` and `Condition::id` (SPEC §63) find a struct by the
    /// id it carries, through the primary index, where `eq("id", ...)`,
    /// the struct's own name for the field, finds nothing.
    #[test]
    fn a_filter_by_id_finds_the_struct_that_carries_it() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Task {
            #[serde(rename = "_id")]
            id: Option<DocId>,
            title: String,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let tasks = db.collection::<Task>("tasks");
        let ids = ["a", "b", "c"].map(|title| {
            tasks
                .insert(Task {
                    id: None,
                    title: title.into(),
                })
                .unwrap()
        });

        // As a command line would get it: the id as text.
        let wanted: DocId = ids[1].to_string().parse().unwrap();
        let one = Filter::new().id(wanted);
        assert_eq!(tasks.explain(&one).unwrap(), QueryPlan::ById);
        let task = tasks.find_one(one).unwrap().unwrap();
        assert_eq!((task.id, &task.title[..]), (Some(ids[1]), "b"));
        assert!(
            tasks
                .find(Filter::new().eq("id", wanted))
                .unwrap()
                .is_empty()
        );

        let two = Filter::new().any_of([ids[2], ids[0]].map(Condition::id));
        assert_eq!(tasks.explain(&two).unwrap(), QueryPlan::ById);
        let titles = tasks.find(two.sort_asc("title")).unwrap();
        let titles = titles.iter().map(|t| &t.title[..]).collect::<Vec<_>>();
        assert_eq!(titles, ["a", "c"]);
    }

    /// The typed path end to end: `status` is one of two values, read as
    /// a union of two ranges of its index.
    #[test]
    fn an_or_of_indexed_values_reads_just_those() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Run {
            status: String,
            seen: i64,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let runs = db.collection::<Run>("runs");
        let mut batch = db.batch();
        for seen in 0..300 {
            let status = ["Queued", "Running", "Complete"][(seen % 3) as usize].to_string();
            batch.insert(&runs, Run { status, seen }).unwrap();
        }
        batch.commit().unwrap();
        runs.ensure_index("status").unwrap();

        let active = Filter::new()
            .any_of([
                Condition::eq("status", "Queued"),
                Condition::eq("status", "Running"),
            ])
            .and(!Condition::lt("seen", 30));
        assert_eq!(
            runs.explain(&active).unwrap(),
            QueryPlan::IndexUnion {
                fields: vec!["status".to_string(), "status".to_string()]
            }
        );
        let mut found = Vec::new();
        let reads = records_read(|| found = runs.find(active.clone()).unwrap());
        assert_eq!(reads, 200, "the two ranges, not the Complete ones");
        assert_eq!(found.len(), 180);
        assert!(found.iter().all(|r| r.status != "Complete" && r.seen >= 30));
        assert_eq!(runs.count(active).unwrap(), 180);
    }

    // --- delete_many (SPEC §37) ---

    #[test]
    fn delete_many_deletes_what_find_would_return() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        let mut batch = db.batch();
        for age in 0..100 {
            batch
                .insert(&users, user(&format!("user {age}"), age))
                .unwrap();
        }
        batch.commit().unwrap();
        users.ensure_index("age").unwrap();
        users
            .ensure_index_with("name", crate::IndexOptions::new().unique())
            .unwrap();
        let ages = || -> Vec<i64> {
            let mut ages: Vec<i64> = users
                .find(Filter::new())
                .unwrap()
                .iter()
                .map(|u| u.age)
                .collect();
            ages.sort();
            ages
        };

        assert_eq!(users.delete_many(Filter::new().lt("age", 18)).unwrap(), 18);
        assert_eq!(ages().first(), Some(&18));
        // Sort and limit count: the five youngest left.
        assert_eq!(
            users
                .delete_many(Filter::new().sort_asc("age").limit(5))
                .unwrap(),
            5
        );
        assert_eq!(ages().first(), Some(&23));
        let either =
            Filter::new().any_of([Condition::eq("name", "user 50"), Condition::eq("age", 60)]);
        assert_eq!(users.delete_many(either).unwrap(), 2);
        assert_eq!(users.delete_many(Filter::new().eq("age", 500)).unwrap(), 0);
        assert_eq!(ages().len(), 75);
        // Indexes lost their entries: a deleted unique value is free again,
        // and a query through the age index finds nothing deleted.
        users.insert(user("user 50", 50)).unwrap();
        assert_eq!(users.count(Filter::new().lt("age", 23)).unwrap(), 0);

        // Nothing to delete in a collection that isn't there, and it
        // isn't created.
        let nobody = db.collection::<User>("nobody");
        assert_eq!(nobody.delete_many(Filter::new()).unwrap(), 0);
        assert_eq!(db.collections().unwrap(), ["users"]);
    }

    /// Random nested filters, sometimes sorted and limited: each
    /// `delete_many` removes exactly what `find` returned just before,
    /// and the indexes still agree with a scan afterwards.
    #[test]
    fn delete_many_matches_find_through_random_filters() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        docs.ensure_index("w").unwrap();
        let mut rng = XorShift(0x7777_1234_ABCD_0001);
        let mut deleted_any = 0;
        for _ in 0..40 {
            let ops = (0..20)
                .map(|_| {
                    WriteOp::Insert(
                        "docs".into(),
                        db.id_gen().generate(),
                        random_document(&mut rng),
                    )
                })
                .collect();
            db.write_batch(ops).unwrap();

            let mut f = Filter::new().and(random_condition(&mut rng, 2));
            if rng.below(2) == 0 {
                f = f
                    .sort_by("v", [SortOrder::Asc, SortOrder::Desc][rng.below(2)])
                    .limit(rng.below(8));
            }
            let before = sorted_ids(docs.find_with_ids(Filter::new()).unwrap());
            let doomed = sorted_ids(docs.find_with_ids(f.clone()).unwrap());
            assert_eq!(docs.delete_many(f.clone()).unwrap(), doomed.len(), "{f:?}");
            let after = sorted_ids(docs.find_with_ids(Filter::new()).unwrap());
            let expected: Vec<DocId> = before
                .into_iter()
                .filter(|id| !doomed.contains(id))
                .collect();
            assert_eq!(after, expected, "{f:?}");
            deleted_any += doomed.len();
        }
        assert!(deleted_any > 100, "only {deleted_any} deleted");
        assert_index_agrees_with_scan(&docs, &mut rng, "v");
        assert_nested_filters_agree_with_a_scan(&docs, &mut rng);
    }

    // --- update_many (SPEC §38) ---

    #[test]
    fn update_many_changes_what_find_would_return() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Task {
            name: String,
            status: String,
            rank: i64,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let tasks = db.collection::<Task>("tasks");
        let mut batch = db.batch();
        for rank in 0..60 {
            let status = if rank < 40 { "Queued" } else { "Done" }.to_string();
            let name = format!("task {rank}");
            batch.insert(&tasks, Task { name, status, rank }).unwrap();
        }
        batch.commit().unwrap();
        tasks.ensure_index("status").unwrap();
        tasks
            .ensure_index_with("name", crate::IndexOptions::new().unique())
            .unwrap();
        let count = |status: &str| tasks.count(Filter::new().eq("status", status)).unwrap();

        // Sort and limit count, and `change` sees the matches in order.
        let mut seen = Vec::new();
        let first_five = Filter::new()
            .eq("status", "Queued")
            .sort_asc("rank")
            .limit(5);
        let n = tasks
            .update_many(first_five, |task| {
                seen.push(task.rank);
                task.status = "Running".to_string();
            })
            .unwrap();
        assert_eq!((n, seen), (5, vec![0, 1, 2, 3, 4]));
        // The index moved with them.
        let running = Filter::new().eq("status", "Running");
        assert_eq!(
            tasks.explain(&running).unwrap(),
            QueryPlan::Index {
                field: "status".to_string()
            }
        );
        assert_eq!((count("Running"), count("Queued")), (5, 35));

        // Matches left as they were aren't written, nor counted.
        let done = Filter::new().eq("status", "Done");
        assert_eq!(
            tasks
                .update_many(done, |task| task.status = "Done".to_string())
                .unwrap(),
            0
        );

        // One change a unique index refuses rolls back all of them.
        let queued = || Filter::new().eq("status", "Queued");
        let all_same = tasks.update_many(queued(), |task| {
            task.name = "same".to_string();
            task.status = "Renamed".to_string();
        });
        assert!(
            matches!(all_same, Err(crate::Error::DuplicateValue { .. })),
            "{all_same:?}"
        );
        assert_eq!((count("Queued"), count("Renamed")), (35, 0));

        // An `_id` stays what it was, whatever `change` does to it.
        let docs = db.collection::<Document>("tasks");
        let (id, _) = tasks
            .find_one_with_id(Filter::new().eq("rank", 7))
            .unwrap()
            .unwrap();
        let n = docs
            .update_many(Filter::new().eq("rank", 7), |doc| {
                if let Document::Object(fields) = doc {
                    fields.insert("_id".to_string(), Document::Id(DocId([0; 16])));
                    fields.insert("rank".to_string(), Document::Int(700));
                }
            })
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(tasks.get(&id).unwrap().map(|task| task.rank), Some(700));

        // A match that doesn't convert to `T` fails the whole batch.
        let odd = [
            ("name", Document::String("odd".into())),
            ("status", Document::String("Queued".into())),
        ];
        docs.insert(object(odd.to_vec())).unwrap(); // no `rank`
        let bumped = tasks.update_many(queued(), |task| task.rank += 1000);
        assert!(
            matches!(bumped, Err(crate::Error::Document(_))),
            "{bumped:?}"
        );
        assert_eq!(tasks.count(Filter::new().gte("rank", 1000)).unwrap(), 0);
    }

    /// Random nested filters, sometimes sorted and limited, and a change
    /// that gives `v` a random new value, or leaves the document alone:
    /// the count and every document afterwards must match a model, and
    /// the indexes must still agree with a scan.
    #[test]
    fn update_many_matches_a_model_through_random_filters() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        docs.ensure_index("w").unwrap();
        let mut rng = XorShift(0x0DDB_A11C_0FFE_E123);
        // Compared as text: NaN isn't equal to itself.
        let text = |all: Vec<(DocId, Document)>| {
            let mut all: Vec<_> = all
                .into_iter()
                .map(|(id, doc)| (id, format!("{doc:?}")))
                .collect();
            all.sort();
            all
        };
        let mut changed_any = 0;
        for _ in 0..30 {
            let ops = (0..15)
                .map(|_| {
                    WriteOp::Insert(
                        "docs".into(),
                        db.id_gen().generate(),
                        random_document(&mut rng),
                    )
                })
                .collect();
            db.write_batch(ops).unwrap();

            let mut f = Filter::new().and(random_condition(&mut rng, 2));
            if rng.below(2) == 0 {
                f = f
                    .sort_by("v", [SortOrder::Asc, SortOrder::Desc][rng.below(2)])
                    .limit(rng.below(8));
            }
            let mut model = docs.find_with_ids(Filter::new()).unwrap();
            let matched = docs.find_with_ids(f.clone()).unwrap();
            // What the change will do to each match, decided up front.
            let new_values: Vec<Option<Document>> = matched
                .iter()
                .map(|_| (rng.below(4) != 0).then(|| random_value(&mut rng)))
                .collect();
            let mut expected_changes = 0;
            for ((id, old), new_v) in matched.iter().zip(&new_values) {
                let Some(new_v) = new_v else { continue };
                let mut new = old.clone();
                if let Document::Object(fields) = &mut new {
                    fields.insert("v".to_string(), new_v.clone());
                }
                if encode_document(&new) != encode_document(old) {
                    expected_changes += 1;
                    let slot = model.iter_mut().find(|(other, _)| other == id).unwrap();
                    slot.1 = new;
                }
            }

            let mut next = new_values.into_iter();
            let n = docs
                .update_many(f.clone(), |doc| {
                    if let (Some(new_v), Document::Object(fields)) = (next.next().unwrap(), doc) {
                        fields.insert("v".to_string(), new_v);
                    }
                })
                .unwrap();
            assert_eq!(n, expected_changes, "{f:?}");
            assert_eq!(
                text(docs.find_with_ids(Filter::new()).unwrap()),
                text(model),
                "{f:?}"
            );
            changed_any += n;
        }
        assert!(changed_any > 50, "only {changed_any} changed");
        assert_index_agrees_with_scan(&docs, &mut rng, "v");
        assert_nested_filters_agree_with_a_scan(&docs, &mut rng);
    }

    // --- Update operators (SPEC §68) ---

    /// `update_fields` on typed tasks: what `find` would return changes,
    /// sort and limit included; unchanged matches aren't counted; the
    /// indexes follow; a change `T` can't hold, an operator that can't be
    /// made, and an update refused as written each change nothing.
    #[test]
    fn update_fields_changes_what_find_would_return() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Task {
            #[serde(rename = "_id")]
            id: Option<DocId>,
            status: String,
            rank: i64,
            tries: i64,
            error: Option<String>,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let tasks = db.collection::<Task>("tasks");
        let mut batch = db.batch();
        for rank in 0..30 {
            let status = if rank < 20 { "Queued" } else { "Done" }.to_string();
            let error = (rank % 2 == 0).then(|| "timeout".to_string());
            let task = Task {
                id: None,
                status,
                rank,
                tries: 0,
                error,
            };
            batch.insert(&tasks, task).unwrap();
        }
        batch.commit().unwrap();
        tasks.ensure_index("status").unwrap();
        tasks.ensure_index("tries").unwrap();
        let count = |f: Filter| tasks.count(f).unwrap();

        let start = Update::new()
            .set("status", "Running")
            .inc("tries", 1)
            .unset("error");
        let first_five = Filter::new()
            .eq("status", "Queued")
            .sort_asc("rank")
            .limit(5);
        assert_eq!(tasks.update_fields(first_five, &start).unwrap(), 5);
        let running = tasks
            .find(Filter::new().eq("status", "Running").sort_asc("rank"))
            .unwrap();
        let ranks: Vec<i64> = running.iter().map(|t| t.rank).collect();
        assert_eq!(ranks, [0, 1, 2, 3, 4]);
        assert!(running.iter().all(|t| t.tries == 1 && t.error.is_none()));
        assert_eq!(
            count(Filter::new().eq("tries", 1)),
            5,
            "the index on tries follows"
        );
        assert_eq!(count(Filter::new().eq("status", "Queued")), 15);

        // Matches left as they were aren't written or counted.
        let same = Update::new().set("status", "Done");
        assert_eq!(
            tasks
                .update_fields(Filter::new().gte("rank", 20), &same)
                .unwrap(),
            0
        );

        // A change `T` can't hold fails the batch: none of it lands.
        let wrong_type = Update::new().set("tries", "many");
        let Err(crate::Error::Document(_)) = tasks.update_fields(Filter::new(), &wrong_type) else {
            panic!("a string in an i64 field must fail");
        };
        // So does an operator that can't be made. Untyped, so nothing but
        // the operator can fail: ranks 0 to 4 have no `error` since the
        // `unset`, and `inc` starts it; rank 5's is null, and fails.
        let untyped = db.collection::<Document>("tasks");
        let bad = Update::new().inc("tries", 1).inc("error", 1);
        let by_rank = Filter::new().sort_asc("rank");
        let Err(crate::Error::Update { id, message, .. }) = untyped.update_fields(by_rank, &bad)
        else {
            panic!("an inc of null must fail");
        };
        let rank_5 = tasks
            .find_one(Filter::new().eq("rank", 5))
            .unwrap()
            .unwrap();
        assert_eq!(Some(id), rank_5.id);
        assert!(
            message.contains("\"error\"") && message.contains("null"),
            "{message}"
        );
        assert_eq!(count(Filter::new().eq("tries", 1)), 5, "rolled back");
        assert_eq!(count(Filter::new().eq("tries", 0)), 25, "rolled back");
        assert_eq!(count(Filter::new().exists("error")), 25, "rolled back");

        // Refused as written, before anything is read.
        for refused in [Update::new().set("_id", 1), Update::new().unset("tags[*]")] {
            let Err(crate::Error::Io(e)) = tasks.update_fields(Filter::new(), &refused) else {
                panic!("{refused:?} must be refused");
            };
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        }
        let nothing = db.collection::<Document>("nothing");
        assert!(
            nothing
                .update_fields(Filter::new(), &Update::new().inc("n", "x"))
                .is_err()
        );
    }

    /// Random filters and random updates on the indexed `v` and `w`, some
    /// of which can't be made to every match: the count, every document
    /// and the indexes afterwards must match a model that applies the
    /// update to each match on its own, and a failed batch changes
    /// nothing.
    #[test]
    fn update_fields_matches_a_model_through_random_updates() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        docs.ensure_index("w").unwrap();
        docs.ensure_index("o.x").unwrap();
        docs.ensure_index("t[*]").unwrap();
        let mut rng = XorShift(0x005E_71BC_0FF5_E7A5);
        let text = |all: Vec<(DocId, Document)>| {
            let mut all: Vec<_> = all
                .into_iter()
                .map(|(id, doc)| (id, format!("{doc:?}")))
                .collect();
            all.sort();
            all
        };
        let (mut changed_any, mut failed_any) = (0, 0);
        for _ in 0..40 {
            let ops = (0..10)
                .map(|_| {
                    WriteOp::Insert(
                        "docs".into(),
                        db.id_gen().generate(),
                        random_document(&mut rng),
                    )
                })
                .collect();
            db.write_batch(ops).unwrap();

            let mut update = Update::new();
            for _ in 0..1 + rng.below(3) {
                let path = ["v", "w", "o.x"][rng.below(3)];
                let other = ["v", "t", "o.y"][rng.below(3)];
                let number = [Document::Int(1), Document::Float(0.5)][rng.below(2)].clone();
                let element = Document::Int(rng.below(4) as i64);
                update = match rng.below(9) {
                    0 => update.set(path, random_value(&mut rng)),
                    1 => update.unset(path),
                    2 => update.inc(path, number),
                    3 => update.min(path, number),
                    4 => update.max(path, number),
                    5 if path != other => update.rename(path, other),
                    6 => update.push("t", element),
                    7 => update.add_to_set("t", element),
                    _ => update.pull("t", element),
                };
            }
            let mut f = Filter::new().and(random_condition(&mut rng, 2));
            if rng.below(2) == 0 {
                f = f.sort_by("w", SortOrder::Asc).limit(rng.below(8));
            }

            let before = docs.find_with_ids(Filter::new()).unwrap();
            let mut model = before.clone();
            let mut expected = Ok(0);
            for (id, old) in docs.find_with_ids(f.clone()).unwrap() {
                let mut new = old.clone();
                if update.apply(&mut new).is_err() {
                    expected = Err(());
                    break;
                }
                if encode_document(&new) != encode_document(&old) {
                    expected = expected.map(|n| n + 1);
                    model.iter_mut().find(|(other, _)| *other == id).unwrap().1 = new;
                }
            }

            let result = docs.update_fields(f.clone(), &update);
            match expected {
                Ok(n) => {
                    assert_eq!(result.unwrap(), n, "{f:?} {update:?}");
                    assert_eq!(
                        text(docs.find_with_ids(Filter::new()).unwrap()),
                        text(model)
                    );
                    changed_any += n;
                }
                Err(()) => {
                    assert!(
                        matches!(result, Err(crate::Error::Update { .. })),
                        "{result:?}"
                    );
                    assert_eq!(
                        text(docs.find_with_ids(Filter::new()).unwrap()),
                        text(before)
                    );
                    failed_any += 1;
                }
            }
        }
        assert!(
            changed_any > 30 && failed_any > 3,
            "{changed_any} changed, {failed_any} failed"
        );
        for path in ["v", "w", "o.x", "t[*]"] {
            assert_index_agrees_with_scan(&docs, &mut rng, path);
        }
    }

    /// SPEC §73 on typed devices: tags as a set, the latest time seen, a
    /// best score, and a renamed field, found through their indexes.
    #[test]
    fn update_fields_changes_arrays_and_keeps_bounds() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Device {
            #[serde(rename = "_id")]
            id: Option<DocId>,
            tags: Vec<String>,
            last_seen: Option<crate::DateTime>,
            best: i64,
            label: Option<String>,
        }
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let devices = db.collection::<Device>("devices");
        devices.ensure_index("tags[*]").unwrap();
        devices.ensure_index("last_seen").unwrap();
        let at = |secs| crate::DateTime::from_unix(secs, 0).unwrap();
        for n in 0..10 {
            let device = Device {
                id: None,
                tags: vec!["new".to_string()],
                last_seen: (n % 2 == 0).then(|| at(1_000 + n)),
                best: n,
                label: None,
            };
            devices.insert(device).unwrap();
        }

        let seen = Update::new()
            .add_to_set("tags", "online")
            .pull("tags", "new")
            .max("last_seen", at(1_005))
            .max("best", 5);
        assert_eq!(devices.update_fields(Filter::new(), &seen).unwrap(), 10);
        assert_eq!(
            devices.update_fields(Filter::new(), &seen).unwrap(),
            0,
            "a set, a bound"
        );
        let all = devices.find(Filter::new().sort_asc("best")).unwrap();
        assert!(all.iter().all(|d| d.tags == ["online"]));
        let seen_at: Vec<i64> = all
            .iter()
            .map(|d| d.last_seen.unwrap().unix_seconds())
            .collect();
        assert_eq!(
            seen_at,
            [1005, 1005, 1005, 1005, 1005, 1005, 1006, 1005, 1008, 1005]
        );
        let best: Vec<i64> = all.iter().map(|d| d.best).collect();
        assert_eq!(best, [5, 5, 5, 5, 5, 5, 6, 7, 8, 9]);
        assert_eq!(
            devices.count(Filter::new().eq("tags[*]", "new")).unwrap(),
            0
        );
        assert_eq!(
            devices
                .count(Filter::new().eq("tags[*]", "online"))
                .unwrap(),
            10
        );
        let later = Filter::new().gt("last_seen", at(1_005));
        assert_eq!(
            devices.count(later).unwrap(),
            2,
            "the index on last_seen follows"
        );

        // A rename, untyped, into a field the struct has.
        let untyped = db.collection::<Document>("devices");
        let named = Update::new().set("name", "sensor").rename("name", "label");
        assert_eq!(
            untyped
                .update_fields(Filter::new().gte("best", 8), &named)
                .unwrap(),
            2
        );
        let labeled = devices.find(Filter::new().eq("label", "sensor")).unwrap();
        assert_eq!(labeled.iter().map(|d| d.best).collect::<Vec<_>>(), [8, 9]);

        // A push into a field `T` holds as a string fails the batch.
        let wrong = Update::new().push("label", "x");
        let Err(crate::Error::Update { message, .. }) =
            devices.update_fields(Filter::new().gte("best", 8), &wrong)
        else {
            panic!("a push onto a string must fail");
        };
        assert!(
            message.contains("`push` needs an array there, not a string"),
            "{message}"
        );
        assert_consistent(&db);
    }

    // --- Date-times (SPEC §69) ---

    /// A struct with `SystemTime` fields end to end: stored as
    /// date-times, found by a range through their index, sorted, changed
    /// by `update_fields`, and the same to the nanosecond after a reopen
    /// and after an export and import.
    #[test]
    fn system_time_fields_are_indexed_ranged_sorted_and_exported() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct Scan {
            #[serde(rename = "_id")]
            id: Option<DocId>,
            device: String,
            at: SystemTime,
            done: Option<SystemTime>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = Database::open(&path).unwrap();
        let scans = db.collection::<Scan>("scans");
        scans.ensure_index("at").unwrap();
        // Whole 100 ns, as every platform's `SystemTime` holds them.
        let start = UNIX_EPOCH + Duration::new(1_790_000_000, 999_999_900);
        let at = |i: u64| start + Duration::from_nanos(i * 1_000_000_100);
        let mut batch = db.batch();
        for i in 0..200 {
            let device = format!("d{}", i % 3);
            let scan = Scan {
                id: None,
                device,
                at: at(i),
                done: None,
            };
            batch.insert(&scans, scan).unwrap();
        }
        batch.commit().unwrap();

        let window = Filter::new().gte("at", at(50)).lt("at", at(60));
        assert_eq!(
            scans.explain(&window).unwrap(),
            QueryPlan::Index { field: "at".into() }
        );
        let found = scans.find(window.sort_desc("at")).unwrap();
        let times: Vec<SystemTime> = found.iter().map(|s| s.at).collect();
        assert_eq!(times, (50..60).rev().map(at).collect::<Vec<_>>());
        let newest = scans
            .find_one(Filter::new().sort_desc("at"))
            .unwrap()
            .unwrap();
        assert_eq!(newest.at, at(199));
        assert_eq!(scans.count(Filter::new().lt("at", start)).unwrap(), 0);
        assert_eq!(
            scans
                .count(Filter::new().eq("at", "2026-09-21T14:13:20Z"))
                .unwrap(),
            0
        );

        let finished = SystemTime::now();
        let first_ten = Filter::new().lt("at", at(10));
        let changed = scans
            .update_fields(first_ten.clone(), &Update::new().set("done", finished))
            .unwrap();
        assert_eq!(changed, 10);
        assert_eq!(scans.count(Filter::new().eq("done", finished)).unwrap(), 10);

        let everything = |db: &Database| {
            let mut all = db.collection::<Scan>("scans").find(Filter::new()).unwrap();
            all.sort_by_key(|s| s.id);
            all
        };
        let before = everything(&db);
        drop((scans, db));
        let db = Database::open(&path).unwrap();
        assert_eq!(everything(&db), before, "after a reopen");
        assert!(db.check().unwrap().is_ok());

        let mut export = Vec::new();
        db.export(&mut export).unwrap();
        let text = String::from_utf8(export.clone()).unwrap();
        assert!(
            text.contains(r#""at":{"$date":"2026-09-21T14:13:20.9999999Z"}"#),
            "{text}"
        );
        let copy = Database::open(dir.path().join("copy.trunkdb")).unwrap();
        copy.import(&export[..]).unwrap();
        assert_eq!(everything(&copy), before, "after an export and import");
        let window = Filter::new().gte("at", at(50)).lt("at", at(60));
        assert_eq!(copy.collection::<Scan>("scans").count(window).unwrap(), 10);
    }

    // --- Unique indexes (SPEC §33) ---

    fn duplicate(result: crate::Result<impl std::fmt::Debug>) -> (DocId, DocId) {
        match result {
            Err(crate::Error::DuplicateValue { id, existing, .. }) => (id, existing),
            other => panic!("expected DuplicateValue, got {other:?}"),
        }
    }

    #[test]
    fn a_unique_index_refuses_equal_values_and_rolls_back_the_whole_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let email = |s: &str| object(vec![("email", Document::String(s.to_string()))]);
        let (ada, bob) = {
            let db = Database::open(&path).unwrap();
            let docs = db.collection::<Document>("docs");
            assert!(
                docs.ensure_index_with("email", crate::IndexOptions::new().unique())
                    .unwrap()
            );
            assert!(
                !docs
                    .ensure_index_with("email", crate::IndexOptions::new().unique())
                    .unwrap()
            );
            assert_eq!(docs.unique_indexes().unwrap(), ["email"]);

            let ada = docs.insert(email("ada@x")).unwrap();
            let (id, existing) = duplicate(docs.insert(email("ada@x")));
            assert_eq!(existing, ada);
            assert_ne!(id, ada);
            // Equal as `Eq` sees it: case matters.
            let bob = docs.insert(email("ADA@x")).unwrap();
            assert_eq!(docs.count(Filter::default()).unwrap(), 2);

            // The first insert of a failing batch is rolled back too.
            let insert = |doc| WriteOp::Insert("docs".into(), db.id_gen().generate(), doc);
            duplicate(db.write_batch(vec![insert(email("cy@x")), insert(email("cy@x"))]));
            assert_eq!(docs.count(Filter::default()).unwrap(), 2);

            // An update onto a taken value fails and changes nothing...
            duplicate(docs.update(&bob, email("ada@x")));
            assert_eq!(docs.get(&bob).unwrap(), Some(with_id(email("ADA@x"), bob)));
            // ...keeping one's own value is fine, even when the document
            // grows and moves (SPEC §20.3)...
            let mut grown = email("ada@x");
            if let Document::Object(fields) = &mut grown {
                fields.insert("pad".into(), Document::String("p".repeat(9000)));
            }
            docs.update(&ada, grown).unwrap();
            // ...and a value is free again once its document is gone.
            docs.delete(&bob).unwrap();
            let bob = docs.insert(email("ADA@x")).unwrap();

            // Any number of documents may lack the field or hold null.
            for doc in [object(vec![]), object(vec![]), email("x")] {
                docs.insert(doc).unwrap();
            }
            for _ in 0..2 {
                docs.insert(object(vec![("email", Document::Null)]))
                    .unwrap();
            }
            (ada, bob)
        };

        let db = Database::open(&path).unwrap();
        let docs = db.collection::<Document>("docs");
        assert_eq!(docs.unique_indexes().unwrap(), ["email"]);
        assert_eq!(duplicate(docs.insert(email("ada@x"))).1, ada);
        assert_eq!(duplicate(docs.insert(email("ADA@x"))).1, bob);
    }

    #[test]
    fn unique_means_equal_values_not_equal_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index_with("k", crate::IndexOptions::new().unique())
            .unwrap();
        let k = |value: Document| object(vec![("k", value)]);
        let long = |tail: &str| Document::String("x".repeat(1200) + tail);
        let big = 1i64 << 53;

        docs.insert(k(Document::Int(1))).unwrap();
        duplicate(docs.insert(k(Document::Float(1.0))));
        docs.insert(k(Document::Float(0.0))).unwrap();
        duplicate(docs.insert(k(Document::Float(-0.0))));
        // Same key (§28.1), different values: both allowed.
        docs.insert(k(Document::Int(big))).unwrap();
        docs.insert(k(Document::Int(big + 1))).unwrap();
        docs.insert(k(long("a"))).unwrap();
        docs.insert(k(long("b"))).unwrap();
        duplicate(docs.insert(k(long("a"))));
        // Values no condition can match equal aren't checked.
        for _ in 0..2 {
            docs.insert(k(Document::Float(f64::NAN))).unwrap();
            docs.insert(k(Document::Array(vec![Document::Int(1)])))
                .unwrap();
        }
    }

    #[test]
    fn ensure_unique_index_over_duplicates_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        let first = users.insert(user("Ada", 36)).unwrap();
        users.insert(user("Bob", 41)).unwrap();
        let second = users.insert(user("Ada", 20)).unwrap();

        assert_eq!(
            duplicate(users.ensure_index_with("name", crate::IndexOptions::new().unique())),
            (second, first)
        );
        assert!(users.index_names().unwrap().is_empty());

        users.delete(&second).unwrap();
        assert!(
            users
                .ensure_index_with("name", crate::IndexOptions::new().unique())
                .unwrap()
        );

        // The other kind of index on the same field is refused, not
        // swapped: dropping it first is the caller's call.
        users.ensure_index("age").unwrap();
        for result in [
            users.ensure_index_with("age", crate::IndexOptions::new().unique()),
            users.ensure_index("name"),
        ] {
            let Err(crate::Error::Io(err)) = result else {
                panic!("expected a refusal, got {result:?}");
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert_eq!(users.index_names().unwrap(), ["name", "age"]);
        assert_eq!(users.unique_indexes().unwrap(), ["name"]);
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Member {
        name: String,
        nick: Option<String>,
        team: Option<String>,
    }

    /// Entries in `collection`'s index named `name`.
    fn entries(db: &Database, collection: &str, name: &str) -> usize {
        let state = db.read().unwrap();
        let index = state
            .catalog()
            .indexes(collection)
            .iter()
            .find(|index| index.name() == name)
            .unwrap()
            .clone();
        BTreeIndex::new(index.root)
            .scan(&state.snapshot().store)
            .unwrap()
            .len()
    }

    /// What sparse indexes are for (SPEC §44): a field few documents
    /// have. One document in ten has a nick — the sparse index holds
    /// those, the plain one every document; both find the same.
    #[test]
    fn a_sparse_index_holds_only_the_documents_with_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let members = db.collection::<Member>("members");
        members.ensure_index_with("nick", SPARSE).unwrap();
        members.ensure_index_with(["nick", "team"], SPARSE).unwrap();
        members.ensure_index("team").unwrap();
        let ops = (0..1000)
            .map(|i| {
                let member = Member {
                    name: format!("m{i}"),
                    nick: (i % 10 == 0).then(|| format!("n{}", i % 30)),
                    team: (i % 25 == 0).then(|| "red".to_string()),
                };
                let doc = crate::serde_bridge::to_document(&member).unwrap();
                WriteOp::Insert("members".into(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();
        assert_eq!(entries(&db, "members", "nick"), 100);
        // Null in both only if neither is set: 100 + 40 - 20 with both.
        assert_eq!(entries(&db, "members", "(nick, team)"), 120);
        assert_eq!(entries(&db, "members", "team"), 1000);
        assert_eq!(members.sparse_indexes().unwrap(), ["nick", "(nick, team)"]);

        let by_nick = Filter::new().eq("nick", "n10");
        assert_eq!(
            members.explain(&by_nick).unwrap(),
            QueryPlan::Index {
                field: "nick".into()
            }
        );
        // The index's entries answer the count (SPEC §100).
        let reads = records_read(|| assert_eq!(members.count(by_nick).unwrap(), 33));
        assert_eq!(reads, 0);
        // Without a nick: not in the index, so a scan finds them.
        let without = Filter::new().is_null("nick");
        assert_eq!(members.explain(&without).unwrap(), QueryPlan::Scan);
        assert_eq!(members.count(without).unwrap(), 900);
        let first_nicks = Filter::new().is_not_null("nick").sort_asc("nick").limit(3);
        assert_eq!(
            members.explain(&first_nicks).unwrap(),
            QueryPlan::IndexOrder {
                field: "nick".into()
            }
        );
        let nicks: Vec<_> = members.find(first_nicks).unwrap();
        assert!(nicks.iter().all(|m| m.nick.as_deref() == Some("n0")));
        assert_consistent(&db);
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Profile {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        nick: Option<String>,
        team: Option<String>,
        tags: Vec<String>,
    }

    /// `exists` and `missing` on typed documents (SPEC §45.1): serde
    /// writes `None` as null, so the field is there — unless the struct
    /// skips it, or the document was written before the field existed.
    /// Next to an indexed comparison, the index finds and they check.
    #[test]
    fn exists_tells_skipped_and_older_fields_from_stored_nulls() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let profiles = db.collection::<Profile>("profiles");
        profiles.ensure_index("name").unwrap();
        let profile = |name: &str, nick: Option<&str>, team: Option<&str>, tags: &[&str]| Profile {
            name: name.into(),
            nick: nick.map(str::to_string),
            team: team.map(str::to_string),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        };
        profiles
            .insert(profile("ada", Some("countess"), Some("red"), &["math"]))
            .unwrap();
        profiles.insert(profile("bob", None, None, &[])).unwrap();
        // Written before `team` and `tags` were added to the struct.
        let untyped = db.collection::<Document>("profiles");
        untyped
            .insert(object(vec![
                ("name", "cy".into()),
                ("nick", Document::Null),
            ]))
            .unwrap();

        let names = |f: Filter| -> Vec<String> {
            let mut names: Vec<String> = untyped
                .find(f)
                .unwrap()
                .into_iter()
                .map(|doc| match crate::query::value_or_null(&doc, "name") {
                    Document::String(name) => name.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            names.sort();
            names
        };
        // `None` skipped: missing. A stored null: there.
        assert_eq!(names(Filter::new().missing("nick")), ["bob"]);
        assert_eq!(names(Filter::new().exists("nick")), ["ada", "cy"]);
        assert_eq!(names(Filter::new().is_null("nick")), ["bob", "cy"]);
        // `None` written as null: there. Older documents: missing.
        assert_eq!(names(Filter::new().exists("team")), ["ada", "bob"]);
        assert_eq!(names(Filter::new().missing("team")), ["cy"]);
        // Sizes: bob's empty list; cy has no list at all.
        assert_eq!(names(Filter::new().size("tags", Op::Eq, 0)), ["bob"]);
        assert_eq!(names(Filter::new().size("tags", Op::Ne, 0)), ["ada", "cy"]);
        assert_eq!(names(Filter::new().size("tags", Op::Gte, 1)), ["ada"]);

        let bob_without_nick = Filter::new().eq("name", "bob").missing("nick");
        assert_eq!(
            untyped.explain(&bob_without_nick).unwrap(),
            QueryPlan::Index {
                field: "name".into()
            }
        );
        assert_eq!(
            records_read(|| assert_eq!(names(bob_without_nick), ["bob"])),
            1
        );
        let found = profiles.find(Filter::new().missing("nick")).unwrap();
        assert_eq!(found, [profile("bob", None, None, &[])]);
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Line {
        sku: String,
        qty: i64,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Order {
        number: i64,
        lines: Vec<Line>,
    }

    /// What `elem_match` is for (SPEC §46): orders with a line of 9 or
    /// more of sku A1 — not an A1 line and some other line of 9. The
    /// index on `lines[*].sku` finds the orders with an A1 line; only
    /// those are read.
    /// SPEC §56: an insert or update nested deeper than the limit is
    /// refused like a name too long, and its batch rolls back whole.
    #[test]
    fn writes_nested_too_deep_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let nest = |levels: usize| {
            (0..levels).fold(Document::Int(1), |inner, _| Document::Array(vec![inner]))
        };
        let wrap =
            |inner: Document| Document::Object([("a".to_string(), inner)].into_iter().collect());
        // The document is the first level, its field's arrays the rest.
        let id = docs.insert(wrap(nest(Document::MAX_NESTING - 1))).unwrap();

        let refused = |e: crate::Error| match e {
            crate::Error::Io(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                assert!(e.to_string().contains("deeper than the limit of 64"), "{e}");
            }
            other => panic!("{other:?}"),
        };
        refused(docs.insert(wrap(nest(Document::MAX_NESTING))).unwrap_err());
        refused(
            docs.update(&id, wrap(nest(Document::MAX_NESTING)))
                .unwrap_err(),
        );
        let ops = [wrap(Document::Int(2)), wrap(nest(100))]
            .into_iter()
            .enumerate()
            .map(|(n, doc)| WriteOp::Insert("docs".into(), DocId([n as u8 + 1; 16]), doc))
            .collect();
        refused(db.write_batch(ops).unwrap_err());
        assert_eq!(
            docs.count(Filter::new()).unwrap(),
            1,
            "the batch rolled back whole"
        );
        let Some(Document::Object(stored)) = docs.get(&id).unwrap() else {
            panic!("the first insert is there");
        };
        assert_eq!(stored["a"], nest(Document::MAX_NESTING - 1));
    }

    /// Found by fuzzing (SPEC §55): a damaged compound key with fewer
    /// values than the index has fields groups by the ones it has, and
    /// a key too short for even its id by none.
    #[test]
    fn groups_of_keys_with_values_missing() {
        let spans = Spans {
            at: 1,
            end: 3,
            fields: 3,
            primary: false,
        };
        let queued = Document::String("Queued".into());
        let one = key::compound(&[&queued], DocId([1; 16]));
        let group = spans.group(&one);
        assert_eq!(group, key::value_part(&one));
        assert!(spans.values(&one).is_empty());
        for short in [&[][..], &[1, 2, 3][..]] {
            assert_eq!(spans.group(short), &[] as &[u8]);
            assert!(spans.values(short).is_empty());
        }
    }

    #[test]
    fn orders_with_a_large_line_of_one_sku() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let orders = db.collection::<Order>("orders");
        orders.ensure_index("lines[*].sku").unwrap();
        let mut rng = XorShift(0x5851_F42D_4C95_7F2D);
        let ops = (0..2000)
            .map(|number| {
                let lines = (0..1 + rng.below(3))
                    .map(|_| Line {
                        sku: format!("A{}", rng.below(20)),
                        qty: 1 + rng.below(10) as i64,
                    })
                    .collect();
                let order = Order { number, lines };
                let doc = crate::serde_bridge::to_document(&order).unwrap();
                WriteOp::Insert("orders".into(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();

        let large_a1 = Filter::new().elem_match(
            "lines",
            Condition::eq("sku", "A1") & Condition::gte("qty", 9),
        );
        let loose = Filter::new()
            .eq("lines[*].sku", "A1")
            .gte("lines[*].qty", 9);
        let all = orders.find(Filter::new()).unwrap();
        let has = |order: &Order, both: bool| {
            let a1 = |line: &Line| line.sku == "A1";
            let large = |line: &Line| line.qty >= 9;
            match both {
                true => order.lines.iter().any(|l| a1(l) && large(l)),
                false => order.lines.iter().any(a1) && order.lines.iter().any(large),
            }
        };
        let numbers = |orders: Vec<Order>| {
            let mut numbers: Vec<i64> = orders.iter().map(|o| o.number).collect();
            numbers.sort();
            numbers
        };
        let expected = numbers(all.iter().filter(|o| has(o, true)).cloned().collect());
        let expected_loose = numbers(all.iter().filter(|o| has(o, false)).cloned().collect());
        assert!(expected.len() < expected_loose.len(), "{expected:?}");
        assert!(!expected.is_empty());
        assert_eq!(
            orders.explain(&large_a1).unwrap(),
            QueryPlan::Index {
                field: "lines[*].sku".into()
            }
        );
        let with_a1 = orders
            .count(Filter::new().eq("lines[*].sku", "A1"))
            .unwrap();
        let reads = records_read(|| assert_eq!(numbers(orders.find(large_a1).unwrap()), expected));
        assert_eq!(reads, with_a1);
        assert!(reads < 2000 / 5, "{reads}");
        assert_eq!(numbers(orders.find(loose).unwrap()), expected_loose);
    }

    /// Asking for an index that exists with other options is refused,
    /// whichever option differs; the same options again are a no-op.
    #[test]
    fn an_index_with_other_options_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let members = db.collection::<Member>("members");
        let both = IndexOptions {
            unique: true,
            sparse: true,
        };
        assert!(members.ensure_index_with("nick", SPARSE).unwrap());
        assert!(!members.ensure_index_with("nick", SPARSE).unwrap());
        assert!(members.ensure_index_with("name", both).unwrap());
        for result in [
            members.ensure_index("nick"),
            members.ensure_index_with("nick", crate::IndexOptions::new().unique()),
            members.ensure_index_with("nick", both),
            members.ensure_index_with("name", crate::IndexOptions::new().unique()),
        ] {
            let Err(crate::Error::Io(err)) = result else {
                panic!("expected a refusal, got {result:?}");
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            assert!(err.to_string().contains("drop it first"), "{err}");
        }
        let Err(err) = members.ensure_index("nick") else {
            unreachable!()
        };
        assert!(
            err.to_string()
                .contains("already has a sparse index on \"nick\", not a plain"),
            "{err}"
        );
        assert_eq!(members.unique_indexes().unwrap(), ["name"]);
        assert_eq!(members.sparse_indexes().unwrap(), ["nick", "name"]);

        members
            .insert(Member {
                name: "Ada".into(),
                nick: None,
                team: None,
            })
            .unwrap();
        members
            .insert(Member {
                name: "Bob".into(),
                nick: None,
                team: None,
            })
            .unwrap();
        let taken = members.insert(Member {
            name: "Ada".into(),
            nick: None,
            team: None,
        });
        assert!(
            matches!(taken, Err(crate::Error::DuplicateValue { .. })),
            "{taken:?}"
        );
    }

    /// Random single-op batches against a unique index on `v`: each must
    /// fail exactly when a scan finds another document with an equal,
    /// non-null `v` — and a failed one must change nothing.
    #[test]
    fn a_unique_index_refuses_exactly_what_a_scan_finds_taken() {
        a_unique_index_refuses_what_a_scan_finds_taken(false);
    }

    /// Unique and sparse (SPEC §44): nulls were exempt anyway, now they
    /// have no entries either.
    #[test]
    fn a_unique_sparse_index_refuses_exactly_what_a_scan_finds_taken() {
        a_unique_index_refuses_what_a_scan_finds_taken(true);
    }

    fn a_unique_index_refuses_what_a_scan_finds_taken(sparse: bool) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let options = IndexOptions {
            unique: true,
            sparse,
        };
        docs.ensure_index_with("v", options).unwrap();
        let mut rng = XorShift(0xD1B5_4A32_D192_ED03);
        let mut ids: Vec<DocId> = Vec::new();
        let (mut accepted, mut refused) = (0, 0);

        // Single-op batches, so each outcome can be checked — each one
        // syncs to disk twice, which is what keeps the count low.
        for _ in 0..300 {
            let before = docs.find_with_ids(Filter::default()).unwrap();
            let doc = random_document(&mut rng);
            let (op, id) = match rng.below(4) {
                0 if !ids.is_empty() => {
                    let id = ids[rng.below(ids.len())];
                    (WriteOp::Delete("docs".into(), id), None)
                }
                1 | 2 if !ids.is_empty() => {
                    let id = ids[rng.below(ids.len())];
                    (WriteOp::Update("docs".into(), id, doc.clone()), Some(id))
                }
                _ => {
                    let id = db.id_gen().generate();
                    (WriteOp::Insert("docs".into(), id, doc.clone()), Some(id))
                }
            };
            let value = crate::query::value_or_null(&doc, "v");
            let taken = id.is_some()
                && !matches!(value, Document::Null)
                && before.iter().any(|(other, stored)| {
                    Some(*other) != id
                        && crate::query::equal(crate::query::value_or_null(stored, "v"), value)
                });

            match (db.write_batch(vec![op.clone()]), taken) {
                (Ok(()), false) => {
                    accepted += 1;
                    match op {
                        WriteOp::Insert(_, id, _) => ids.push(id),
                        WriteOp::Delete(_, id) => ids.retain(|other| *other != id),
                        WriteOp::Update(..) => {}
                    }
                }
                (Err(crate::Error::DuplicateValue { .. }), true) => {
                    refused += 1;
                    let after = docs.find_with_ids(Filter::default()).unwrap();
                    assert_eq!(sorted_ids(after), sorted_ids(before));
                }
                (result, taken) => panic!("{op:?}: {result:?}, but taken = {taken}"),
            }
        }
        assert!(accepted > 100 && refused > 20, "{accepted} / {refused}");
        assert_index_agrees_with_scan(&docs, &mut rng, "v");
    }

    /// A batch that fails after touching an index leaves no entry behind.
    #[test]
    fn a_failed_batch_rolls_back_its_index_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        users.ensure_index("age").unwrap();
        let ada = users
            .insert(User {
                name: "Ada".to_string(),
                age: 36,
            })
            .unwrap();

        let mut batch = db.batch();
        let grace = User {
            name: "Grace".to_string(),
            age: 45,
        };
        batch.insert(&users, grace).unwrap();
        batch.delete(&users, &DocId([0; 16])); // doesn't exist: fails the batch
        assert!(batch.commit().is_err());

        let found = users.find_with_ids(age_filter(Op::Gte, 0)).unwrap();
        assert_eq!(
            found.into_iter().map(|(id, _)| id).collect::<Vec<_>>(),
            [ada]
        );
    }

    // --- find_one, count, cursor, upsert (SPEC §29) ---

    use crate::Upserted;
    use crate::query::{Sort, SortOrder};

    fn user(name: &str, age: i64) -> User {
        User {
            name: name.to_string(),
            age,
        }
    }

    fn name_is(name: &str) -> Filter {
        filter(vec![cond(
            "name",
            Op::Eq,
            Document::String(name.to_string()),
        )])
    }

    fn by_age(order: SortOrder) -> Filter {
        Filter {
            sort: vec![Sort {
                field: "age".to_string(),
                order,
            }],
            ..Filter::default()
        }
    }

    fn users_db() -> (tempfile::TempDir, Database, Collection<User>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        let mut batch = db.batch();
        for (name, age) in [("Ada", 36), ("Grace", 45), ("Alan", 41), ("Edsger", 72)] {
            batch.insert(&users, user(name, age)).unwrap();
        }
        batch.commit().unwrap();
        (dir, db, users)
    }

    #[test]
    fn find_one_returns_a_match_or_none() {
        let (_dir, _db, users) = users_db();

        assert_eq!(
            users.find_one(name_is("Alan")).unwrap(),
            Some(user("Alan", 41))
        );
        assert_eq!(users.find_one(name_is("Barbara")).unwrap(), None);
        // With a sort: the first in that order.
        assert_eq!(
            users.find_one(by_age(SortOrder::Asc)).unwrap(),
            Some(user("Ada", 36))
        );
        assert_eq!(
            users.find_one(by_age(SortOrder::Desc)).unwrap(),
            Some(user("Edsger", 72))
        );

        let (id, found) = users.find_one_with_id(name_is("Grace")).unwrap().unwrap();
        assert_eq!(users.get(&id).unwrap(), Some(found));
    }

    #[test]
    fn count_counts_matches_up_to_the_limit() {
        let (_dir, db, users) = users_db();

        assert_eq!(users.count(Filter::default()).unwrap(), 4);
        assert_eq!(users.count(age_filter(Op::Gt, 40)).unwrap(), 3);
        let limited = Filter {
            limit: Some(2),
            ..age_filter(Op::Gt, 40)
        };
        assert_eq!(users.count(limited).unwrap(), 2);
        // Through an index, with a candidate the recheck must drop.
        users.ensure_index("age").unwrap();
        assert_eq!(users.count(age_filter(Op::Gt, 41)).unwrap(), 2);
        assert_eq!(
            db.collection::<User>("nobody")
                .count(Filter::default())
                .unwrap(),
            0
        );
    }

    /// A cursor shows the moment it was created (SPEC §82). It holds no
    /// lock — the writes below, on the same thread, would deadlock
    /// otherwise — and sees none of them: not a delete of a document it
    /// hasn't reached, not an update that makes one stop matching, not an
    /// insert. Until §82 it saw each document as it was when it got there.
    #[test]
    fn a_cursor_shows_the_moment_it_was_created() {
        let (_dir, db, users) = users_db();
        let over_40 = age_filter(Op::Gt, 40);
        let ids: Vec<DocId> = users
            .find_with_ids(Filter::default())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect(); // _id order: Ada, Grace, Alan, Edsger

        let mut cursor = users.cursor(over_40.clone()).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), user("Grace", 45));

        assert!(users.delete(&ids[2]).unwrap()); // Alan: gone before he's read
        assert!(users.update(&ids[3], user("Edsger", 39)).unwrap()); // no longer matches
        assert!(users.update(&ids[0], user("Ada", 50)).unwrap()); // matches now
        users.insert(user("Barbara", 60)).unwrap(); // after the cursor was created
        // And the slot Alan was in taken by someone else, in the file.
        users.insert(user("Linus", 99)).unwrap();
        db.checkpoint().unwrap();

        assert_eq!(cursor.next().unwrap().unwrap(), user("Alan", 41));
        assert_eq!(cursor.next().unwrap().unwrap(), user("Edsger", 72));
        assert!(cursor.next().is_none());

        // A cursor made now has all of it.
        let now: Vec<User> = users
            .cursor(over_40)
            .unwrap()
            .collect::<crate::Result<_>>()
            .unwrap();
        let expected = [
            user("Ada", 50),
            user("Grace", 45),
            user("Barbara", 60),
            user("Linus", 99),
        ];
        assert_eq!(now, expected);

        let limited = Filter {
            limit: Some(1),
            ..Filter::default()
        };
        assert_eq!(users.cursor(limited).unwrap().count(), 1);
    }

    /// An open cursor keeps its moment as a snapshot does (SPEC §82): the
    /// database can't be compacted until it's dropped. A sorted cursor has
    /// read everything when it's made, and keeps nothing.
    #[test]
    fn an_open_cursor_refuses_compaction() {
        let (_dir, db, users) = users_db();
        let ids: Vec<DocId> = (0..300)
            .map(|i| users.insert(user(&"x".repeat(i % 900), 1)).unwrap())
            .collect();
        for id in &ids {
            assert!(users.delete(id).unwrap());
        }

        let mut cursor = users.cursor(Filter::new()).unwrap();
        assert!(cursor.next().is_some());
        assert!(matches!(db.compact(), Err(crate::Error::SnapshotOpen)));
        assert_eq!(cursor.count(), 3, "and reads on");

        let sorted = users.cursor(by_age(SortOrder::Desc)).unwrap();
        let compacted = db.compact().unwrap();
        assert!(compacted.pages_after < compacted.pages_before);
        assert_eq!(sorted.count(), 4);
    }

    #[test]
    fn a_sorted_cursor_yields_in_order() {
        let (_dir, _db, users) = users_db();
        let ages: Vec<i64> = users
            .cursor(by_age(SortOrder::Desc))
            .unwrap()
            .map(|result| result.unwrap().age)
            .collect();
        assert_eq!(ages, [72, 45, 41, 36]);
    }

    #[test]
    fn upsert_inserts_updates_or_refuses() {
        let (_dir, _db, users) = users_db();

        let alan = users.find_one_with_id(name_is("Alan")).unwrap().unwrap().0;
        let updated = users.upsert(name_is("Alan"), user("Alan", 42)).unwrap();
        assert_eq!(updated, Upserted::Updated(alan));
        assert_eq!(users.get(&alan).unwrap(), Some(user("Alan", 42)));

        let inserted = users
            .upsert(name_is("Barbara"), user("Barbara", 80))
            .unwrap();
        let Upserted::Inserted(barbara) = inserted else {
            panic!("expected an insert, got {inserted:?}");
        };
        assert_eq!(users.get(&barbara).unwrap(), Some(user("Barbara", 80)));

        let err = users
            .upsert(age_filter(Op::Gt, 40), user("X", 1))
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::MultipleMatches { count: 4, .. }),
            "{err}"
        );
        assert_eq!(
            users.count(Filter::default()).unwrap(),
            5,
            "nothing written"
        );
    }

    /// Lookup and write are one atomic step: threads racing to upsert the
    /// same keys leave exactly one document per key.
    #[test]
    fn concurrent_upserts_of_one_key_insert_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<User>("users");
        users.ensure_index("name").unwrap();

        std::thread::scope(|scope| {
            for thread in 0..4 {
                let users = users.clone();
                scope.spawn(move || {
                    for key in 0..5 {
                        let name = format!("user {key}");
                        users.upsert(name_is(&name), user(&name, thread)).unwrap();
                    }
                });
            }
        });

        assert_eq!(users.count(Filter::default()).unwrap(), 5);
        for key in 0..5 {
            assert_eq!(users.count(name_is(&format!("user {key}"))).unwrap(), 1);
        }
    }
}
