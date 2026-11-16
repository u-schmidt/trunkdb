//! Writes: applying a batch's ops to data and index pages, keeping
//! secondary and unique indexes right, and building or rekeying them.

use super::Scope;
use super::Upserted;
use super::find::Wants;
use super::plan::follow_hint;
use crate::catalog::{Catalog, CollectionMeta, IndexMeta, IndexOptions};
use crate::data::{self};
use crate::document::{DocId, Document, encode_document, with_id};
use crate::explain::Trace;
use crate::index::{BTreeIndex, Index, KeyRange, key};
use crate::names::FieldNames;
use crate::query::{Filter, Update};
use crate::storage::{PageId, PageStore, RecordLocation};
use crate::txn::WriteOp;
use std::sync::Arc;

impl<'a> Scope<'a> {
    /// The id of the one document of `collection` matching `filter`'s
    /// conditions, `None` if none does; `Error::MultipleMatches` if several
    /// do: what an upsert writes to (SPEC §29.3).
    pub(crate) fn only_match_in(self, filter: &Filter) -> crate::Result<Option<DocId>> {
        follow_hint(self.collection, filter, self.indexes())?;
        let names = filter.fields_with(false);
        let trace = Trace::default();
        let matcher = filter.matcher();
        let candidates = self
            .read_candidates(filter, names.as_deref(), Wants::default(), &trace)?
            .0;
        let mut matches: Vec<DocId> = Vec::new();
        for (source, fields) in candidates {
            if source.decided() || matcher.matches(&fields)? {
                matches.push(source.id());
            }
        }
        match matches[..] {
            [] => Ok(None),
            [id] => Ok(Some(id)),
            _ => Err(crate::Error::MultipleMatches {
                collection: self.collection.to_string(),
                count: matches.len(),
            }),
        }
    }

    /// Where document `id` of `collection` lives — `Error::NotFound` if the
    /// collection or the document doesn't exist.
    fn locate(self, id: &DocId) -> crate::Result<(CollectionMeta, RecordLocation)> {
        let not_found = || crate::Error::NotFound {
            collection: self.collection.to_string(),
            id: *id,
        };
        let meta = *self.meta().ok_or_else(not_found)?;
        let loc = BTreeIndex::new(meta.index_root)
            .lookup(self.store, &key::primary(*id))?
            .ok_or_else(not_found)?;
        Ok((meta, loc))
    }
}

/// `upsert_fields` under the writer's lock (SPEC §144.2, §175): `update`
/// made to the one document of `collection` matching `filter`, or, where
/// none does, to a copy of `seed` inserted as `id`. With whether a
/// document was written: an update the operators leave as it was isn't.
pub(crate) fn upsert_fields_in(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    filter: &Filter,
    (id, seed): (DocId, &Document),
    update: &Update,
    accept: &mut dyn FnMut(&Document) -> crate::Result<()>,
) -> crate::Result<(Upserted, bool)> {
    if let Some(found) = Scope::new(catalog, store, collection).only_match_in(filter)? {
        let changed = update_by_id(catalog, store, collection, found, update, accept)?;
        return Ok((Upserted::Updated(found), changed));
    }
    let mut doc = seed.clone();
    update
        .apply_inserting(&mut doc)
        .map_err(|message| crate::Error::Update {
            collection: collection.to_string(),
            id,
            message,
        })?;
    let doc = with_id(doc, id);
    accept(&doc)?;
    apply_write_op(
        catalog,
        store,
        &WriteOp::Insert(collection.to_string(), id, doc),
    )?;
    Ok((Upserted::Inserted(id), true))
}

/// An `InvalidInput` error saying `message`: an update or filter refused
/// as written, before anything is read.
pub(crate) fn invalid_input(message: String) -> crate::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message).into()
}

/// Makes `update` to document `id` of `collection` and writes it, unless
/// that left it as it was, judged by its encoding (SPEC §68.2): for
/// `upsert_fields` and a batch's `update_fields` (§144). `accept` may
/// refuse the changed document; `Error::NotFound` if there's none.
/// Whether it wrote.
pub(crate) fn update_by_id(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    id: DocId,
    update: &Update,
    accept: &mut dyn FnMut(&Document) -> crate::Result<()>,
) -> crate::Result<bool> {
    let (_, loc) = Scope::new(catalog, store, collection).locate(&id)?;
    let old = with_id(data::get_record(store, catalog.names(), loc)?.1, id);
    let mut new = old.clone();
    update
        .apply(&mut new)
        .map_err(|message| crate::Error::Update {
            collection: collection.to_string(),
            id,
            message,
        })?;
    let new = with_id(new, id);
    accept(&new)?;
    if encode_document(&new) == encode_document(&old) {
        return Ok(false); // nothing to write
    }
    apply_write_op(
        catalog,
        store,
        &WriteOp::Update(collection.to_string(), id, new),
    )?;
    Ok(true)
}

