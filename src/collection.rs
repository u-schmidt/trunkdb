use crate::catalog::{Catalog, CollectionMeta, IndexMeta};
use crate::cursor::Cursor;
use crate::data;
use crate::database::Database;
use crate::document::{DocId, Document};
use crate::id::IdGenerator;
use crate::index::{BTreeIndex, Index, key};
use crate::query::{Filter, QueryPlan};
use crate::storage::{PageId, PageStore, RecordLocation};
use crate::txn::WriteOp;
use serde::Serialize;
use serde::de::DeserializeOwned;
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

    /// Makes sure there's a secondary index on `field` (SPEC §28): `true`
    /// if it was created now — from every document already stored, in
    /// one atomic batch — `false` if it already existed. Creates the
    /// collection if needed, so indexes can be declared up front. From
    /// then on every write keeps the index up to date, and `find` uses it
    /// for `Eq`/`Lt`/`Lte`/`Gt`/`Gte` conditions on `field`. The index
    /// persists; call this at startup.
    ///
    /// `field` can be a dotted path into nested objects, like
    /// `address.city` (SPEC §31); a document without that path just has
    /// no entry. `_id` is always indexed (the primary index) and is
    /// rejected here, and so are paths below it and paths with an empty
    /// part (`a..b`, `.a`).
    pub fn ensure_index(&self, field: &str) -> crate::Result<bool> {
        let invalid = |message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
        if field.split('.').next() == Some("_id") {
            return Err(invalid("`_id` is the primary key; it needs no secondary index").into());
        }
        if field.split('.').any(str::is_empty) {
            return Err(invalid("an index path needs a name between every two dots").into());
        }
        self.db
            .transact(|catalog, store| build_index(catalog, store, &self.name, field))
    }

    /// Drops the secondary index on `field` and frees its pages: `false`
    /// if there was none.
    pub fn drop_index(&self, field: &str) -> crate::Result<bool> {
        self.db.transact(|catalog, store| {
            let Some(index) = catalog.drop_index(store, &self.name, field)? else {
                return Ok(false);
            };
            BTreeIndex::new(index.root).free_all(store)?;
            Ok(true)
        })
    }

    /// The fields this collection has secondary indexes on, in creation
    /// order.
    pub fn indexes(&self) -> crate::Result<Vec<String>> {
        let state = self.db.read()?;
        Ok(state
            .catalog
            .indexes(&self.name)
            .iter()
            .map(|index| index.field.clone())
            .collect())
    }

    /// How `find` would run `filter`: a full scan, or a range of one
    /// secondary index.
    pub fn explain(&self, filter: &Filter) -> crate::Result<QueryPlan> {
        let state = self.db.read()?;
        Ok(
            match filter.index_range(state.catalog.indexes(&self.name)) {
                Some((index, _range)) => QueryPlan::Index {
                    field: index.field.clone(),
                },
                None => QueryPlan::Scan,
            },
        )
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
        self.cursor(filter)?.next().transpose()
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
    pub fn find_with_ids(&self, filter: Filter) -> crate::Result<Vec<(DocId, Document)>> {
        let state = self.db.read()?;
        let candidates = read_candidates(&state.catalog, &state.store, &self.name, &filter)?;
        // Filtering needs no lock: the candidates are owned copies.
        drop(state);
        Ok(filter.apply_to(candidates, |(_id, doc)| doc))
    }

    /// The first match, reading no further than it: the first in `sort`
    /// order if the filter has one (which does read every match), any
    /// match otherwise. `None` if nothing matches.
    pub fn find_one(&self, filter: Filter) -> crate::Result<Option<Document>> {
        Ok(self.find_one_with_id(filter)?.map(|(_id, doc)| doc))
    }

    pub fn find_one_with_id(&self, filter: Filter) -> crate::Result<Option<(DocId, Document)>> {
        self.cursor(filter)?.next().transpose()
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
        if filter.sort.is_some() {
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

/// The index entries `find` has to look at for `filter`: one secondary
/// index's range if a condition allows (SPEC §28.4), otherwise the whole
/// primary index. Each entry's key ends with its document's id.
fn candidate_entries(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
) -> std::io::Result<Vec<(Vec<u8>, RecordLocation)>> {
    let Some(meta) = catalog.get(collection) else {
        return Ok(Vec::new());
    };
    match filter.index_range(catalog.indexes(collection)) {
        Some((index, range)) => BTreeIndex::new(index.root).range(store, &range),
        None => BTreeIndex::new(meta.index_root).scan(store),
    }
}

/// The documents behind `candidate_entries`, not yet checked against the
/// filter — an index range may hold a few that don't match (SPEC §28.3).
fn read_candidates(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
) -> std::io::Result<Vec<(DocId, Document)>> {
    candidate_entries(catalog, store, collection, filter)?
        .into_iter()
        // The id stored in the data cell itself (SPEC §11).
        .map(|(_key, loc)| data::get_record(store, loc))
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
            let mut current = meta.current_data_page;
            let loc = data::insert_record(store, &mut current, *id, &doc)?;
            index.insert(store, &key::primary(*id), loc)?;
            let secondary = catalog.indexes(collection);
            update_secondary_indexes(secondary, store, *id, None, Some((&doc, loc)))?;
            save_current_data_page(catalog, store, collection, &meta, current)
        }
        WriteOp::Update(collection, id, doc) => {
            let (meta, loc) = locate(catalog, store, collection, id)?;
            let mut index = BTreeIndex::new(meta.index_root);
            let doc = with_id(doc.clone(), *id);
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
            update_secondary_indexes(secondary, store, *id, old, Some((&doc, new_loc)))?;
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
            update_secondary_indexes(secondary, store, *id, old, None)?;
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

/// A document's key in the secondary index on `field`, if it has one: the
/// field must exist and hold an indexed type (`key::encode_value`).
fn secondary_key(doc: &Document, field: &str, id: DocId) -> Option<Vec<u8>> {
    key::secondary(crate::query::field_value(doc, field)?, id)
}

/// Brings every secondary index from a document's `old` state to its
/// `new` one — each `(document, location)`, `None` for "not there":
/// insert is `(None, new)`, delete `(old, None)`. An index whose key and
/// location both stayed the same isn't touched.
fn update_secondary_indexes(
    indexes: &[IndexMeta],
    store: &mut dyn PageStore,
    id: DocId,
    old: Option<(&Document, RecordLocation)>,
    new: Option<(&Document, RecordLocation)>,
) -> std::io::Result<()> {
    for index in indexes {
        let entry = |state: Option<(&Document, RecordLocation)>| {
            state.and_then(|(doc, loc)| Some((secondary_key(doc, &index.field, id)?, loc)))
        };
        let (before, after) = (entry(old), entry(new));
        if before == after {
            continue;
        }
        let mut tree = BTreeIndex::new(index.root);
        if let Some((key, _loc)) = before {
            tree.remove(store, &key)?;
        }
        if let Some((key, loc)) = after {
            tree.insert(store, &key, loc)?;
        }
    }
    Ok(())
}

/// Creates the secondary index on `field` and fills it from every
/// document in the collection — `false` if it already exists.
fn build_index(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    field: &str,
) -> crate::Result<bool> {
    if catalog.indexes(collection).iter().any(|i| i.field == field) {
        return Ok(false);
    }
    let meta = get_or_create_meta(catalog, store, collection)?;
    let index = catalog.create_index(store, collection, field)?;
    let mut tree = BTreeIndex::new(index.root);
    for (_key, loc) in BTreeIndex::new(meta.index_root).scan(store)? {
        let (id, doc) = data::get_record(store, loc)?;
        if let Some(key) = secondary_key(&doc, field, id) {
            tree.insert(store, &key, loc)?;
        }
    }
    Ok(true)
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
/// SPEC.md §13.5). Overwrites any existing `_id` key rather than trusting
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
            conditions: vec![crate::query::Condition {
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
            conditions: vec![crate::query::Condition {
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
            conditions: vec![crate::query::Condition {
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
            sort: Some(crate::query::Sort {
                field: "age".to_string(),
                order: crate::query::SortOrder::Desc,
            }),
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
        filter.conditions.push(crate::query::Condition {
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
                conditions: vec![crate::query::Condition {
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
        Condition {
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

    /// Every random filter must find through the index exactly what it
    /// finds by checking every document.
    fn assert_index_agrees_with_scan(docs: &Collection<Document>, rng: &mut XorShift, path: &str) {
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
            assert!(docs.ensure_index("v").unwrap());
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
            sort: Some(crate::query::Sort {
                field: "address.zip".to_string(),
                order: SortOrder::Desc,
            }),
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
            sort: Some(Sort {
                field: "age".to_string(),
                order,
            }),
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
