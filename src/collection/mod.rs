use crate::aggregate::Aggregate;
use crate::catalog::{Catalog, CollectionMeta, IndexInfo, IndexMeta, IndexOptions};
use crate::cursor::{Convert, Cursor, Pending};
use crate::data::{self, Stored};
use crate::database::{Committed, Database, ReadGuard};
use crate::document::{DocId, Document};
use crate::explain::Trace;
use crate::id::IdGenerator;
use crate::index::{BTreeIndex, key};
use crate::live::{LiveAggregate, LiveQuery};
use crate::query::{Explained, Filter, QueryPlan, SortWork, Update, caught};
use crate::select::Selection;
use crate::storage::{PageStore, RecordLocation};
use crate::txn::WriteOp;
use serde::Serialize;
use serde::de::DeserializeOwned;

mod count;
mod find;
mod plan;
#[cfg(test)]
mod tests;
mod walk;
mod write;

pub(crate) use find::{Found, document_at, find_at};
use find::{answer_for, first_only, stored_at};
use plan::{follow_hint, plan_for};
pub(crate) use write::{
    apply_write_op, delete_matching_in, get_or_create_meta, index_keys, invalid_input,
    only_objects_in_old_file, rekey_old_indexes, same_values, unique_tuples, update_by_id,
    update_matching_in, upsert_fields_in,
};
use write::{build_index, found, primary_key_error};

/// One collection as a read sees it: the catalog that names its
/// indexes, the pages its documents and index entries are on, and its
/// name. The reads behind `Collection` — finding, counting, aggregating,
/// walking an index, weighing a plan — are its methods, spread over this
/// module's files, as these three always go together.
///
/// The pages are a snapshot's for a read, or the writer's staged ones
/// for the reads a batch makes as it goes (SPEC §162).
#[derive(Clone, Copy)]
pub(crate) struct Scope<'a> {
    catalog: &'a Catalog,
    store: &'a dyn PageStore,
    collection: &'a str,
}

impl<'a> Scope<'a> {
    pub(crate) fn new(catalog: &'a Catalog, store: &'a dyn PageStore, collection: &'a str) -> Self {
        Scope {
            catalog,
            store,
            collection,
        }
    }