/// The documents `find_in` finds for `filter`, `sort`, `skip` and
/// `limit` included, each mapped by `change` and written where that
/// changed it, judged by its encoding; how many were. For `update_many`
/// and `update_fields`, and a batch's `update_many` (SPEC §168).
pub(crate) fn update_matching_in(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    filter: &Filter,
    change: &mut dyn FnMut(DocId, Document) -> crate::Result<Document>,
) -> crate::Result<usize> {
    // The whole documents, whatever is selected: what `change` returns
    // is written back (SPEC §106.3).
    let found =
        Scope::new(catalog, store, collection).find_in(filter, true, None, &Trace::default())?;
    let mut changed = 0;
    for found in found {
        let (id, old) = (found.id(), found.decode(None)?);
        let new = with_id(change(id, old.clone())?, id);
        // By encoding, not `==`: a NaN isn't equal to itself, and a
        // document holding one would never count as unchanged.
        if encode_document(&new) == encode_document(&old) {
            continue; // nothing to write
        }
        apply_write_op(
            catalog,
            store,
            &WriteOp::Update(collection.to_string(), id, new),
        )?;
        changed += 1;
    }
    Ok(changed)
}

/// Deletes the documents `find_in` finds for `filter`, `sort`, `skip`
/// and `limit` included; how many. For `delete_many`, and a batch's
/// (SPEC §168).
pub(crate) fn delete_matching_in(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    filter: &Filter,
) -> crate::Result<usize> {
    let doomed =
        Scope::new(catalog, store, collection).find_in(filter, false, None, &Trace::default())?;
    for found in &doomed {
        let op = WriteOp::Delete(collection.to_string(), found.id());
        apply_write_op(catalog, store, &op)?;
    }
    Ok(doomed.len())
}

/// Maps a single-op write's outcome to `update`/`delete`'s `bool`.
pub(super) fn found(result: crate::Result<()>) -> crate::Result<bool> {
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
    if let WriteOp::Insert(collection, _, doc) | WriteOp::Update(collection, _, doc) = op {
        // Every document is an object, to carry its id as `_id` (SPEC §95).
        if !matches!(doc, Document::Object(_)) {
            return Err(crate::Error::NotAnObject {
                collection: collection.clone(),
            });
        }
    }
    apply_unchecked(catalog, store, op)?;
    catalog.written(op);
    Ok(())
}

/// `apply_write_op` without its check that the document is an object:
/// for tests that need a file holding one, as before SPEC §95.
pub(super) fn apply_unchecked(
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
            catalog.learn(store, &doc)?;
            let names = catalog.names().clone();
            let mut current = meta.current_data_page;
            let free = catalog.free_space_mut(collection);
            let loc = data::insert_record(store, &mut current, free, *id, &doc, &names)?;
            index.insert(store, &key::primary(*id), loc)?;
            let secondary = catalog.indexes(collection);
            let left_out = update_secondary_indexes(
                collection,
                secondary,
                &names,
                store,
                *id,
                None,
                Some((&doc, loc)),
            )?;
            count_left_out(catalog, store, collection, left_out)?;
            save_current_data_page(catalog, store, collection, &meta, current)
        }
        WriteOp::Update(collection, id, doc) => {
            let (meta, loc) = Scope::new(catalog, store, collection).locate(id)?;
            let mut index = BTreeIndex::new(meta.index_root);
            let doc = with_id(doc.clone(), *id);
            refuse_too_deep(collection, id, &doc)?;
            catalog.learn(store, &doc)?;
            let names = catalog.names().clone();
            let (secondary, free) = catalog.indexes_and_free_space(collection);
            let old = old_document(secondary, &names, store, loc)?;
            let mut current = meta.current_data_page;
            let new_loc = data::update_record(store, &mut current, free, loc, *id, &doc, &names)?;
            if new_loc != loc {
                // The document outgrew its page and moved (SPEC §20.3).
                index.remove(store, &key::primary(*id))?;
                index.insert(store, &key::primary(*id), new_loc)?;
            }
            let old = old.as_ref().map(|old| (old, loc));
            let left_out = update_secondary_indexes(
                collection,
                secondary,
                &names,
                store,
                *id,
                old,
                Some((&doc, new_loc)),
            )?;
            count_left_out(catalog, store, collection, left_out)?;
            save_current_data_page(catalog, store, collection, &meta, current)
        }
        WriteOp::Delete(collection, id) => {
            let (meta, loc) = Scope::new(catalog, store, collection).locate(id)?;
            let mut index = BTreeIndex::new(meta.index_root);
            let names = catalog.names().clone();
            let (secondary, free) = catalog.indexes_and_free_space(collection);
            let old = old_document(secondary, &names, store, loc)?;
            data::delete_record(store, meta.current_data_page, free, loc)?;
            index.remove(store, &key::primary(*id))?;
            let old = old.as_ref().map(|old| (old, loc));
            let left_out =
                update_secondary_indexes(collection, secondary, &names, store, *id, old, None)?;
            count_left_out(catalog, store, collection, left_out)
        }
    }
}

