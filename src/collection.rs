use crate::catalog::{Catalog, CollectionMeta, IndexMeta, IndexOptions};
use crate::cursor::Cursor;
use crate::data;
use crate::database::Database;
use crate::document::{DocId, Document, encode_document};
use crate::id::IdGenerator;
use crate::index::{BTreeIndex, Index, KeyRange, key};
use crate::query::{Filter, OrderedRead, QueryPlan, SortOrder};
use crate::storage::{PageId, PageStore, RecordLocation};
use crate::txn::WriteOp;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// What `ensure_index`, `ensure_unique_index` and `drop_index` take: one
/// field — `"age"`, `"address.city"`, `"tags[*]"` — or several, for a
/// compound index (SPEC §43): `["status", "created"]`.
pub trait IndexFields {
    fn into_fields(self) -> Vec<String>;
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

impl IndexFields for Vec<String> {
    fn into_fields(self) -> Vec<String> {
        self
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
pub struct Collection<T> {
    db: Database,
    name: String,
    /// `fn() -> T`, not `T`: the handle holds no `T`, so whether it's
    /// `Send`/`Sync` shouldn't depend on whether `T` is — a plain
    /// `PhantomData<T>` would make it so.
    _marker: PhantomData<fn() -> T>,
}

/// By hand, not derived: `#[derive(Clone)]` would require `T: Clone`,
/// though only the handle is cloned, never a `T`.
impl<T> Clone for Collection<T> {
    fn clone(&self) -> Self {
        Collection::new(self.db.clone(), self.name.clone())
    }
}

impl<T> Collection<T> {
    pub(crate) fn new(db: Database, name: impl Into<String>) -> Self {
        Self {
            db,
            name: name.into(),
            _marker: PhantomData,
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
    /// An existing unique index on the same fields is an error, not a
    /// match — see `ensure_unique_index`.
    pub fn ensure_index(&self, fields: impl IndexFields) -> crate::Result<bool> {
        self.ensure_index_with(fields, IndexOptions::default())
    }

    /// `ensure_index`, plus a constraint (SPEC §33): no two documents may
    /// have equal values in the field — equal as a filter's `Eq` sees it,
    /// so `1` and `1.0` count as equal, `"a"` and `"A"` don't. Null and
    /// missing values are exempt: any number of documents may lack the
    /// field. On several fields, no two documents may be equal in all of
    /// them, and a document with a null in any is exempt (SPEC §43.4). A
    /// write that would break it fails with `Error::DuplicateValue`, and
    /// its whole batch is rolled back.
    ///
    /// Creating it over documents that already break it fails the same
    /// way, naming two of them, and creates nothing. An existing
    /// non-unique index on the same fields is an error too: drop it
    /// first, then call this.
    pub fn ensure_unique_index(&self, fields: impl IndexFields) -> crate::Result<bool> {
        let unique = IndexOptions {
            unique: true,
            ..IndexOptions::default()
        };
        self.ensure_index_with(fields, unique)
    }

    /// `ensure_index` with `options`: unique (`ensure_unique_index`),
    /// sparse, or both. A sparse index (SPEC §44) has no entry for a
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
    /// let sparse = IndexOptions { sparse: true, ..IndexOptions::default() };
    /// users.ensure_index_with("nick", sparse)?;
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
                return Err(invalid(
                    "`_id` is the primary key; it needs no secondary index",
                ));
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
    /// if there was none.
    pub fn drop_index(&self, fields: impl IndexFields) -> crate::Result<bool> {
        let fields = fields.into_fields();
        self.db.transact(|catalog, store| {
            let Some(index) = catalog.drop_index(store, &self.name, &fields)? else {
                return Ok(false);
            };
            BTreeIndex::new(index.root).free_all(store)?;
            Ok(true)
        })
    }

    /// This collection's secondary indexes, in creation order — unique
    /// ones included: each by its field, a compound one by its fields in
    /// parentheses, `(status, created)`.
    pub fn indexes(&self) -> crate::Result<Vec<String>> {
        self.index_names(|_| true)
    }

    /// The names of `indexes` whose index is unique (SPEC §33).
    pub fn unique_indexes(&self) -> crate::Result<Vec<String>> {
        self.index_names(|index| index.unique)
    }

    /// The names of `indexes` whose index is sparse (SPEC §44).
    pub fn sparse_indexes(&self) -> crate::Result<Vec<String>> {
        self.index_names(|index| index.sparse)
    }

    fn index_names(&self, keep: impl Fn(&IndexMeta) -> bool) -> crate::Result<Vec<String>> {
        let state = self.db.read()?;
        Ok(state
            .catalog
            .indexes(&self.name)
            .iter()
            .filter(|index| keep(index))
            .map(IndexMeta::name)
            .collect())
    }

    /// How `find` would run `filter`: a full scan, or a range of one
    /// secondary index.
    pub fn explain(&self, filter: &Filter) -> crate::Result<QueryPlan> {
        let state = self.db.read()?;
        let indexes = state.catalog.indexes(&self.name);
        if let Some(order) = filter.index_order(indexes) {
            return Ok(QueryPlan::IndexOrder {
                field: order.index.name(),
            });
        }
        Ok(match filter.index_ranges(indexes) {
            None => QueryPlan::Scan,
            Some(ranges) => match &ranges[..] {
                [(index, _range)] => QueryPlan::Index {
                    field: index.name(),
                },
                _ => QueryPlan::IndexUnion {
                    fields: ranges.iter().map(|(index, _)| index.name()).collect(),
                },
            },
        })
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
        Collection::new(self.db.clone(), self.name.clone())
    }

    pub fn insert(&self, doc: T) -> crate::Result<DocId> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().insert(document)
    }

    pub fn get(&self, id: &DocId) -> crate::Result<Option<T>> {
        match self.as_document().get(id)? {
            Some(document) => Ok(Some(from_document(document)?)),
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
        self.as_document().update_matching(filter, |doc| {
            let mut value: T = from_document(doc)?;
            change(&mut value);
            Ok(crate::serde_bridge::to_document(&value)?)
        })
    }

    /// See the untyped `delete_many`. No document is converted to `T`.
    pub fn delete_many(&self, filter: Filter) -> crate::Result<usize> {
        self.as_document().delete_many(filter)
    }

    pub fn find(&self, filter: Filter) -> crate::Result<Vec<T>> {
        Ok(self
            .find_with_ids(filter)?
            .into_iter()
            .map(|(_id, doc)| doc)
            .collect())
    }

    /// `find`, plus each document's id — which a `T` has nowhere to carry
    /// (SPEC §13.5), so without this a caller can only get ids from
    /// `insert`, and loses them on restart. With it, a caller can look a
    /// document up by any field and then `update`/`delete` it by id.
    pub fn find_with_ids(&self, filter: Filter) -> crate::Result<Vec<(DocId, T)>> {
        self.as_document()
            .find_with_ids(filter)?
            .into_iter()
            .map(|(id, document)| Ok((id, from_document(document)?)))
            .collect()
    }

    /// The first match — see the untyped `find_one`.
    pub fn find_one(&self, filter: Filter) -> crate::Result<Option<T>> {
        Ok(self.find_one_with_id(filter)?.map(|(_id, doc)| doc))
    }

    pub fn find_one_with_id(&self, filter: Filter) -> crate::Result<Option<(DocId, T)>> {
        self.cursor(first_only(filter))?.next().transpose()
    }

    /// How many documents match — see the untyped `count`. No document is
    /// converted to `T`.
    pub fn count(&self, filter: Filter) -> crate::Result<usize> {
        self.as_document().count(filter)
    }

    /// A streaming `find` — see `Cursor`.
    pub fn cursor(&self, filter: Filter) -> crate::Result<Cursor<T>> {
        self.as_document().cursor_converting(filter, from_document)
    }

    /// Replaces the one document matching `filter`, or inserts `doc` if
    /// none does — see the untyped `upsert`.
    pub fn upsert(&self, filter: Filter, doc: T) -> crate::Result<Upserted> {
        let document = crate::serde_bridge::to_document(&doc)?;
        self.as_document().upsert(filter, document)
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

    pub fn insert(&self, doc: Document) -> crate::Result<DocId> {
        let id = self.db.id_gen().generate();
        self.write(WriteOp::Insert(self.name.clone(), id, doc))?;
        Ok(id)
    }

    pub fn get(&self, id: &DocId) -> crate::Result<Option<Document>> {
        let state = self.db.read()?;
        let Some(meta) = state.catalog.get(&self.name) else {
            return Ok(None); // collection doesn't exist yet, so neither does the document
        };

        let index = BTreeIndex::new(meta.index_root);
        let Some(loc) = index.lookup(&state.store, &key::primary(*id))? else {
            return Ok(None);
        };
        let (_id, doc) = data::get_record(&state.store, loc)?;
        Ok(Some(doc))
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

    /// Changes the documents `find(filter)` would return — `sort` and
    /// `limit` included — by calling `change` on each, in that order, and
    /// writes the ones it changed; returns how many (SPEC §38). One batch
    /// under one write lock, like `delete_many`: all changes land or none
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
        self.update_matching(filter, |mut doc| {
            change(&mut doc);
            Ok(doc)
        })
    }

    /// `update_many` for both paths: `change` maps each document found to
    /// its new version, or fails the whole batch.
    fn update_matching(
        &self,
        filter: Filter,
        mut change: impl FnMut(Document) -> crate::Result<Document>,
    ) -> crate::Result<usize> {
        self.db.transact(|catalog, store| {
            let found = find_in(catalog, store, &self.name, &filter)?;
            let mut changed = 0;
            for (id, old) in found {
                let new = with_id(change(old.clone())?, id);
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

    /// Deletes exactly the documents `find(filter)` would return — `sort`
    /// and `limit` included, so "the oldest 100" works — and says how many
    /// (SPEC §37). One batch under one write lock, like `upsert`: all of
    /// them go or none, and no write lands between the lookup and the
    /// deletes. A collection that doesn't exist has nothing to delete and
    /// isn't created.
    pub fn delete_many(&self, filter: Filter) -> crate::Result<usize> {
        self.db.transact(|catalog, store| {
            let doomed = find_in(catalog, store, &self.name, &filter)?;
            for (id, _doc) in &doomed {
                apply_write_op(catalog, store, &WriteOp::Delete(self.name.clone(), *id))?;
            }
            Ok(doomed.len())
        })
    }

    pub fn find(&self, filter: Filter) -> crate::Result<Vec<Document>> {
        Ok(self
            .find_with_ids(filter)?
            .into_iter()
            .map(|(_id, doc)| doc)
            .collect())
    }

    /// `find`, plus each document's id. An `Object` document already
    /// carries it as `_id` (SPEC §18); this also covers the others, and
    /// is what the typed `find_with_ids` builds on.
    ///
    /// Reads only the documents in one secondary index's range if the
    /// filter has a condition an index can answer (see `explain`), every
    /// document otherwise. Either way each candidate is checked against
    /// the whole filter — an index range may hold a few documents that
    /// don't match (SPEC §28.3). Without a `sort`, the order of the
    /// results is unspecified.
    ///
    /// With a `sort` and a `limit`, and an index on the sort field, it
    /// reads that index in order instead and stops after `limit` matches
    /// (SPEC §34.2). Equal sort values come in id order either way.
    pub fn find_with_ids(&self, filter: Filter) -> crate::Result<Vec<(DocId, Document)>> {
        let state = self.db.read()?;
        find_in(&state.catalog, &state.store, &self.name, &filter)
    }

    /// The first match, reading no further than it: the first in `sort`
    /// order if the filter has one, any match otherwise. `None` if nothing
    /// matches. With a `sort` on an indexed field this reads one index
    /// entry's worth of documents, not every match (SPEC §34.2).
    pub fn find_one(&self, filter: Filter) -> crate::Result<Option<Document>> {
        Ok(self.find_one_with_id(filter)?.map(|(_id, doc)| doc))
    }

    pub fn find_one_with_id(&self, filter: Filter) -> crate::Result<Option<(DocId, Document)>> {
        self.cursor(first_only(filter))?.next().transpose()
    }

    /// How many documents match `filter`'s conditions, at most its
    /// `limit`; `sort` is irrelevant. Without conditions it only counts
    /// the primary index's entries — no document is read.
    pub fn count(&self, filter: Filter) -> crate::Result<usize> {
        let state = self.db.read()?;
        let count = if filter.conditions.is_empty() {
            candidate_entries(&state.catalog, &state.store, &self.name, &filter)?.len()
        } else {
            read_candidates(&state.catalog, &state.store, &self.name, &filter)?
                .iter()
                .filter(|(_id, doc)| filter.matches(doc))
                .count()
        };
        Ok(filter.limit.map_or(count, |limit| count.min(limit)))
    }

    /// A streaming `find`: yields matches one at a time, holding only
    /// their ids in memory and no lock in between — see `Cursor` for what
    /// it sees of writes made meanwhile.
    pub fn cursor(&self, filter: Filter) -> crate::Result<Cursor<Document>> {
        self.cursor_converting(filter, Ok)
    }

    /// `cursor`, converting each document with `convert` — the typed
    /// path's `cursor` passes the serde bridge.
    fn cursor_converting<T>(
        &self,
        filter: Filter,
        convert: fn(Document) -> crate::Result<T>,
    ) -> crate::Result<Cursor<T>> {
        if !filter.sort.is_empty() {
            let results = self.find_with_ids(filter)?;
            let converted = results
                .into_iter()
                .map(|(id, doc)| Ok((id, convert(doc)?)))
                .collect::<crate::Result<Vec<_>>>()?;
            return Ok(Cursor::collected(converted));
        }
        let state = self.db.read()?;
        let entries = candidate_entries(&state.catalog, &state.store, &self.name, &filter)?;
        drop(state);
        let ids = entries.iter().map(|(key, _loc)| key::doc_id(key)).collect();
        Ok(Cursor::streaming(self.clone(), ids, filter, convert))
    }

    /// Replaces the one document matching `filter`'s conditions (keeping
    /// its id), or inserts `doc` with a new id if none matches;
    /// `Error::MultipleMatches` if several do. `sort` and `limit` don't
    /// matter. Atomic: the lookup and the write happen under one write
    /// lock, so two threads upserting the same key can't both insert —
    /// with a secondary index on the key's field, the lookup is cheap
    /// (SPEC §29.3).
    pub fn upsert(&self, filter: Filter, doc: Document) -> crate::Result<Upserted> {
        let id_if_new = self.db.id_gen().generate();
        self.db.transact(|catalog, store| {
            let matches: Vec<DocId> = read_candidates(catalog, store, &self.name, &filter)?
                .into_iter()
                .filter(|(_id, candidate)| filter.matches(candidate))
                .map(|(id, _candidate)| id)
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

/// What `find_with_ids` returns, against a catalog and store the caller
/// has locked — for a read, or inside a write batch (`delete_many`).
fn find_in(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
) -> crate::Result<Vec<(DocId, Document)>> {
    if let Some(order) = filter.index_order(catalog.indexes(collection)) {
        return read_in_index_order(catalog, store, collection, filter, order);
    }
    let mut candidates = read_candidates(catalog, store, collection, filter)?;
    if !filter.sort.is_empty() {
        // So equal sort values end up in id order, as they do when read
        // from an index (the sort is stable).
        candidates.sort_unstable_by_key(|(id, _doc)| *id);
    }
    Ok(filter.apply_to(candidates, |(_id, doc)| doc))
}

/// The index entries `find` has to look at for `filter`: the secondary
/// index ranges its conditions allow (SPEC §28.4, §36.3), otherwise the
/// whole primary index. Each entry's key ends with its document's id, and
/// each document comes once, even if several ranges hold it (an OR
/// whose branches overlap) or one range holds it several times (a
/// multikey index, SPEC §42.2).
fn candidate_entries(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
) -> std::io::Result<Vec<(Vec<u8>, RecordLocation)>> {
    let Some(meta) = catalog.get(collection) else {
        return Ok(Vec::new());
    };
    match filter.index_ranges(catalog.indexes(collection)) {
        None => BTreeIndex::new(meta.index_root).scan(store),
        Some(ranges) => {
            let mut seen = std::collections::HashSet::new();
            let mut entries = Vec::new();
            for (index, range) in ranges {
                for entry in BTreeIndex::new(index.root).range(store, &range)? {
                    if seen.insert(key::doc_id(&entry.0)) {
                        entries.push(entry);
                    }
                }
            }
            Ok(entries)
        }
    }
}

/// `find` for a filter `Filter::index_order` chose (SPEC §34.2): walks the
/// index in sort order (`BTreeIndex::walk`, SPEC §49), checks each
/// document against the whole filter, and stops as soon as `limit`
/// match — the entries and documents after that are never read. Already
/// filtered, sorted and limited.
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
fn read_in_index_order(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
    order: OrderedRead,
) -> crate::Result<Vec<(DocId, Document)>> {
    let (Some(first), Some(limit)) = (filter.sort.first(), filter.limit) else {
        unreachable!("index_order needs a sort and a limit");
    };
    let mut results = Vec::new();
    if limit == 0 {
        return Ok(results);
    }
    let index = order.index;
    let unordered_can_match = order.range.is_none() && !index.is_compound();
    let backward = first.order == SortOrder::Desc;
    let mut entries = BTreeIndex::new(index.root).walk(
        store,
        order.range.unwrap_or_else(KeyRange::everything),
        backward,
    );
    let fields = index.fields.len();
    let (at, end) = (order.fixed, order.fixed + order.served);
    let spans = Spans { at, end, fields };
    let in_memory = filter.sort.len() > order.served;
    let mut records = data::Records::new(store);
    let mut read = |loc: &RecordLocation| records.get(*loc);
    // Hands out one group's matching documents; `true` once `limit` are.
    let mut take = |group: Group, results: &mut Vec<(DocId, Document)>| -> crate::Result<bool> {
        // Ties in id order: fields after the served ones would order them
        // otherwise, and a backward walk hands them out last id first.
        let mut group = group;
        group.sort_by_key(|(key, _)| key::doc_id(key));
        let exact = spans
            .values(&group[0].0)
            .iter()
            .all(|part| key::part_is_exact(part, fields));
        if exact && !in_memory {
            for (_key, loc) in &group {
                let (id, doc) = read(loc)?;
                if filter.matches(&doc) {
                    results.push((id, doc));
                    if results.len() == limit {
                        return Ok(true);
                    }
                }
            }
        } else {
            let docs = group
                .iter()
                .map(|(_key, loc)| read(loc))
                .collect::<Result<Vec<_>, _>>()?;
            let unlimited = Filter {
                limit: None,
                ..filter.clone()
            };
            for found in unlimited.apply_to(docs, |(_id, doc)| doc) {
                results.push(found);
                if results.len() == limit {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    };
    let mut held = Held::new(backward && index.is_compound(), spans, first.order);
    let mut group: Group = Vec::new();
    loop {
        let entry = entries.next().transpose()?;
        let ends_group = match (&entry, group.first()) {
            (Some((key, _)), Some((first_key, _))) => spans.group(key) != spans.group(first_key),
            (None, Some(_)) => true,
            _ => false,
        };
        if ends_group {
            let next = entry.as_ref().map(|(key, _)| key.as_slice());
            for ready in held.pass(std::mem::take(&mut group), next) {
                if take(ready, &mut results)? {
                    return Ok(results);
                }
            }
        }
        match entry {
            Some(entry) => group.push(entry),
            None => break,
        }
    }
    let unordered_after = results.len();
    if unordered_can_match {
        let meta = catalog
            .get(collection)
            .expect("an indexed collection exists");
        let mut unordered = Vec::new();
        for (_key, loc) in BTreeIndex::new(meta.index_root).scan(store)? {
            let (id, doc) = read(&loc)?;
            let value = crate::query::value_or_null(&doc, &first.field);
            if crate::query::is_unordered(value) && filter.matches(&doc) {
                unordered.push((id, doc));
                // Without keys after the first, id order is their order.
                if !in_memory && unordered_after + unordered.len() == limit {
                    break;
                }
            }
        }
        let unlimited = Filter {
            conditions: Vec::new(),
            limit: None,
            ..filter.clone()
        };
        results.extend(unlimited.apply_to(unordered, |(_id, doc)| doc));
        results.truncate(limit);
    }
    Ok(results)
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
}

impl Spans {
    /// The bytes a group shares: the fixed values and the served ones —
    /// up to the first that may stand for several values (a cut string,
    /// a huge number): the keys after it don't order entries it holds,
    /// whose true values may differ; they're sorted in memory instead.
    /// A damaged key with fewer values than the index has fields groups
    /// by the ones it has (SPEC §55).
    fn group<'k>(&self, key: &'k [u8]) -> &'k [u8] {
        let parts = key::parts(key::value_part(key));
        let mut len: usize = parts.iter().take(self.at).map(|p| p.len()).sum();
        for part in parts.iter().take(self.end).skip(self.at) {
            len += part.len();
            if !key::part_is_exact(part, self.fields) {
                break;
            }
        }
        &key[..len]
    }

    /// The served values a group shares, each on its own.
    fn values<'k>(&self, key: &'k [u8]) -> Vec<&'k [u8]> {
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

/// The documents behind `candidate_entries`, not yet checked against the
/// filter — an index range may hold a few that don't match (SPEC §28.3).
fn read_candidates(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
) -> std::io::Result<Vec<(DocId, Document)>> {
    let mut records = data::Records::new(store);
    candidate_entries(catalog, store, collection, filter)?
        .into_iter()
        // The id stored in the data cell itself (SPEC §11).
        .map(|(_key, loc)| records.get(loc))
        .collect()
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
/// A document nested deeper than `MAX_NESTING` is refused like a name
/// too long (SPEC §56): an `InvalidInput` error, and the batch rolls back.
fn refuse_too_deep(collection: &str, id: &DocId, doc: &Document) -> crate::Result<()> {
    if doc.nests_too_deep() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "document {id} for collection {collection:?} nests deeper than the limit of {} levels",
                crate::document::MAX_NESTING
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
            let loc = data::insert_record(store, &mut current, *id, &doc)?;
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
            let secondary = catalog.indexes(collection);
            let old = old_document(secondary, store, loc)?;
            let mut current = meta.current_data_page;
            let new_loc = data::update_record(store, &mut current, loc, *id, &doc)?;
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
            let secondary = catalog.indexes(collection);
            let old = old_document(secondary, store, loc)?;
            data::delete_record(store, meta.current_data_page, loc)?;
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
    let mut tree = BTreeIndex::new(index.root);
    for (_key, loc) in BTreeIndex::new(meta.index_root).scan(store)? {
        let (id, doc) = data::get_record(store, loc)?;
        if index.unique {
            for (value_part, values) in unique_tuples(&doc, &index) {
                check_unique(store, collection, &index, id, &value_part, &values)?;
            }
        }
        for key in index_keys(&doc, &index, id) {
            tree.insert(store, &key, loc)?;
        }
    }
    Ok(true)
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

/// Merges `_id` into `doc` before it's stored, so the untyped path's
/// documents always report their own id when read back — LiteDB/Mongo
/// convention — even though the physical primary key lives outside the
/// document content as far as `data.rs`/`BTreeIndex` are concerned (see
/// SPEC §13.5). Overwrites any existing `_id` key rather than trusting
/// one the caller supplied, since the real id is always the one `insert`/
/// `update` were actually called with. Non-`Object` documents (a bare
/// `Document::Int`, a top-level `String`, ...) have no field to attach an
/// id to, so they pass through unchanged — not every document is shaped
/// to carry one.
fn with_id(doc: Document, id: DocId) -> Document {
    match doc {
        Document::Object(mut map) => {
            map.insert("_id".to_string(), Document::Id(id));
            Document::Object(map)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct User {
        name: String,
        age: i64,
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

        let id = users.insert(Document::String("Ada".to_string())).unwrap();

        let found = users.get(&id).unwrap();
        assert_eq!(found, Some(Document::String("Ada".to_string())));
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

        let id = users.insert(Document::Int(1)).unwrap();
        assert!(users.update(&id, Document::Int(2)).unwrap());

        assert_eq!(users.get(&id).unwrap(), Some(Document::Int(2)));
    }

    #[test]
    fn update_on_unknown_id_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        assert!(!users.update(&DocId([9; 16]), Document::Int(1)).unwrap());
    }

    #[test]
    fn delete_removes_it_from_get_and_find() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let id = users.insert(Document::Int(1)).unwrap();
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

    #[test]
    fn insert_overwrites_a_caller_supplied_id_field() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let mut fields = indexmap::IndexMap::new();
        fields.insert("_id".to_string(), Document::Id(DocId([0xFF; 16])));
        fields.insert("name".to_string(), Document::String("Ada".to_string()));
        let id = users.insert(Document::Object(fields)).unwrap();

        assert_ne!(
            id,
            DocId([0xFF; 16]),
            "the real id is always freshly generated"
        );
        let Document::Object(found) = users.get(&id).unwrap().unwrap() else {
            panic!("expected an Object back");
        };
        assert_eq!(
            found.get("_id"),
            Some(&Document::Id(id)),
            "the bogus caller-supplied _id must be overwritten with the real one"
        );
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

    #[test]
    fn non_object_documents_are_unaffected_by_id_merging() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");

        let id = users.insert(Document::Int(42)).unwrap();
        assert_eq!(users.get(&id).unwrap(), Some(Document::Int(42)));
    }

    #[test]
    fn two_collections_stay_independent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let posts = db.collection::<Document>("posts");

        let id = users.insert(Document::String("Ada".to_string())).unwrap();

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
                .map(|_| docs.insert(Document::Binary(vec![1; 100])).unwrap())
                .collect();
            for (n, id) in ids.iter().enumerate() {
                assert!(
                    docs.update(id, Document::Binary(vec![n as u8; 3000]))
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
                Some(Document::Binary(vec![n as u8; 3000]))
            );
        }
        assert_eq!(docs.find(Filter::default()).unwrap().len(), 20);

        // Deleting everything frees the data pages for reuse by new ones.
        let len_before = std::fs::metadata(&path).unwrap().len();
        for id in &ids {
            assert!(docs.delete(id).unwrap());
        }
        for _ in 0..20 {
            docs.insert(Document::Binary(vec![9; 3000])).unwrap();
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
    /// `insert`.
    #[test]
    fn typed_find_with_ids_finds_updatable_ids_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        {
            let db = Database::open(&path).unwrap();
            let users = db.collection::<User>("users");
            for (name, age) in [("Ada", 36), ("Grace", 45), ("Linus", 21)] {
                users
                    .insert(User {
                        name: name.to_string(),
                        age,
                    })
                    .unwrap();
            }
        }

        let db = Database::open(&path).unwrap();
        let users = db.collection::<User>("users");
        let found = users
            .find_with_ids(age_filter(crate::query::Op::Gt, 30))
            .unwrap();
        assert_eq!(found.len(), 2);
        for (id, user) in &found {
            assert_eq!(users.get(id).unwrap().as_ref(), Some(user));
        }

        let (grace_id, mut grace) = found
            .into_iter()
            .find(|(_id, user)| user.name == "Grace")
            .unwrap();
        grace.age = 46;
        assert!(users.update(&grace_id, grace.clone()).unwrap());
        assert_eq!(users.get(&grace_id).unwrap(), Some(grace));
    }

    /// Sort and limit act on the documents, and each id stays with its
    /// own document through both.
    #[test]
    fn find_with_ids_keeps_ids_paired_through_sort_and_limit() {
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

    /// Non-`Object` documents have no `_id` field (SPEC §18) — their ids
    /// are only available this way.
    #[test]
    fn untyped_find_with_ids_covers_non_object_documents() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let values = db.collection::<Document>("values");
        let a = values.insert(Document::Int(1)).unwrap();
        let b = values.insert(Document::String("two".to_string())).unwrap();

        let mut found = values.find_with_ids(Filter::default()).unwrap();
        found.sort_by_key(|(id, _doc)| *id);
        let mut expected = vec![
            (a, Document::Int(1)),
            (b, Document::String("two".to_string())),
        ];
        expected.sort_by_key(|(id, _doc)| *id);
        assert_eq!(found, expected);

        assert!(
            db.collection::<Document>("never_created")
                .find_with_ids(Filter::default())
                .unwrap()
                .is_empty()
        );
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
        match rng.below(10) {
            0 | 1 => Document::Int(rng.below(11) as i64 - 5),
            2 => Document::Int(big + rng.below(4) as i64),
            3 => Document::Float((rng.below(21) as f64 - 10.0) / 2.0),
            4 => [Document::Float(-0.0), Document::Float(f64::NAN)][rng.below(2)].clone(),
            5 | 6 => Document::String(["", "a", "ab", "b", "B", "a\0"][rng.below(6)].to_string()),
            7 => Document::String("x".repeat(1200) + ["a", "b", ""][rng.below(3)]),
            8 => Document::Bool(rng.below(2) == 1),
            _ => [Document::Null, Document::Array(vec![Document::Int(1)])][rng.below(2)].clone(),
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
                    format!("{name} {:?} {all:?}", docs.indexes().unwrap())
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
            let plan = docs.explain(&f).unwrap();
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
        assert_eq!(docs.indexes().unwrap(), vec!["v".to_string()]);
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
            let plan = docs.explain(&f).unwrap();
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
        assert_eq!(docs.indexes().unwrap(), paths);
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
        assert_eq!(copied.indexes().unwrap(), ["tags[*]", "comments[*].likes"]);
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
        users.ensure_unique_index("aliases[*]").unwrap();
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
            other.ensure_unique_index("aliases[*]"),
            Err(crate::Error::DuplicateValue { .. })
        ));
        assert_eq!(other.indexes().unwrap(), Vec::<String>::new());
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
        users.ensure_unique_index("aliases[*]").unwrap();
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
        assert_eq!(users.indexes().unwrap(), ["age", "name"]);
        assert_eq!(
            users.explain(&adults).unwrap(),
            QueryPlan::Index {
                field: "age".to_string()
            }
        );
        let with_indexes = file_len();

        assert!(users.drop_index("age").unwrap());
        assert!(!users.drop_index("age").unwrap());
        assert_eq!(users.indexes().unwrap(), ["name"]);
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
        assert!(docs.indexes().unwrap().is_empty());
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
        assert_eq!(docs.indexes().unwrap(), ["a.b.c"]);
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
    fn random_sorted_filter(rng: &mut XorShift) -> Filter {
        let ops = [Op::Eq, Op::Lt, Op::Lte, Op::Gt, Op::Gte];
        let conditions = match rng.below(6) {
            0 => vec![cond("v", ops[rng.below(5)].clone(), random_value(rng))],
            1 => vec![cond("w", Op::Eq, Document::Int(rng.below(3) as i64))],
            2 => vec![cond("v", Op::Ne, random_value(rng))],
            3 => vec![cond("pad", Op::Eq, Document::String(String::new()))],
            _ => vec![],
        };
        let order = [SortOrder::Asc, SortOrder::Desc][rng.below(2)];
        let field = ["v", "v", "w"][rng.below(3)].to_string();
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
        Filter {
            conditions,
            sort: vec![crate::query::Sort { field, order }],
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
        let mut plans = std::collections::HashSet::new();
        for _ in 0..300 {
            let f = random_sorted_filter(rng);
            let expected: Vec<DocId> = f
                .apply_to(all.clone(), |(_, doc)| doc)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let plan = docs.explain(&f).unwrap();
            let found: Vec<DocId> = docs
                .find_with_ids(f.clone())
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(found, expected, "{f:?} via {plan:?}");
            plans.insert(format!("{plan:?}"));
        }
        for plan in [
            r#"IndexOrder { field: "v" }"#,
            r#"IndexOrder { field: "w" }"#,
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
        Filter {
            conditions,
            sort,
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
                let plan = docs.explain(&f).unwrap();
                let found = ids(docs.find_with_ids(f.clone()).unwrap());
                if f.sort.is_empty() && f.limit.is_some() {
                    // Which documents a limit without a sort keeps isn't
                    // promised: any that match, as many as it allows.
                    let unlimited = Filter {
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
        assert_eq!(docs.indexes().unwrap(), names);
        check(&docs, &mut rng);
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
        users.ensure_unique_index(["tenant", "email"]).unwrap();
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
            other.ensure_unique_index(["tenant", "email"]),
            Err(crate::Error::DuplicateValue { .. })
        ));
        assert_eq!(other.indexes().unwrap(), Vec::<String>::new());
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
        assert!(docs.ensure_unique_index(["a", "c"]).unwrap());
        assert!(
            docs.ensure_unique_index(["a", "b"]).is_err(),
            "exists, not unique"
        );
        docs.insert(object(vec![
            ("a", 1.into()),
            ("b", 2.into()),
            ("c", 3.into()),
        ]))
        .unwrap();
        assert_eq!(
            docs.indexes().unwrap(),
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
        assert_eq!(copied.indexes().unwrap(), docs.indexes().unwrap());
        assert_eq!(copied.unique_indexes().unwrap(), ["(a, c)"]);
        assert_consistent(&copy);

        assert!(docs.drop_index(["b", "a"]).unwrap());
        assert!(!docs.drop_index(["b", "a"]).unwrap());
        assert!(!docs.drop_index("b").unwrap(), "no index on b alone");
        assert_eq!(docs.indexes().unwrap().len(), 3);
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
        let reads = records_read(|| assert_eq!(users.find(young).unwrap().len(), 10));
        assert_eq!(reads, 11, "a range includes its bound, age 10 (§28.4)");

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
    /// what checking every document finds — whichever plan it runs.
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
            let plan = docs.explain(&f).unwrap();
            let mut found = ids(docs.find_with_ids(f.clone()).unwrap());
            let streamed = docs
                .cursor(f.clone())
                .unwrap()
                .collect::<crate::Result<Vec<_>>>();
            let mut streamed = ids(streamed.unwrap());
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
        assert_eq!(plans.len(), 4, "scan, index, union and order all ran");
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
        users.ensure_unique_index("name").unwrap();
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
        tasks.ensure_unique_index("name").unwrap();
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
            assert!(docs.ensure_unique_index("email").unwrap());
            assert!(!docs.ensure_unique_index("email").unwrap());
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
        docs.ensure_unique_index("k").unwrap();
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
            duplicate(users.ensure_unique_index("name")),
            (second, first)
        );
        assert!(users.indexes().unwrap().is_empty());

        users.delete(&second).unwrap();
        assert!(users.ensure_unique_index("name").unwrap());

        // The other kind of index on the same field is refused, not
        // swapped: dropping it first is the caller's call.
        users.ensure_index("age").unwrap();
        for result in [users.ensure_unique_index("age"), users.ensure_index("name")] {
            let Err(crate::Error::Io(err)) = result else {
                panic!("expected a refusal, got {result:?}");
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert_eq!(users.indexes().unwrap(), ["name", "age"]);
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
            .catalog
            .indexes(collection)
            .iter()
            .find(|index| index.name() == name)
            .unwrap()
            .clone();
        BTreeIndex::new(index.root)
            .scan(&state.store)
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
        let reads = records_read(|| assert_eq!(members.count(by_nick).unwrap(), 33));
        assert_eq!(reads, 33);
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
        let id = docs
            .insert(wrap(nest(crate::document::MAX_NESTING - 1)))
            .unwrap();

        let refused = |e: crate::Error| match e {
            crate::Error::Io(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                assert!(e.to_string().contains("deeper than the limit of 64"), "{e}");
            }
            other => panic!("{other:?}"),
        };
        refused(
            docs.insert(wrap(nest(crate::document::MAX_NESTING)))
                .unwrap_err(),
        );
        refused(
            docs.update(&id, wrap(nest(crate::document::MAX_NESTING)))
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
        assert_eq!(stored["a"], nest(crate::document::MAX_NESTING - 1));
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
            members.ensure_unique_index("nick"),
            members.ensure_index_with("nick", both),
            members.ensure_unique_index("name"),
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

    /// The cursor holds no lock between items — the writes below would
    /// deadlock otherwise — and sees them as `Cursor` documents.
    #[test]
    fn a_cursor_streams_and_sees_writes_made_meanwhile() {
        let (_dir, _db, users) = users_db();
        let over_40 = age_filter(Op::Gt, 40);
        let ids: Vec<DocId> = users
            .find_with_ids(Filter::default())
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect(); // _id order: Ada, Grace, Alan, Edsger

        let mut cursor = users.cursor(over_40.clone()).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), (ids[1], user("Grace", 45)));

        assert!(users.delete(&ids[2]).unwrap()); // Alan: gone before he's read
        assert!(users.update(&ids[3], user("Edsger", 39)).unwrap()); // no longer matches
        assert!(users.update(&ids[0], user("Ada", 50)).unwrap()); // matches, but already passed
        users.insert(user("Barbara", 60)).unwrap(); // after the cursor was created

        assert!(cursor.next().is_none());

        let limited = Filter {
            limit: Some(1),
            ..Filter::default()
        };
        assert_eq!(users.cursor(limited).unwrap().count(), 1);
    }

    #[test]
    fn a_sorted_cursor_yields_in_order() {
        let (_dir, _db, users) = users_db();
        let ages: Vec<i64> = users
            .cursor(by_age(SortOrder::Desc))
            .unwrap()
            .map(|result| result.unwrap().1.age)
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