    /// The collection's secondary indexes; none if it doesn't exist yet.
    fn indexes(self) -> &'a [IndexMeta] {
        self.catalog.indexes(self.collection)
    }

    /// The collection's entry in the catalog; `None` if it doesn't exist
    /// yet.
    fn meta(self) -> Option<&'a CollectionMeta> {
        self.catalog.get(self.collection)
    }
}

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

    /// A live aggregate (SPEC §139): first an `Entered` for each row of
    /// `aggregate` over what `filter` finds, then, for each commit that
    /// changes the collection, the rows that came into the result, left
    /// it, or changed. See [`LiveAggregate`].
    ///
    /// The aggregate runs again, whole, as each such commit commits,
    /// under the writer's lock: give it a `window` over an indexed
    /// date-time, or a filter that an index bounds, so that it reads a
    /// few documents, not the collection. Everything `aggregate` takes
    /// goes, the filter's and the rows' sort, skip and limit among it;
    /// refused as `aggregate` refuses. It waits for a batch being
    /// committed to finish, as `Database::subscribe` does. Opened
    /// read-only beside a writer, it runs again as that writer commits,
    /// as `watch` does there (SPEC §195).
    pub fn watch_aggregate(
        &self,
        filter: Filter,
        aggregate: &Aggregate,
    ) -> crate::Result<LiveAggregate> {
        self.db.watch_aggregate(&self.name, filter, aggregate, Ok)
    }

    /// `watch_aggregate`, its rows converted to an `R` of the
    /// application's own as they are received (SPEC §148): a struct with
    /// a field for each group field and column, by name, the group
    /// fields nested as their paths are. A row that doesn't convert is
    /// an `Err` item, and the live aggregate goes on; `group` stays a
    /// `Document`.
    ///
    /// ```
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb"))?;
    /// use trunkdb::{Document, LiveRow, query::{Aggregate, Filter}};
    ///
    /// #[derive(serde::Deserialize, Debug, PartialEq)]
    /// struct Load {
    ///     server: String,
    ///     mean: f64,
    /// }
    ///
    /// let readings = db.collection::<Document>("readings");
    /// let per_server = Aggregate::new().group_by("server").avg("load").named("mean");
    /// let live = readings.watch_aggregate_as::<Load>(Filter::new(), &per_server)?;
    /// readings.insert(r#"{"server": "a", "load": 0.5}"#.parse()?)?;
    /// let Some(Ok(LiveRow::Entered { row, .. })) = live.recv() else { panic!() };
    /// assert_eq!(row, Load { server: "a".into(), mean: 0.5 });
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn watch_aggregate_as<R: DeserializeOwned>(
        &self,
        filter: Filter,
        aggregate: &Aggregate,
    ) -> crate::Result<LiveAggregate<R>> {
        self.db
            .watch_aggregate(&self.name, filter, aggregate, |row| {
                Ok(crate::serde_bridge::from_document(row)?)
            })
    }

    /// The collection's name.
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
        let filter = &*filter.resolved()?;
        let state = self.read()?;
        let indexes = state.catalog().indexes(&self.name);
        follow_hint(&self.name, filter, indexes)?;
        Ok(plan_for(filter, indexes))
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
    /// assert!(matches!(&adults.plan, QueryPlan::Index { field, .. } if field == "age"));
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
    /// The documents `find(filter)` would return, put in groups and
    /// summed up as `aggregate` says — SQL's `GROUP BY` (SPEC §107). One
    /// row per group, a document of the group's values and each column;
    /// see `Aggregate`. `aggregate_as` makes each row a struct of the
    /// application's own (SPEC §148).
    ///
    /// Each match is decoded only on the fields the aggregate reads.
    /// Where an index's entries decide the filter and hold every field
    /// the aggregate reads, they are aggregated without their documents
    /// for `count`, `avg`, `min` and `max`; `sum` reads its numbers from
    /// the documents, as a key doesn't keep whether a number was an
    /// `Int` or a `Float` (§107.5). Without conditions, an index whose
    /// entries hold every document and every such field is walked
    /// instead of every document. Refused, before anything is read, as
    /// `Aggregate` says.
    pub fn aggregate(&self, filter: Filter, aggregate: &Aggregate) -> crate::Result<Vec<Document>> {
        let (filter, aggregate, _now) = at_now(&filter, aggregate)?;
        let state = self.read()?;
        let at = state.snapshot();
        let trace = Trace::default();
        let (rows, _plan) = at
            .scope(&self.name)
            .aggregate_in(&filter, &aggregate, &trace)?;
        Ok(rows)
    }

    /// `aggregate`, each row converted to an `R` of the application's
    /// own (SPEC §148): a struct with a field for each group field and
    /// column, by name, the group fields nested as their paths are, an
    /// `Option` where a value may be `null`. A row that doesn't convert
    /// fails the call with `Error::Document`, as a typed `find` does.
    ///
    /// ```
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb"))?;
    /// use trunkdb::{Document, query::{Aggregate, Filter}};
    ///
    /// #[derive(serde::Deserialize, Debug, PartialEq)]
    /// struct PerStatus {
    ///     status: String,
    ///     n: i64,
    /// }
    ///
    /// let tasks = db.collection::<Document>("tasks");
    /// tasks.insert(r#"{"status": "open"}"#.parse()?)?;
    /// let per_status = Aggregate::new().group_by("status").count().named("n");
    /// let rows: Vec<PerStatus> = tasks.aggregate_as(Filter::new(), &per_status)?;
    /// assert_eq!(rows, [PerStatus { status: "open".into(), n: 1 }]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn aggregate_as<R: DeserializeOwned>(
        &self,
        filter: Filter,
        aggregate: &Aggregate,
    ) -> crate::Result<Vec<R>> {
        (self.aggregate(filter, aggregate)?.into_iter())
            .map(|row| Ok(crate::serde_bridge::from_document(row)?))
            .collect()
    }

    /// Runs `aggregate` as `aggregate` does and says how (SPEC §107.6),
    /// as `explain_run` does for `find`: the plan, the index reads, the
    /// documents read, the matches aggregated from their entries alone
    /// (`from_entries`), the groups, and the rows (`documents_returned`).
    pub fn explain_aggregate(
        &self,
        filter: Filter,
        aggregate: &Aggregate,
    ) -> crate::Result<Explained> {
        let started = std::time::Instant::now();
        let (filter, aggregate, now) = at_now(&filter, aggregate)?;
        let state = self.read()?;
        let at = state.snapshot();
        let trace = Trace::default();
        let (rows, plan) = at
            .scope(&self.name)
            .aggregate_in(&filter, &aggregate, &trace)?;
        let mut explained = trace.explained(plan, None, None, rows.len(), started.elapsed());
        explained.hinted = filter.hint.is_some();
        explained.now = now;
        Ok(explained)
    }

    /// Runs `filter` as `find` does and says how it ran (SPEC §94): the
    /// plan, each index read with its bounds, how it sorted, the entries
    /// and documents examined, matched and returned, and the time. The
    /// documents found aren't handed out.
    pub fn explain_run(&self, filter: Filter) -> crate::Result<Explained> {
        let started = std::time::Instant::now();
        let (filter, now) = filter.resolved_owned()?;
        let state = self.read()?;
        let at = state.snapshot();
        let indexes = at.catalog.indexes(&self.name);
        follow_hint(&self.name, &filter, indexes)?;
        let plan = plan_for(&filter, indexes);
        // A key of the application's is one more sort key, in memory.
        let sort = match filter.sort.len() + usize::from(filter.by_key.is_some()) {
            0 => None,
            keys => {
                let from_index = match filter.index_order(indexes) {
                    Some(order) => order.served,
                    // Merged by id, which no two documents share (§167).
                    None if filter.merged_by_id(indexes).is_some() => filter.sort.len(),
                    None => 0,
                };
                Some(SortWork {
                    from_index,
                    in_memory: keys - from_index,
                })
            }
        };
        let trace = Trace::default();
        let select = filter.selection();
        let found = at
            .scope(&self.name)
            .find_in(&filter, true, select.as_ref(), &trace)?;
        let mut explained = trace.explained(
            plan,
            sort,
            filter.select.clone(),
            found.len(),
            started.elapsed(),
        );
        explained.hinted = filter.hint.is_some();
        explained.except = filter.except.clone();
        explained.now = now;
        explained.alternatives = at.scope(&self.name).alternatives(
            &filter,
            &explained.plan,
            explained.stats.keys_examined,
        )?;
        Ok(explained)
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

    /// Inserts `doc`, under the id in its `_id` field if that's `Some`
    /// (SPEC §59), and returns the id it's stored under.
    pub fn insert(&self, doc: T) -> crate::Result<DocId> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().insert(document)
    }

    /// The document `id`, as a `T`; `None` if there's none.
    pub fn get(&self, id: &DocId) -> crate::Result<Option<T>> {
        match self.as_document().stored(id)? {
            Some(stored) => Ok(Some(stored.deserialize()?)),
            None => Ok(None),
        }
    }

    /// A live query of `filter` (SPEC §138), its documents converted to
    /// `T` as they are received: see [`LiveQuery`]. As the untyped
    /// `watch`; a document that doesn't convert is an `Err` item, and
    /// the query goes on.
    pub fn watch(&self, filter: Filter) -> crate::Result<LiveQuery<T>> {
        self.db.watch(&self.name, filter, |doc| {
            Ok(crate::serde_bridge::from_document(doc)?)
        })
    }

    /// Replaces document `id` with `doc`, which keeps its id. `false` if
    /// there's no document `id`, as for the untyped `update`.
    pub fn update(&self, id: &DocId, doc: T) -> crate::Result<bool> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().update(id, document)
    }

    /// Deletes document `id`. `false` if there's none, as for `update`.
    pub fn delete(&self, id: &DocId) -> crate::Result<bool> {
        self.as_document().delete(id)
    }

    /// See the untyped `update_many`: `change` gets each match as a `T`.
    /// A document that doesn't convert to `T` fails the whole batch.
    /// Whether a document changed is judged after the round trip through
    /// `T`, so one written before a field was added to `T` counts as
    /// changed — it gets the field.
    ///
    /// **Each match is stored as the `T` it became, whole: a stored field
    /// `T` doesn't declare is dropped,** even where `change` does nothing
    /// (SPEC §184.3). An older build of an application, its `T` without a
    /// field a newer one added, erases that field from every document it
    /// touches this way. To change some fields and keep every other as it
    /// is stored, use `update_fields`, or the untyped `update_many`.
    pub fn update_many(
        &self,
        filter: Filter,
        mut change: impl FnMut(&mut T),
    ) -> crate::Result<usize> {
        // `T`'s conversions are the application's code too (SPEC §137).
        self.as_document().update_matching(filter, |_id, doc| {
            caught(|| -> crate::Result<Document> {
                let mut value: T = from_document(doc)?;
                change(&mut value);
                Ok(crate::serde_bridge::to_document(&value)?)
            })?
        })
    }

    /// See the untyped `update_fields`. Each document the update changes
    /// must still convert to `T`, or the whole batch fails: a field set
    /// to a value of the wrong type is caught here, not by the next read.
    pub fn update_fields(&self, filter: Filter, update: &Update) -> crate::Result<usize> {
        self.as_document()
            .update_fields_then(filter, update, |doc| {
                caught(|| from_document::<T>(doc.clone()).map(drop))?
            })
    }

    /// See the untyped `delete_many`. No document is converted to `T`.
    pub fn delete_many(&self, filter: Filter) -> crate::Result<usize> {
        self.as_document().delete_many(filter)
    }

    /// What `filter` finds, as `T`s — see the untyped `find`. With a
    /// `select`, `T` has to deserialize from the fields selected.
    pub fn find(&self, filter: Filter) -> crate::Result<Vec<T>> {
        let select = filter.selection();
        self.as_document()
            .matches_found(&filter, false, select.as_ref())?
            .into_iter()
            .map(|found| found.deserialize(select.as_ref()))
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
            .cursor_converting(filter, false, |found, select| found.deserialize(select))
    }

    /// Replaces the one document matching `filter`, or inserts `doc` if
    /// none does — see the untyped `upsert`.
    pub fn upsert(&self, filter: Filter, doc: T) -> crate::Result<Upserted> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().upsert(filter, document)
    }

    /// See the untyped `upsert_fields`. The document written, inserted or
    /// updated, must convert to `T`, or nothing is: a `T` with a field the
    /// filter and the operators don't give fails as an insert.
    pub fn upsert_fields(&self, filter: Filter, update: &Update) -> crate::Result<Upserted> {
        self.as_document()
            .upsert_fields_then(filter, update, |doc| {
                caught(|| from_document::<T>(doc.clone()).map(drop))?
            })
    }
}