/// The document stored at `loc`, if the collection has secondary indexes
/// to take its old keys out of — otherwise it isn't read at all.
fn old_document(
    indexes: &[IndexMeta],
    names: &Arc<FieldNames>,
    store: &dyn PageStore,
    loc: RecordLocation,
) -> crate::Result<Option<Document>> {
    if indexes.is_empty() {
        return Ok(None);
    }
    Ok(Some(data::get_record(store, names, loc)?.1))
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
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| crate::values::equal(a, b))
}

/// Brings every secondary index from a document's `old` state to its
/// `new` one — each `(document, location)`, `None` for "not there":
/// insert is `(None, new)`, delete `(old, None)`. Only keys that went
/// away are removed and only new ones inserted — all of them, if the
/// document moved. A unique index checks each of the new document's
/// values before anything goes in, but those the old one held already,
/// equal as `Eq` sees them: they can't be a new duplicate. Values, not
/// keys: a key that stays may hold another value, past where a long
/// string is cut (SPEC §119.1).
///
/// Returns each index whose count of documents left out the write
/// changed (SPEC §181), by position, with the change: for
/// `Catalog::add_left_out`, once `indexes` is no longer borrowed. Nearly
/// always none.
fn update_secondary_indexes(
    collection: &str,
    indexes: &[IndexMeta],
    names: &Arc<FieldNames>,
    store: &mut dyn PageStore,
    id: DocId,
    old: Option<(&Document, RecordLocation)>,
    new: Option<(&Document, RecordLocation)>,
) -> crate::Result<Vec<(usize, i64)>> {
    let moved = old.map(|(_, loc)| loc) != new.map(|(_, loc)| loc);
    let mut left_out = Vec::new();
    for (position, index) in indexes.iter().enumerate() {
        let leaves_out = |side: Option<(&Document, RecordLocation)>| {
            side.is_some_and(|(doc, _)| index.leaves_out(doc))
        };
        let change = i64::from(leaves_out(new)) - i64::from(leaves_out(old));
        if change != 0 {
            left_out.push((position, change));
        }
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
            let held = old.map_or(Vec::new(), |(doc, _)| unique_tuples(doc, index));
            for (value_part, values) in unique_tuples(doc, index) {
                let kept = (held.iter())
                    .any(|(part, was)| *part == value_part && same_values(was, &values));
                if !kept {
                    check_unique(names, store, collection, index, id, &value_part, &values)?;
                }
            }
        }
        for key in &after {
            if moved || !before.contains(key) {
                tree.insert(store, key, loc)?;
            }
        }
    }
    Ok(left_out)
}

/// Brings the counts of documents left out up to date after a write
/// (SPEC §181): `changes` from `update_secondary_indexes`.
fn count_left_out(
    catalog: &mut Catalog,
    store: &mut dyn PageStore,
    collection: &str,
    changes: Vec<(usize, i64)>,
) -> crate::Result<()> {
    for (position, change) in changes {
        catalog.add_left_out(store, collection, position, change)?;
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
    names: &Arc<FieldNames>,
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
        let (_, other) = data::get_record(store, names, loc)?;
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
pub(super) fn build_index(
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
    let left_out = fill_index(catalog.names(), store, collection, &index, &documents)?;
    if index.left_out.is_some() {
        let position = catalog.indexes(collection).len() - 1;
        catalog.set_left_out(store, collection, position, left_out)?;
    }
    Ok(true)
}

/// Puts the `documents` (the primary index's entries) into the empty
/// `index`. A unique index checks each document as it goes in, so the
/// first duplicate fails the batch. Returns how many it leaves out (SPEC
/// §181).
fn fill_index(
    names: &Arc<FieldNames>,
    store: &mut dyn PageStore,
    collection: &str,
    index: &IndexMeta,
    documents: &[(Vec<u8>, RecordLocation)],
) -> crate::Result<u64> {
    let mut tree = BTreeIndex::new(index.root);
    let mut left_out = 0;
    for (_key, loc) in documents {
        let (id, doc) = data::get_record(store, names, *loc)?;
        left_out += u64::from(index.leaves_out(&doc));
        if index.unique {
            for (value_part, values) in unique_tuples(&doc, index) {
                check_unique(names, store, collection, index, id, &value_part, &values)?;
            }
        }
        for key in index_keys(&doc, index, id) {
            tree.insert(store, &key, *loc)?;
        }
    }
    Ok(left_out)
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
    let mut names: Vec<&str> = catalog.collection_names().collect();
    names.sort_unstable();
    for name in names {
        let meta = catalog.get(name).expect("a listed collection");
        let mut records = data::Records::new(store, catalog.names());
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

/// Brings a file from before ids had index keys (format 10, SPEC §62),
/// or binary (format 13, SPEC §166), up to date, in `Database::open`'s
/// batch: every secondary index that a document holds such a value in
/// is rebuilt in place. `Database::open` stamps the header after it, so
/// that the check runs once and an older build refuses the file from
/// then on. Only files with secondary indexes: without them nothing
/// needs checking. Rebuilding a unique index can fail, if documents
/// share an id or binary value in it: before, those never collided.
pub(crate) fn rekey_old_indexes(
    catalog: &Catalog,
    store: &mut crate::storage::FileStore,
) -> crate::Result<()> {
    if !store.predates_binary_keys()? {
        return Ok(());
    }
    let ids = store.predates_id_keys()?;
    for name in catalog.collection_names() {
        if catalog.indexes(name).is_empty() {
            continue;
        }
        let (documents, stale) = stale_indexes(catalog, store, name, ids)?;
        for index in stale {
            BTreeIndex::new(index.root).clear(store)?;
            fill_index(catalog.names(), store, name, index, &documents)?;
        }
    }
    Ok(())
}

/// Index entries: each key with the location of its document.
type Entries = Vec<(Vec<u8>, RecordLocation)>;

/// The secondary indexes of `collection` that a document holds binary
/// in, or an id if `ids` (`keyed_since`), and the collection's documents
/// (the primary index's entries), to rebuild them from.
pub(super) fn stale_indexes<'c>(
    catalog: &'c Catalog,
    store: &dyn PageStore,
    collection: &str,
    ids: bool,
) -> crate::Result<(Entries, Vec<&'c IndexMeta>)> {
    let indexes = catalog.indexes(collection);
    let meta = catalog.get(collection).expect("a listed collection");
    let documents = BTreeIndex::new(meta.index_root).scan(store)?;
    let mut stale = vec![false; indexes.len()];
    let mut records = data::Records::new(store, catalog.names());
    for (_key, loc) in &documents {
        let (_id, doc) = records.get(*loc)?;
        for (stale, index) in stale.iter_mut().zip(indexes) {
            *stale |= keyed_since(&doc, index, ids);
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

/// Whether `doc` has binary among the values `index` keys, or an id if
/// `ids`: an entry it lacks, or holds as a value nothing orders, if the
/// index was built before format 13 (SPEC §166), or 10 for ids (§62).
fn keyed_since(doc: &Document, index: &IndexMeta, ids: bool) -> bool {
    let new = |value: &&Document| match value {
        Document::Binary(_) => true,
        Document::Id(_) => ids,
        _ => false,
    };
    match index.single() {
        Some(field) => crate::query::values_at(doc, field).iter().any(new),
        None => field_values(doc, index).iter().any(new),
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
pub(super) fn primary_key_error() -> crate::Error {
    crate::Error::from(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "`_id` is the primary key; it needs no secondary index",
    ))
}