/// The id an insert stores `doc` under (SPEC §59): the id in its `_id`
/// field, if it has one and it isn't `DocId::NIL`; otherwise a new one.
/// Any other `_id` (a string, `null`) is overwritten, as before.
/// `filter` and `aggregate` as a read runs them (SPEC §203): the clock
/// read once for both where either compares with it, and that time.
/// Refused as `Filter::resolved` refuses.
fn at_now<'r>(
    filter: &'r Filter,
    aggregate: &'r Aggregate,
) -> crate::Result<(
    std::borrow::Cow<'r, Filter>,
    std::borrow::Cow<'r, Aggregate>,
    Option<crate::DateTime>,
)> {
    use std::borrow::Cow;
    if !filter.has_now() && !aggregate.has_now() {
        return Ok((Cow::Borrowed(filter), Cow::Borrowed(aggregate), None));
    }
    crate::query::refuse_against_now(&filter.conditions)?;
    aggregate.check()?;
    let now = crate::DateTime::now();
    Ok((
        Cow::Owned(filter.clone().now_is(now)),
        Cow::Owned(aggregate.now_is(now)),
        Some(now),
    ))
}

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
    /// The id of the document written, inserted or updated.
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

    /// A live query of `filter` (SPEC §138): first an `Entered` for each
    /// document that matches now, then what each commit after changes,
    /// documents that came to match, no longer match, or were written
    /// and still match. See [`LiveQuery`].
    ///
    /// `filter`'s conditions decide, its `select` cuts what is handed
    /// out, and a hint steers the first read only. With a sort, and a
    /// limit or none, it is a list in that order (SPEC §145), "the ten
    /// fastest vans", and each event says where in it: see [`Live`](crate::Live). A
    /// skip, or a limit without a sort, is refused with
    /// `Error::NotLive`. It waits for a batch being committed to finish,
    /// as `Database::subscribe` does.
    ///
    /// **Opened read-only beside a writer in another process,** it is
    /// read again as that writer commits (SPEC §195): every
    /// `OpenOptions::watch_every`, where the WAL gained a commit, what
    /// changed since the last look is reported as one commit's would be.
    /// Several commits between two looks come as one, and a document that
    /// came and went between them isn't seen.
    pub fn watch(&self, filter: Filter) -> crate::Result<LiveQuery<Document>> {
        self.db.watch(&self.name, filter, Ok)
    }

    /// The document `id`; `None` if there's none.
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
        stored_at(&state.snapshot(), &self.name, id)
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
    /// not use this database itself, which would deadlock. If it panics,
    /// the call fails with `Error::ClosurePanicked`, nothing is written,
    /// and the database goes on (SPEC §137).
    pub fn update_many(
        &self,
        filter: Filter,
        mut change: impl FnMut(&mut Document),
    ) -> crate::Result<usize> {
        self.update_matching(filter, |_id, mut doc| {
            caught(|| change(&mut doc))?;
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
        update.check().map_err(invalid_input)?;
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
            update_matching_in(catalog, store, &self.name, &filter, &mut change)
        })
    }

    /// Deletes exactly the documents `find(filter)` would return — `sort`,
    /// `skip` and `limit` included, so "the oldest 100" works — and says how many
    /// (SPEC §37). One batch under the writer's lock, like `upsert`: all of
    /// them go or none, and no write lands between the lookup and the
    /// deletes. A collection that doesn't exist has nothing to delete and
    /// isn't created.
    pub fn delete_many(&self, filter: Filter) -> crate::Result<usize> {
        self.db
            .transact(|catalog, store| delete_matching_in(catalog, store, &self.name, &filter))
    }

    /// Every match with the id of its cell: what `find` and a sorted
    /// `cursor` read.
    pub(crate) fn matches_with_ids(
        &self,
        filter: &Filter,
    ) -> crate::Result<Vec<(DocId, Document)>> {
        let select = filter.selection();
        self.matches_found(filter, true, select.as_ref())?
            .into_iter()
            .map(|found| Ok((found.id(), found.decode(select.as_ref())?)))
            .collect()
    }

    /// Every match, in `find`'s order, for the caller to decode as a
    /// `Document` or a `T` (SPEC §99.3) — `whole_from_index` as in
    /// `find_in`.
    fn matches_found(
        &self,
        filter: &Filter,
        whole_from_index: bool,
        select: Option<&Selection>,
    ) -> crate::Result<Vec<Found>> {
        let filter = &*filter.resolved()?;
        let state = self.read()?;
        let at = state.snapshot();
        let trace = Trace::default();
        at.scope(&self.name)
            .find_in(filter, whole_from_index, select, &trace)
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
        let filter = filter.resolved()?;
        let state = self.read()?;
        let at = state.snapshot();
        at.scope(&self.name)
            .count_matches(&filter, &Trace::default())
    }

    /// A streaming `find`: yields matches one at a time, holding only
    /// where they are in memory, and no lock. It shows the collection as
    /// it was when the cursor was made, and keeps that moment until it's
    /// dropped — see `Cursor`.
    pub fn cursor(&self, filter: Filter) -> crate::Result<Cursor<Document>> {
        self.cursor_converting(filter, true, |found, select| Ok(found.decode(select)?))
    }

    /// `cursor`, decoding each match with `convert` — into a `Document`,
    /// or a `T` for the typed path (SPEC §99.3).
    fn cursor_converting<T>(
        &self,
        filter: Filter,
        whole_from_index: bool,
        convert: Convert<T>,
    ) -> crate::Result<Cursor<T>> {
        // The clock read once, for the candidates and every document
        // read after them (SPEC §203).
        let (filter, _now) = filter.resolved_owned()?;
        let select = filter.selection();
        if filter.sorted() {
            let results = self.matches_found(&filter, whole_from_index, select.as_ref())?;
            let converted = results
                .into_iter()
                .map(|found| convert(found, select.as_ref()))
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
        follow_hint(&self.name, &filter, at.catalog.indexes(&self.name))?;
        let (entries, indexed) = at
            .scope(&self.name)
            .candidate_entries(&filter, &Trace::default())?;
        // Where the entries decide (SPEC §100), a rejected one is left
        // out, and a match is kept as decided: passed over by a skip
        // unread. Where they may answer the selection (§106.5), with its
        // key.
        let mut decides = indexed.covering(&filter, false).map(|c| c.decider(&filter));
        let answer = answer_for(select.as_ref(), indexed, &filter);
        let candidates = entries
            .into_iter()
            .filter_map(
                |(key, loc)| match decides.as_mut().and_then(|d| d.decide(&key)) {
                    Some(true) => Some(Pending::Matched(loc, answer.is_some().then_some(key))),
                    Some(false) => None,
                    None => Some(Pending::Unchecked(loc)),
                },
            )
            .collect();
        drop(state);
        Ok(Cursor::streaming(
            kept, candidates, filter, select, answer, convert,
        ))
    }

    /// The document at `loc`, as this handle's commit has it: for a
    /// cursor, which took `loc` from an index of the same commit.
    pub(crate) fn stored_at(&self, loc: RecordLocation) -> crate::Result<Stored> {
        debug_assert!(
            self.at.is_some(),
            "a location is only good within one commit"
        );
        let state = self.read()?;
        let at = state.snapshot();
        Ok(data::Records::new(&at.store, at.catalog.names()).stored(loc)?)
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
            let (op, outcome) = match self.only_match(catalog, store, &filter)? {
                None => (
                    WriteOp::Insert(self.name.clone(), id_if_new, doc),
                    Upserted::Inserted(id_if_new),
                ),
                Some(id) => (
                    WriteOp::Update(self.name.clone(), id, doc),
                    Upserted::Updated(id),
                ),
            };
            apply_write_op(catalog, store, &op)?;
            Ok(outcome)
        })
    }

    /// `upsert` by `update`'s operators (SPEC §144.2): the one document
    /// matching `filter`'s conditions gets them, as `update_fields` would
    /// make them; where none matches, a new one is inserted, made of the
    /// filter's `eq`s and then the operators, `set_on_insert` included.
    /// `Error::MultipleMatches` if several match.
    ///
    /// ```no_run
    /// # fn main() -> trunkdb::Result<()> {
    /// use trunkdb::{Database, DateTime, Document, query::{Filter, Update}};
    ///
    /// let db = Database::open("app.trunkdb")?;
    /// let visits = db.collection::<Document>("visits");
    /// // { page: "/home", hits: 1, first: … } the first time, hits + 1 after.
    /// visits.upsert_fields(
    ///     Filter::new().eq("page", "/home"),
    ///     &Update::new().inc("hits", 1).set_on_insert("first", DateTime::now()),
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The new document's fields come from each `eq` at the top of the
    /// filter or in an AND, on a path an update could name; an `eq` on
    /// `_id` gives its id. ORs, NOTs and other comparisons are left out:
    /// they don't say one value. A filter that gives a field two values,
    /// or a value to a field and to one inside it, is refused before
    /// anything is read (`ErrorKind::InvalidInput`), as an update that
    /// `update_fields` would refuse is. A match the operators leave as it
    /// was isn't written, and is still `Upserted::Updated`.
    pub fn upsert_fields(&self, filter: Filter, update: &Update) -> crate::Result<Upserted> {
        self.upsert_fields_then(filter, update, |_doc| Ok(()))
    }

    /// `upsert_fields`, with `accept` to refuse the document written: the
    /// typed path's check that it still converts to `T`.
    fn upsert_fields_then(
        &self,
        filter: Filter,
        update: &Update,
        mut accept: impl FnMut(&Document) -> crate::Result<()>,
    ) -> crate::Result<Upserted> {
        update.check().map_err(invalid_input)?;
        let (named, seed) = crate::update::seed(&filter.conditions).map_err(invalid_input)?;
        let id_if_new = named.unwrap_or_else(|| self.db.id_gen().generate());
        self.db.transact(|catalog, store| {
            upsert_fields_in(
                catalog,
                store,
                &self.name,
                &filter,
                (id_if_new, &seed),
                update,
                &mut accept,
            )
            .map(|(upserted, _)| upserted)
        })
    }

    /// The id of the one document matching `filter`'s conditions, `None`
    /// if none does; `Error::MultipleMatches` if several do: what an
    /// upsert writes to (SPEC §29.3).
    fn only_match(
        &self,
        catalog: &Catalog,
        store: &dyn PageStore,
        filter: &Filter,
    ) -> crate::Result<Option<DocId>> {
        Scope::new(catalog, store, &self.name).only_match_in(filter)
    }
}

// Each match with its id, for tests that need the id of a document they
// didn't insert themselves, or of one that isn't an object. The public
// `find_with_ids` and `find_one_with_id` were removed before 1.0 (SPEC §74).
#[cfg(test)]
impl<T: Serialize + DeserializeOwned> Collection<T> {
    pub(crate) fn find_with_ids(&self, filter: Filter) -> crate::Result<Vec<(DocId, T)>> {
        self.as_document()
            .matches_found(&filter, false, filter.selection().as_ref())?
            .into_iter()
            .map(|found| Ok((found.id(), found.deserialize(filter.selection().as_ref())?)))
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
