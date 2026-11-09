use crate::Result;
use crate::changes::ChangeLog;
use crate::decode::{take, take_u8, take_u64};
use crate::free_space::FreeSpace;
use crate::names::FieldNames;
use crate::storage::{PageId, PageStore, PageType, SlottedPage};
use std::collections::HashMap;
use std::sync::Arc;

const CATALOG_PAGE_ID: PageId = 1;
/// Longest collection name, in UTF-8 bytes. A catalog cell has to fit in
/// one page; without a limit, a name too long for even an empty catalog
/// page made `create_collection` chain new pages forever (SPEC §22.3).
pub const MAX_COLLECTION_NAME_LEN: usize = 255;
/// Longest indexed field name, in UTF-8 bytes — for the same reason: an
/// index cell holds the collection name and the field name.
pub const MAX_FIELD_NAME_LEN: usize = 255;

// The first byte of every catalog cell says what it describes.
const KIND_COLLECTION: u8 = 0;
const KIND_INDEX: u8 = 1;
/// Laid out like `KIND_INDEX` (SPEC §33).
const KIND_UNIQUE_INDEX: u8 = 2;
/// Any other index: on several fields (SPEC §43.1), or sparse (§44),
/// with a byte of `FLAG_*` bits.
const KIND_INDEX_WITH_FLAGS: u8 = 3;
const FLAG_UNIQUE: u8 = 1;
const FLAG_SPARSE: u8 = 2;
/// A one-field index that counts the documents it leaves out (SPEC
/// §181, file format 16), laid out so the count is rewritten in place.
const KIND_COUNTED_INDEX: u8 = 5;
/// Field names of the file's dictionary (`names.rs`, file format 14):
/// `[u8 kind = 4][u32 first number]`, then each name as `[u8
/// length][name]`, numbered on from the first.
const KIND_FIELD_NAMES: u8 = 4;
/// Most bytes of names in one cell: a batch adding more writes several.
const NAMES_PER_CELL: usize = 1024;

#[derive(Clone, Copy)]
pub struct CollectionMeta {
    /// Root of the primary (`_id`) index.
    pub index_root: PageId,
    /// The data page this collection's next insert tries first (see
    /// `data::insert_record`); `0` until its first insert.
    pub current_data_page: PageId,
}

/// A secondary index of a collection (SPEC §28): on one field — a
/// dotted path into nested objects (SPEC §31), or into array elements
/// (SPEC §42) — or on several, a compound index (SPEC §43).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexMeta {
    pub fields: Vec<String>,
    pub root: PageId,
    /// No two documents may have equal non-null values in the field
    /// (SPEC §33) — in all of a compound index's fields at once (§43.4).
    pub unique: bool,
    /// Null and missing values get no entry (SPEC §44) — on several
    /// fields, a document null in all of them.
    pub sparse: bool,
    /// How many documents have a value at its field that nothing orders
    /// — an array, an object, NaN — and so no place in its order (SPEC
    /// §181): a walk in its order finds those after it only by a scan,
    /// which a count of 0 spares. Kept for a one-field index without
    /// `[*]` (`counts_left_out`), the only kind a walk in sort order
    /// scans after; `None` for the others, and for one an older file
    /// holds until it's built anew.
    pub left_out: Option<u64>,
}

/// How `Collection::ensure_index_with` builds an index: `new()` is what
/// `ensure_index` builds, and each method adds one option (SPEC §60).
///
/// ```
/// use trunkdb::IndexOptions;
///
/// let options = IndexOptions::new().unique().sparse();
/// assert!(options.is_unique() && options.is_sparse());
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexOptions {
    pub(crate) unique: bool,
    pub(crate) sparse: bool,
}

/// One secondary index, as `Collection::indexes` lists it (SPEC §60): its
/// fields, and the options it was built with.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexInfo {
    fields: Vec<String>,
    options: IndexOptions,
}

impl IndexInfo {
    /// Its fields: one, or several for a compound index (SPEC §43).
    pub fn fields(&self) -> &[String] {
        &self.fields
    }

    /// How lists and errors name it: its field, or a compound index's
    /// fields in parentheses, `(status, created)`.
    pub fn name(&self) -> String {
        match &self.fields[..] {
            [single] => single.clone(),
            fields => format!("({})", fields.join(", ")),
        }
    }

    /// The options it was built with.
    pub fn options(&self) -> IndexOptions {
        self.options.clone()
    }

    /// Whether it's a unique index (SPEC §33).
    pub fn is_unique(&self) -> bool {
        self.options.unique
    }

    /// Whether it's a sparse index (SPEC §44).
    pub fn is_sparse(&self) -> bool {
        self.options.sparse
    }
}

impl IndexOptions {
    /// A plain index: no option set.
    pub const fn new() -> Self {
        IndexOptions {
            unique: false,
            sparse: false,
        }
    }

    /// No two documents may have equal values in it (SPEC §33); nulls
    /// and missing values never count, nor do values nothing orders —
    /// arrays, objects, NaN — which a filter's `=` may still find equal
    /// (SPEC §124.5). Binary counts, by its bytes, since SPEC §166.
    pub const fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Documents with a null or missing value get no entry (SPEC §44):
    /// smaller for a field few documents have, but only used for
    /// queries that rule nulls out.
    pub const fn sparse(mut self) -> Self {
        self.sparse = true;
        self
    }

    /// Whether `unique` is set.
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    /// Whether `sparse` is set.
    pub const fn is_sparse(&self) -> bool {
        self.sparse
    }
}

impl IndexMeta {
    /// How lists and errors name it: its field, or a compound index's
    /// fields in parentheses, `(status, created)`.
    pub fn name(&self) -> String {
        match self.fields.as_slice() {
            [field] => field.clone(),
            fields => format!("({})", fields.join(", ")),
        }
    }

    pub fn is_compound(&self) -> bool {
        self.fields.len() > 1
    }

    /// Whether it keeps `left_out`: on one field, without `[*]` (SPEC
    /// §181).
    pub(crate) fn counts_left_out(&self) -> bool {
        self.single()
            .is_some_and(|field| !crate::query::is_multi(field))
    }

    /// Whether `doc` is one `left_out` counts: its value at the field is
    /// one nothing orders, which a walk in the index's order finds only
    /// by a scan (SPEC §181). `false` for an index that doesn't count.
    pub(crate) fn leaves_out(&self, doc: &crate::Document) -> bool {
        self.counts_left_out()
            && crate::query::is_unordered(crate::query::value_or_null(doc, &self.fields[0]))
    }

    /// The field of a one-field index.
    pub fn single(&self) -> Option<&str> {
        match self.fields.as_slice() {
            [field] => Some(field),
            _ => None,
        }
    }

    /// What `Collection::indexes` hands out for it.
    pub(crate) fn info(&self) -> IndexInfo {
        IndexInfo {
            fields: self.fields.clone(),
            options: self.options(),
        }
    }

    pub fn options(&self) -> IndexOptions {
        IndexOptions {
            unique: self.unique,
            sparse: self.sparse,
        }
    }
}

/// `Clone` so `Database::write_batch` can snapshot it before a batch and
/// restore it on rollback — a batch can create a collection, and the
/// cache must not remember one whose pages were never written.
#[derive(Clone)]
pub struct Catalog {
    collections: HashMap<String, CollectionMeta>,
    /// Secondary indexes by collection name, in creation order. Separate
    /// from `CollectionMeta` so that stays a small `Copy` value.
    indexes: HashMap<String, Vec<IndexMeta>>,
    /// Each collection's data pages with room left (SPEC §75). In memory
    /// only, empty at open; restored with the rest on rollback, so it
    /// never names a page a failed batch freed or never wrote.
    free_space: HashMap<String, FreeSpace>,
    /// What the batch has changed so far, while a subscriber listens
    /// (SPEC §135): the batch's own, like the rest of its catalog, and
    /// taken out before the catalog is published. `None` otherwise.
    changes: Option<ChangeLog>,
    /// The file's dictionary of field names, shared with the snapshots
    /// and reads that have it: a batch adding names copies it first.
    names: Arc<FieldNames>,
}

impl Catalog {
    /// Records from here on what the batch changes (SPEC §135).
    pub(crate) fn record_changes(&mut self) {
        self.changes = Some(ChangeLog::default());
    }

    /// Whether the batch records what it changes.
    #[cfg(test)]
    pub(crate) fn records_changes(&self) -> bool {
        self.changes.is_some()
    }

    /// What the batch changed, if it recorded it; nothing is recorded
    /// after.
    pub(crate) fn take_changes(&mut self) -> Option<ChangeLog> {
        self.changes.take()
    }

    /// `op`, just applied, for the record.
    pub(crate) fn written(&mut self, op: &crate::txn::WriteOp) {
        if let Some(changes) = &mut self.changes {
            changes.written(op);
        }
    }

    /// `collection`, just dropped, for the record.
    pub(crate) fn dropped(&mut self, collection: &str) {
        if let Some(changes) = &mut self.changes {
            changes.dropped(collection);
        }
    }

    /// Reads the catalog page chain (from page 1), decoding its cells into
    /// collections and indexes. If the file is fresh and page 1 doesn't
    /// exist yet, creates and initializes an empty catalog page first,
    /// then returns an empty `Catalog` — this is why `store` needs to be
    /// `&mut`, not `&`.
    pub fn load(store: &mut dyn PageStore) -> std::io::Result<Self> {
        let mut collections = HashMap::new();
        let mut name_cells = std::collections::BTreeMap::new();
        let mut indexes: HashMap<String, Vec<IndexMeta>> = HashMap::new();
        let mut page_id = CATALOG_PAGE_ID;
        // Catalog pages link to the next; one seen twice is a loop, which
        // would otherwise be followed forever (SPEC §55).
        let mut seen = std::collections::HashSet::new();

        loop {
            if !seen.insert(page_id) {
                return Err(corrupt(format!(
                    "catalog page {page_id} comes twice in the chain: it goes round in a loop"
                )));
            }
            let bytes = match store.try_read_page(page_id)? {
                Some(bytes) => bytes,
                None if page_id == CATALOG_PAGE_ID => {
                    // Fresh file — bootstrap the first catalog page, then
                    // fall through to the same read/decode path below (it's
                    // empty, so that's a harmless no-op iteration).
                    let id = store.allocate_page()?;
                    assert_eq!(
                        id, CATALOG_PAGE_ID,
                        "catalog page must be the first page ever allocated in the file"
                    );
                    let bytes = SlottedPage::new(PageType::Catalog).into_page();
                    store.write_page(id, bytes.clone())?;
                    bytes
                }
                None => {
                    // A previous page's next_page pointed here, but it
                    // doesn't exist — this is corruption, not a fresh file.
                    return Err(corrupt(format!(
                        "catalog chain references page {page_id}, which doesn't exist"
                    )));
                }
            };

            let page = SlottedPage::from_bytes(bytes)?;
            if page.page_type() != PageType::Catalog {
                return Err(corrupt(format!(
                    "page {page_id} was expected to be a catalog page"
                )));
            }

            for (_slot, cell) in page.iter_cells() {
                match decode_entry(cell)? {
                    Entry::Collection(name, meta) => {
                        collections.insert(name, meta);
                    }
                    Entry::Index(collection, index) => {
                        indexes.entry(collection).or_default().push(index);
                    }
                    Entry::Names(first, list) => {
                        if name_cells.insert(first, list).is_some() {
                            return Err(corrupt(format!(
                                "two catalog cells number field names from {first}"
                            )));
                        }
                    }
                }
            }

            let next = page.next_page();
            if next == 0 {
                break;
            }
            page_id = next;
        }

        if let Some(orphan) = indexes.keys().find(|c| !collections.contains_key(*c)) {
            return Err(corrupt(format!(
                "an index belongs to collection {orphan:?}, which doesn't exist"
            )));
        }
        let mut names = FieldNames::default();
        for (first, list) in name_cells {
            if first as usize != names.len() {
                return Err(corrupt(format!(
                    "field names numbered from {first} follow {} names",
                    names.len()
                )));
            }
            for name in list {
                if !names.add(&name) {
                    return Err(corrupt(format!(
                        "field name {name:?} can't be in the dictionary, or comes twice"
                    )));
                }
            }
        }
        Ok(Catalog {
            collections,
            indexes,
            free_space: HashMap::new(),
            changes: None,
            names: Arc::new(names),
        })
    }

    /// The file's dictionary of field names.
    pub fn names(&self) -> &Arc<FieldNames> {
        &self.names
    }

    /// Adds the names of `doc` the dictionary doesn't have yet and can
    /// take, and writes them to the catalog's pages: before `doc` is
    /// stored, so it is stored with them. Nothing to write, nearly
    /// always.
    pub fn learn(
        &mut self,
        store: &mut dyn PageStore,
        doc: &crate::Document,
    ) -> std::io::Result<()> {
        let unknown = self.names.unknown(doc);
        if unknown.is_empty() {
            return Ok(());
        }
        let first = self.names.len() as u32;
        let names = Arc::make_mut(&mut self.names);
        for name in &unknown {
            names.add(name);
        }
        for cell in encode_names(first, &unknown) {
            append_cell(store, &cell)?;
        }
        Ok(())
    }

    /// How many pages the catalog's chain has: for tests.
    #[cfg(test)]
    fn pages_used(&self, store: &dyn PageStore) -> usize {
        Catalog::pages(store).unwrap().len()
    }

    /// The catalog's own pages: the chain from page 1.
    pub fn pages(store: &dyn PageStore) -> std::io::Result<Vec<PageId>> {
        let mut pages = Vec::new();
        let mut page_id = CATALOG_PAGE_ID;
        while page_id != 0 {
            if pages.contains(&page_id) {
                return Err(corrupt(format!(
                    "the catalog chain loops at page {page_id}"
                )));
            }
            pages.push(page_id);
            page_id = SlottedPage::from_bytes(store.read_page(page_id)?)?.next_page();
        }
        Ok(pages)
    }

    pub fn get(&self, name: &str) -> Option<&CollectionMeta> {
        self.collections.get(name)
    }

    /// Every collection's name, in no particular order.
    pub fn collection_names(&self) -> impl Iterator<Item = &str> {
        self.collections.keys().map(String::as_str)
    }

    /// The collection's secondary indexes — empty if it has none, or
    /// doesn't exist.
    pub fn indexes(&self, collection: &str) -> &[IndexMeta] {
        self.indexes.get(collection).map_or(&[], Vec::as_slice)
    }

    /// How many pages the catalog `compact` writes would take (SPEC
    /// §93.4): this one's cells, in the order `compact` creates them —
    /// each collection by name, then its indexes — each on the first page
    /// with room, as `append_cell` puts them.
    pub(crate) fn rebuilt_pages(&self) -> u64 {
        let mut names: Vec<&str> = self.collection_names().collect();
        names.sort();
        let empty = SlottedPage::new(PageType::Catalog).free_space();
        let mut rooms = vec![empty];
        let all: Vec<String> = self.names.iter().map(str::to_string).collect();
        for cell in encode_names(0, &all) {
            let needed = SlottedPage::space_needed(cell.len());
            match rooms.iter_mut().find(|room| **room >= needed) {
                Some(room) => *room -= needed,
                None => rooms.push(empty - needed),
            }
        }
        for name in names {
            let meta = self.collections[name];
            let cells = std::iter::once(encode_collection(name, &meta).len()).chain(
                self.indexes(name).iter().map(|index| {
                    // Compaction counts what each index leaves out
                    // (SPEC §181), an older file's too: its cell's size.
                    let counted = IndexMeta {
                        left_out: index.counts_left_out().then_some(0),
                        ..index.clone()
                    };
                    encode_index(name, &counted).len()
                }),
            );
            for cell in cells {
                let needed = SlottedPage::space_needed(cell);
                match rooms.iter_mut().find(|room| **room >= needed) {
                    Some(room) => *room -= needed,
                    None => rooms.push(empty - needed),
                }
            }
        }
        rooms.len() as u64
    }

    pub fn create_collection(
        &mut self,
        store: &mut dyn PageStore,
        name: &str,
    ) -> Result<CollectionMeta> {
        check_len("collection name", name, MAX_COLLECTION_NAME_LEN)?;
        let meta = CollectionMeta {
            index_root: allocate_index_root(store)?,
            current_data_page: 0,
        };
        append_cell(store, &encode_collection(name, &meta))?;
        self.collections.insert(name.to_string(), meta);
        Ok(meta)
    }

    /// Records a new, empty secondary index on `fields` — building it
    /// from the collection's documents is the caller's job. `collection`
    /// must exist and not have an index on `fields` yet.
    pub fn create_index(
        &mut self,
        store: &mut dyn PageStore,
        collection: &str,
        fields: &[String],
        options: IndexOptions,
    ) -> Result<IndexMeta> {
        for field in fields {
            check_len("field name", field, MAX_FIELD_NAME_LEN)?;
        }
        assert!(self.collections.contains_key(collection));
        assert!(!self.indexes(collection).iter().any(|i| i.fields == fields));
        let mut index = IndexMeta {
            fields: fields.to_vec(),
            root: allocate_index_root(store)?,
            unique: options.unique,
            sparse: options.sparse,
            left_out: None,
        };
        // Empty, it leaves nothing out; filling it counts (SPEC §181).
        if index.counts_left_out() {
            index.left_out = Some(0);
        }
        append_cell(store, &encode_index(collection, &index))?;
        self.indexes
            .entry(collection.to_string())
            .or_default()
            .push(index.clone());
        Ok(index)
    }

    /// Removes the index on `fields` from the catalog and returns it, so
    /// the caller can free its pages; `None` if there's no such index.
    pub fn drop_index(
        &mut self,
        store: &mut dyn PageStore,
        collection: &str,
        fields: &[String],
    ) -> Result<Option<IndexMeta>> {
        let Some(list) = self.indexes.get_mut(collection) else {
            return Ok(None);
        };
        let Some(pos) = list.iter().position(|i| i.fields == fields) else {
            return Ok(None);
        };
        let index = list.remove(pos);
        let cell = encode_index(collection, &index);
        let (page_id, mut page, slot) =
            find_cell(store, |c| c == cell.as_slice())?.ok_or_else(|| {
                corrupt(format!(
                    "index {collection}.{} has no catalog entry",
                    index.name()
                ))
            })?;
        page.delete_cell(slot);
        store.write_page(page_id, page.into_page())?;
        Ok(Some(index))
    }

    /// Removes a collection and its indexes from the catalog and returns
    /// them, so the caller can free their pages (SPEC §37); `None` if
    /// there's no such collection.
    pub fn drop_collection(
        &mut self,
        store: &mut dyn PageStore,
        name: &str,
    ) -> Result<Option<(CollectionMeta, Vec<IndexMeta>)>> {
        let Some(meta) = self.collections.get(name).copied() else {
            return Ok(None);
        };
        let indexes = self.indexes(name).to_vec();
        for index in &indexes {
            self.drop_index(store, name, &index.fields)?;
        }
        let cell = encode_collection(name, &meta);
        let (page_id, mut page, slot) = find_cell(store, |c| c == cell.as_slice())?
            .ok_or_else(|| corrupt(format!("collection {name:?} has no catalog entry")))?;
        page.delete_cell(slot);
        store.write_page(page_id, page.into_page())?;
        self.collections.remove(name);
        self.indexes.remove(name);
        self.free_space.remove(name);
        Ok(Some((meta, indexes)))
    }

    /// `collection`'s data pages with room left — an empty map for a
    /// collection nothing has been learned about yet (SPEC §75).
    pub fn free_space_mut(&mut self, collection: &str) -> &mut FreeSpace {
        self.free_space.entry(collection.to_string()).or_default()
    }

    /// `collection`'s map, if anything has been learned about it — for
    /// tests.
    #[cfg(test)]
    pub fn free_space(&self, collection: &str) -> Option<&FreeSpace> {
        self.free_space.get(collection)
    }

    /// `indexes` and `free_space_mut` at once: a write reads the one while
    /// it changes the other, which two calls on `self` can't lend out
    /// together — one method borrowing two separate fields can.
    pub fn indexes_and_free_space(&mut self, collection: &str) -> (&[IndexMeta], &mut FreeSpace) {
        let indexes = self.indexes.get(collection).map_or(&[][..], Vec::as_slice);
        let free = self.free_space.entry(collection.to_string()).or_default();
        (indexes, free)
    }

    /// Records how many documents `collection`'s index at `position`
    /// leaves out (SPEC §181) — in its catalog cell and in the cache. A
    /// cell with a count keeps its length, so it's rewritten in place;
    /// an older file's, without one, is replaced by a cell of kind 5.
    /// Nothing is written if the count is already `left_out`.
    pub(crate) fn set_left_out(
        &mut self,
        store: &mut dyn PageStore,
        collection: &str,
        position: usize,
        left_out: u64,
    ) -> Result<()> {
        let index = &mut self
            .indexes
            .get_mut(collection)
            .expect("set_left_out on a collection with indexes")[position];
        assert!(
            index.counts_left_out(),
            "set_left_out on an index that doesn't count"
        );
        if index.left_out == Some(left_out) {
            return Ok(());
        }
        let cell = encode_index(collection, index);
        index.left_out = Some(left_out);
        let entry_bytes = encode_index(collection, index);
        let (page_id, mut page, slot) =
            find_cell(store, |c| c == cell.as_slice())?.ok_or_else(|| {
                corrupt(format!(
                    "index {collection}.{} has no catalog entry",
                    index.name()
                ))
            })?;
        if !page.update_cell(slot, &entry_bytes) {
            // An older file's cell, shorter: counted for the first time.
            page.delete_cell(slot);
            store.write_page(page_id, page.into_page())?;
            return Ok(append_cell(store, &entry_bytes)?);
        }
        store.write_page(page_id, page.into_page())?;
        Ok(())
    }

    /// Makes `collection`'s index at `position` one an older file holds,
    /// without a count (SPEC §181.4): for tests.
    #[cfg(test)]
    pub(crate) fn forget_left_out(
        &mut self,
        store: &mut dyn PageStore,
        collection: &str,
        position: usize,
    ) -> Result<()> {
        let index = &mut self.indexes.get_mut(collection).unwrap()[position];
        let cell = encode_index(collection, index);
        index.left_out = None;
        let (page_id, mut page, slot) = find_cell(store, |c| c == cell.as_slice())?.unwrap();
        page.delete_cell(slot);
        store.write_page(page_id, page.into_page())?;
        Ok(append_cell(store, &encode_index(collection, index))?)
    }

    /// Adds `change` to how many documents `collection`'s index at
    /// `position` leaves out (SPEC §181), for a write that changed it.
    /// An index that doesn't know its count yet stays so.
    pub(crate) fn add_left_out(
        &mut self,
        store: &mut dyn PageStore,
        collection: &str,
        position: usize,
        change: i64,
    ) -> Result<()> {
        let index = &self.indexes(collection)[position];
        let Some(count) = index.left_out else {
            return Ok(());
        };
        let count = count.checked_add_signed(change).ok_or_else(|| {
            corrupt(format!(
                "index {collection}.{} counts {count} documents left out, not {}",
                index.name(),
                -change
            ))
        })?;
        self.set_left_out(store, collection, position, count)
    }

    /// Records a collection's new current data page — in its catalog
    /// cell and in the cache. Only called when an insert had to allocate
    /// a new data page, not on every insert. The cell keeps its length
    /// (only a fixed-width field changes), so it's rewritten in place.
    pub fn set_current_data_page(
        &mut self,
        store: &mut dyn PageStore,
        name: &str,
        page: PageId,
    ) -> Result<()> {
        let meta = self
            .collections
            .get_mut(name)
            .expect("set_current_data_page on a collection that doesn't exist");
        meta.current_data_page = page;
        let entry_bytes = encode_collection(name, meta);

        let is_this_collection = |c: &[u8]| {
            c[0] == KIND_COLLECTION && c.get(COLLECTION_NAME_AT..) == Some(name.as_bytes())
        };
        let (page_id, mut page, slot) = find_cell(store, is_this_collection)?.ok_or_else(|| {
            corrupt(format!(
                "collection {name:?} is cached but has no catalog entry"
            ))
        })?;
        assert!(
            page.update_cell(slot, &entry_bytes),
            "same length, fits in place"
        );
        store.write_page(page_id, page.into_page())?;
        Ok(())
    }
}

/// An index's root starts out as an empty leaf, written right away — the
/// index's first read would otherwise misread whatever the page held
/// (SPEC §10.7).
fn allocate_index_root(store: &mut dyn PageStore) -> std::io::Result<PageId> {
    let root = store.allocate_page()?;
    store.write_page(
        root,
        SlottedPage::new(PageType::IndexLeafPacked).into_page(),
    )?;
    Ok(root)
}

/// Adds a cell to the first catalog page with room for it, chaining a new
/// page onto the end if none has.
fn append_cell(store: &mut dyn PageStore, cell: &[u8]) -> std::io::Result<()> {
    let mut page_id = CATALOG_PAGE_ID;
    let mut page = SlottedPage::from_bytes(store.read_page(page_id)?)?;

    while !page.has_room_for(cell.len()) {
        let next_page_id = page.next_page();
        if next_page_id != 0 {
            page_id = next_page_id;
            page = SlottedPage::from_bytes(store.read_page(page_id)?)?;
            continue;
        }

        let new_page_id = store.allocate_page()?;
        page.set_next_page(new_page_id);
        store.write_page(page_id, page.into_page())?;

        page = SlottedPage::new(PageType::Catalog);
        page_id = new_page_id;
    }

    page.insert_cell(cell)
        .expect("just verified has_room_for(cell.len())");
    store.write_page(page_id, page.into_page())
}

/// The first catalog cell `matches` accepts: its page's id, the page,
/// and its slot.
fn find_cell(
    store: &dyn PageStore,
    matches: impl Fn(&[u8]) -> bool,
) -> std::io::Result<Option<(PageId, SlottedPage, u16)>> {
    let mut page_id = CATALOG_PAGE_ID;
    while page_id != 0 {
        let page = SlottedPage::from_bytes(store.read_page(page_id)?)?;
        let found = page
            .iter_cells()
            .find(|(_slot, cell)| matches(cell))
            .map(|(slot, _cell)| slot);
        if let Some(slot) = found {
            return Ok(Some((page_id, page, slot)));
        }
        page_id = page.next_page();
    }
    Ok(None)
}

fn check_len(what: &str, name: &str, max: usize) -> Result<()> {
    if name.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{what} is {} bytes, longer than the limit of {max}",
                name.len()
            ),
        )
        .into());
    }
    Ok(())
}

enum Entry {
    Collection(String, CollectionMeta),
    Index(String, IndexMeta),
    /// Field names, numbered from the first.
    Names(u32, Vec<String>),
}

/// `names`, numbered from `first`, in cells of `KIND_FIELD_NAMES`.
fn encode_names(first: u32, names: &[String]) -> Vec<Vec<u8>> {
    let mut cells = Vec::new();
    let mut cell = Vec::new();
    for (number, name) in (first..).zip(names) {
        if cell.is_empty() {
            cell.push(KIND_FIELD_NAMES);
            cell.extend_from_slice(&number.to_le_bytes());
        }
        cell.push(name.len() as u8);
        cell.extend_from_slice(name.as_bytes());
        if cell.len() >= NAMES_PER_CELL {
            cells.push(std::mem::take(&mut cell));
        }
    }
    if !cell.is_empty() {
        cells.push(cell);
    }
    cells
}

/// Where a collection cell's name starts.
const COLLECTION_NAME_AT: usize = 1 + 8 + 8;

/// A collection cell: `[u8 kind = 0][u64 index_root][u64
/// current_data_page][name]` — the name last and unprefixed, since the
/// slot directory already records the cell's length.
fn encode_collection(name: &str, meta: &CollectionMeta) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(COLLECTION_NAME_AT + name.len());
    buffer.push(KIND_COLLECTION);
    buffer.extend_from_slice(&meta.index_root.to_le_bytes());
    buffer.extend_from_slice(&meta.current_data_page.to_le_bytes());
    buffer.extend_from_slice(name.as_bytes());
    buffer
}

/// An index cell: `[u8 kind][u64 root][u8 collection name length]
/// [collection name]`, then for a plain one-field index (kind 1, or 2 if
/// unique) the field name. Any other (kind 3: compound, SPEC §43.1, or
/// sparse, §44) has `[u8 flags][u8 count]` and each field as `[u8
/// length][field name]`. Names are at most 255 bytes, so one length
/// byte does. An index that counts what it leaves out (kind 5, SPEC
/// §181) has `[u64 left_out]` after the collection name, then what kind
/// 3 has: the count at the same place whatever it is.
fn encode_index(collection: &str, index: &IndexMeta) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(20 + collection.len() + 256 * index.fields.len());
    let plain = !index.is_compound() && !index.sparse;
    buffer.push(match (index.left_out, plain, index.unique) {
        (Some(_), ..) => KIND_COUNTED_INDEX,
        (None, true, false) => KIND_INDEX,
        (None, true, true) => KIND_UNIQUE_INDEX,
        (None, false, _) => KIND_INDEX_WITH_FLAGS,
    });
    buffer.extend_from_slice(&index.root.to_le_bytes());
    buffer.push(collection.len() as u8);
    buffer.extend_from_slice(collection.as_bytes());
    if let Some(left_out) = index.left_out {
        buffer.extend_from_slice(&left_out.to_le_bytes());
    } else if plain {
        buffer.extend_from_slice(index.fields[0].as_bytes());
        return buffer;
    }
    let flag = |set: bool, flag: u8| if set { flag } else { 0 };
    buffer.push(flag(index.unique, FLAG_UNIQUE) | flag(index.sparse, FLAG_SPARSE));
    buffer.push(index.fields.len() as u8);
    for field in &index.fields {
        buffer.push(field.len() as u8);
        buffer.extend_from_slice(field.as_bytes());
    }
    buffer
}

/// A cell too short for its kind is damage, not a panic (SPEC §55).
fn decode_entry(cell: &[u8]) -> std::io::Result<Entry> {
    let utf8 = |bytes: &[u8]| {
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("bad utf8 in catalog".into()))
    };
    let mut rest = cell;
    let kind = take_u8(&mut rest, "a catalog entry's kind")?;
    if kind == KIND_FIELD_NAMES {
        let first = crate::decode::take_u32(&mut rest, "field names' first number")?;
        let mut names = Vec::new();
        while !rest.is_empty() {
            let len = take_u8(&mut rest, "a field name")? as usize;
            names.push(utf8(take(&mut rest, len, "a field name")?)?);
        }
        return Ok(Entry::Names(first, names));
    }
    let root = take_u64(&mut rest, "a catalog entry's root")?;
    match kind {
        KIND_COLLECTION => {
            let current_data_page = take_u64(&mut rest, "a collection's current data page")?;
            Ok(Entry::Collection(
                utf8(rest)?,
                CollectionMeta {
                    index_root: root,
                    current_data_page,
                },
            ))
        }
        kind @ (KIND_INDEX | KIND_UNIQUE_INDEX) => {
            let name_len = take_u8(&mut rest, "an index's collection name")? as usize;
            let collection = take(&mut rest, name_len, "an index's collection name")?;
            Ok(Entry::Index(
                utf8(collection)?,
                IndexMeta {
                    fields: vec![utf8(rest)?],
                    root,
                    unique: kind == KIND_UNIQUE_INDEX,
                    sparse: false,
                    left_out: None,
                },
            ))
        }
        kind @ (KIND_INDEX_WITH_FLAGS | KIND_COUNTED_INDEX) => {
            let name_len = take_u8(&mut rest, "an index's collection name")? as usize;
            let collection = take(&mut rest, name_len, "an index's collection name")?;
            let left_out = match kind {
                KIND_COUNTED_INDEX => Some(take_u64(&mut rest, "an index's count left out")?),
                _ => None,
            };
            let flags = take_u8(&mut rest, "an index's flags")?;
            if flags & !(FLAG_UNIQUE | FLAG_SPARSE) != 0 {
                return Err(corrupt(format!("unknown index flags {flags:#04x}")));
            }
            let count = take_u8(&mut rest, "an index's field count")?;
            let mut fields = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let len = take_u8(&mut rest, "an index's field")? as usize;
                fields.push(utf8(take(&mut rest, len, "an index's field")?)?);
            }
            let index = IndexMeta {
                fields,
                root,
                unique: flags & FLAG_UNIQUE != 0,
                sparse: flags & FLAG_SPARSE != 0,
                left_out,
            };
            if index.left_out.is_some() && !index.counts_left_out() {
                return Err(corrupt(format!(
                    "index {} counts documents left out, which only an index on one field does",
                    index.name()
                )));
            }
            Ok(Entry::Index(utf8(collection)?, index))
        }
        other => Err(corrupt(format!("unknown catalog entry kind {other}"))),
    }
}

fn corrupt(what: String) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{what} — file may be corrupt"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FileStore;

    #[test]
    fn fresh_file_gets_an_empty_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();

        let catalog = Catalog::load(&mut store).unwrap();

        assert!(catalog.get("users").is_none());
    }

    #[test]
    fn catalog_survives_reopen_once_populated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");

        {
            let mut store = FileStore::open(&path).unwrap();
            let _catalog = Catalog::load(&mut store).unwrap();
            // Catalog page now exists on disk, empty, but present.
        }

        let mut store = FileStore::open(&path).unwrap();
        let catalog = Catalog::load(&mut store).unwrap();
        assert!(catalog.get("users").is_none());
    }

    #[test]
    fn created_collection_is_visible_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();

        let meta = catalog.create_collection(&mut store, "users").unwrap();

        let found = catalog.get("users").expect("just created");
        assert_eq!(found.index_root, meta.index_root);
    }

    #[test]
    fn created_collection_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");

        let index_root = {
            let mut store = FileStore::open(&path).unwrap();
            let mut catalog = Catalog::load(&mut store).unwrap();
            catalog
                .create_collection(&mut store, "users")
                .unwrap()
                .index_root
        };

        let mut store = FileStore::open(&path).unwrap();
        let catalog = Catalog::load(&mut store).unwrap();
        let found = catalog.get("users").expect("should survive reopen");
        assert_eq!(found.index_root, index_root);
    }

    #[test]
    fn overlong_collection_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();

        let longest = "x".repeat(MAX_COLLECTION_NAME_LEN);
        catalog.create_collection(&mut store, &longest).unwrap();

        let too_long = "x".repeat(MAX_COLLECTION_NAME_LEN + 1);
        let Err(crate::Error::Io(err)) = catalog.create_collection(&mut store, &too_long) else {
            panic!("an overlong name must be an I/O InvalidInput error");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(catalog.get(&too_long).is_none());
    }

    #[test]
    fn current_data_page_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");

        {
            let mut store = FileStore::open(&path).unwrap();
            let mut catalog = Catalog::load(&mut store).unwrap();
            catalog.create_collection(&mut store, "users").unwrap();
            catalog.create_collection(&mut store, "posts").unwrap();
            catalog
                .set_current_data_page(&mut store, "users", 42)
                .unwrap();
            assert_eq!(catalog.get("users").unwrap().current_data_page, 42);
        }

        let mut store = FileStore::open(&path).unwrap();
        let catalog = Catalog::load(&mut store).unwrap();
        assert_eq!(catalog.get("users").unwrap().current_data_page, 42);
        assert_eq!(catalog.get("posts").unwrap().current_data_page, 0);
    }

    fn fields(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    const PLAIN: IndexOptions = IndexOptions {
        unique: false,
        sparse: false,
    };
    const UNIQUE: IndexOptions = IndexOptions {
        unique: true,
        sparse: false,
    };
    const SPARSE: IndexOptions = IndexOptions {
        unique: false,
        sparse: true,
    };
    const BOTH: IndexOptions = IndexOptions {
        unique: true,
        sparse: true,
    };

    #[test]
    fn indexes_are_created_dropped_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");

        let (email, age, tenant_email, nick, phone) = {
            let mut store = FileStore::open(&path).unwrap();
            let mut catalog = Catalog::load(&mut store).unwrap();
            catalog.create_collection(&mut store, "users").unwrap();
            catalog.create_collection(&mut store, "posts").unwrap();
            let email = catalog
                .create_index(&mut store, "users", &fields(&["email"]), UNIQUE)
                .unwrap();
            let age = catalog
                .create_index(&mut store, "users", &fields(&["age"]), PLAIN)
                .unwrap();
            let tenant_email = catalog
                .create_index(&mut store, "users", &fields(&["tenant", "email"]), UNIQUE)
                .unwrap();
            let nick = catalog
                .create_index(&mut store, "users", &fields(&["nick"]), SPARSE)
                .unwrap();
            let phone = catalog
                .create_index(&mut store, "users", &fields(&["phone"]), BOTH)
                .unwrap();
            // Unique, so dropping them has to find kind-2 and kind-3 cells.
            catalog
                .create_index(&mut store, "posts", &fields(&["title"]), UNIQUE)
                .unwrap();
            catalog
                .create_index(&mut store, "posts", &fields(&["a", "b", "c"]), UNIQUE)
                .unwrap();
            assert_eq!(
                catalog.indexes("users"),
                [
                    email.clone(),
                    age.clone(),
                    tenant_email.clone(),
                    nick.clone(),
                    phone.clone()
                ]
            );
            assert!(email.unique && !age.unique && tenant_email.unique);
            assert_eq!(nick.options(), SPARSE);
            assert_eq!(phone.options(), BOTH);
            assert!(!email.sparse && !tenant_email.sparse);
            assert_eq!(tenant_email.name(), "(tenant, email)");

            for dropped in [&["title"][..], &["a", "b", "c"]] {
                let index = catalog.drop_index(&mut store, "posts", &fields(dropped));
                assert_eq!(index.unwrap().map(|i| i.fields), Some(fields(dropped)));
                let again = catalog.drop_index(&mut store, "posts", &fields(dropped));
                assert_eq!(again.unwrap(), None);
            }
            // Still findable after an index cell came before it.
            catalog
                .set_current_data_page(&mut store, "posts", 7)
                .unwrap();
            (email, age, tenant_email, nick, phone)
        };

        let mut store = FileStore::open(&path).unwrap();
        let catalog = Catalog::load(&mut store).unwrap();
        assert_eq!(
            catalog.indexes("users"),
            [email, age, tenant_email, nick, phone]
        );
        assert_eq!(catalog.indexes("posts"), []);
        assert_eq!(catalog.indexes("nothing"), []);
        assert_eq!(catalog.get("posts").unwrap().current_data_page, 7);
    }

    /// A plain one-field index keeps its old cell (kind 1 or 2); any
    /// other gets kind 3 with flags, and flags this version doesn't know
    /// are corruption, not ignored.
    #[test]
    fn index_cells_carry_their_options_and_refuse_unknown_flags() {
        for (fields, options, kind) in [
            (fields(&["a"]), PLAIN, KIND_INDEX),
            (fields(&["a"]), UNIQUE, KIND_UNIQUE_INDEX),
            (fields(&["a"]), SPARSE, KIND_INDEX_WITH_FLAGS),
            (fields(&["a", "b"]), PLAIN, KIND_INDEX_WITH_FLAGS),
            (fields(&["a", "b"]), BOTH, KIND_INDEX_WITH_FLAGS),
        ] {
            let index = IndexMeta {
                fields,
                root: 9,
                unique: options.unique,
                sparse: options.sparse,
                left_out: None,
            };
            let cell = encode_index("users", &index);
            assert_eq!(cell[0], kind);
            let Entry::Index(collection, decoded) = decode_entry(&cell).unwrap() else {
                panic!("an index cell decodes as an index");
            };
            assert_eq!((collection.as_str(), decoded), ("users", index));
        }
        let index = IndexMeta {
            fields: fields(&["a"]),
            root: 9,
            unique: false,
            sparse: true,
            left_out: None,
        };
        let mut cell = encode_index("users", &index);
        cell[10 + "users".len()] |= 4;
        let Err(err) = decode_entry(&cell) else {
            panic!("unknown flags must be refused");
        };
        assert!(
            err.to_string().contains("unknown index flags 0x06"),
            "{err}"
        );
    }

    /// A one-field index's count of documents left out (SPEC §181) is
    /// read back with its options, rewritten in place, and refused on an
    /// index that can't keep one.
    #[test]
    fn index_cells_carry_their_count_left_out() {
        for options in [PLAIN, UNIQUE, SPARSE, BOTH] {
            let index = IndexMeta {
                fields: fields(&["a.b"]),
                root: 9,
                unique: options.unique,
                sparse: options.sparse,
                left_out: Some(u64::MAX - 1),
            };
            let cell = encode_index("users", &index);
            assert_eq!(cell[0], KIND_COUNTED_INDEX);
            let Entry::Index(_, decoded) = decode_entry(&cell).unwrap() else {
                panic!("an index cell decodes as an index");
            };
            assert_eq!(decoded, index);
            let other = encode_index(
                "users",
                &IndexMeta {
                    left_out: Some(0),
                    ..index
                },
            );
            assert_eq!(other.len(), cell.len(), "rewritten in place");
        }
        for list in [&["a", "b"][..], &["a[*]"]] {
            let index = IndexMeta {
                fields: fields(list),
                root: 9,
                unique: false,
                sparse: false,
                left_out: Some(0),
            };
            let Err(err) = decode_entry(&encode_index("users", &index)) else {
                panic!("{list:?} can't count");
            };
            assert!(
                err.to_string().contains("only an index on one field"),
                "{err}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut store = FileStore::open(&path).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();
        catalog.create_collection(&mut store, "users").unwrap();
        for list in [&["a"][..], &["a", "b"], &["c[*]"]] {
            catalog
                .create_index(&mut store, "users", &fields(list), PLAIN)
                .unwrap();
        }
        let counts = |catalog: &Catalog| -> Vec<Option<u64>> {
            catalog
                .indexes("users")
                .iter()
                .map(|i| i.left_out)
                .collect()
        };
        assert_eq!(counts(&catalog), [Some(0), None, None]);
        catalog.add_left_out(&mut store, "users", 0, 3).unwrap();
        catalog.add_left_out(&mut store, "users", 0, -1).unwrap();
        catalog.add_left_out(&mut store, "users", 1, 1).unwrap();
        assert_eq!(counts(&catalog), [Some(2), None, None]);
        assert!(catalog.add_left_out(&mut store, "users", 0, -3).is_err());
        drop(store);
        let mut store = FileStore::open(&path).unwrap();
        assert_eq!(
            counts(&Catalog::load(&mut store).unwrap()),
            [Some(2), None, None]
        );
    }

    /// An older file's cell, without a count, that can't grow where it
    /// is, on a full catalog page, is replaced by one appended where
    /// there's room as it gets its first count (SPEC §181.2).
    #[test]
    fn a_first_count_on_a_full_page_moves_the_cell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let mut store = FileStore::open(&path).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();
        catalog.create_collection(&mut store, "c").unwrap();
        // An older file's cell, kind 1, written as it was: nothing
        // deleted before it that compacting the page could reclaim.
        let old = IndexMeta {
            fields: fields(&["a"]),
            root: allocate_index_root(&mut store).unwrap(),
            unique: false,
            sparse: false,
            left_out: None,
        };
        append_cell(&mut store, &encode_index("c", &old)).unwrap();
        catalog.indexes.insert("c".to_string(), vec![old]);
        let free = |store: &FileStore| {
            SlottedPage::from_bytes(store.read_page(CATALOG_PAGE_ID).unwrap())
                .unwrap()
                .free_space()
        };
        // Collections until page 1 has fewer free bytes than the cell
        // grows by: the count's 8, and the flags, the field count and
        // the field's length, a byte each.
        let grows = 11;
        for n in 0.. {
            let room = free(&store);
            if room < grows {
                break;
            }
            // A name filling what's left exactly, or a long one first.
            let needed = SlottedPage::space_needed(COLLECTION_NAME_AT + 1);
            let more = room.saturating_sub(needed);
            let len = 1 + if more < 200 { more } else { 100 };
            let name = format!("{n:0>len$}");
            catalog.create_collection(&mut store, &name).unwrap();
            if catalog.pages_used(&store) > 1 {
                break;
            }
        }
        assert!(free(&store) < grows, "page 1 full: {}", free(&store));
        catalog.set_left_out(&mut store, "c", 0, 4).unwrap();
        drop(store);
        let mut store = FileStore::open(&path).unwrap();
        let loaded = Catalog::load(&mut store).unwrap();
        assert_eq!(loaded.indexes("c")[0].left_out, Some(4));
    }

    /// The field names' cells must number the names from 0, without a
    /// gap or an overlap, each name once (SPEC §170.3): anything else
    /// is damage at load, not names renumbered.
    #[test]
    fn field_name_cells_that_dont_follow_on_are_an_error() {
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        for (cells, says) in [
            (
                vec![(0, names(&["a", "b"])), (1, names(&["c"]))],
                "numbered from 1 follow 2",
            ),
            (
                vec![(0, names(&["a"])), (2, names(&["c"]))],
                "numbered from 2 follow 1",
            ),
            (vec![(0, names(&["a", "a"]))], "comes twice"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
            Catalog::load(&mut store).unwrap();
            for (first, list) in &cells {
                for cell in encode_names(*first, list) {
                    append_cell(&mut store, &cell).unwrap();
                }
            }
            let Err(err) = Catalog::load(&mut store) else {
                panic!("{cells:?} loaded");
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            assert!(err.to_string().contains(says), "{err}");
        }
        // Following on, they load.
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        Catalog::load(&mut store).unwrap();
        for (first, list) in [(0, names(&["a", "b"])), (2, names(&["c"]))] {
            for cell in encode_names(first, &list) {
                append_cell(&mut store, &cell).unwrap();
            }
        }
        let loaded = Catalog::load(&mut store).unwrap();
        assert_eq!(loaded.names().iter().collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    /// Found by fuzzing (SPEC §55): a catalog page linking back to one
    /// before it is an error at load, not a load that never ends.
    #[test]
    fn a_catalog_chain_that_loops_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();
        catalog.create_collection(&mut store, "users").unwrap();
        let mut page = SlottedPage::from_bytes(store.read_page(CATALOG_PAGE_ID).unwrap()).unwrap();
        page.set_next_page(CATALOG_PAGE_ID);
        store.write_page(CATALOG_PAGE_ID, page.into_page()).unwrap();

        let Err(err) = Catalog::load(&mut store) else {
            panic!("a looping catalog loaded");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("comes twice"), "{err}");
    }

    /// Found by fuzzing (SPEC §55): a catalog cell cut short anywhere
    /// decodes or is an `InvalidData` error, never a panic; cut within its
    /// fixed part, it's always the error.
    #[test]
    fn truncated_catalog_cells_are_errors() {
        let meta = CollectionMeta {
            index_root: 3,
            current_data_page: 4,
        };
        let cells = [
            (encode_collection("users", &meta), COLLECTION_NAME_AT),
            (
                encode_index(
                    "users",
                    &IndexMeta {
                        fields: fields(&["a"]),
                        root: 9,
                        unique: false,
                        sparse: false,
                        left_out: None,
                    },
                ),
                10 + "users".len(),
            ),
            (
                encode_index(
                    "users",
                    &IndexMeta {
                        fields: fields(&["a", "b"]),
                        root: 9,
                        unique: true,
                        sparse: true,
                        left_out: None,
                    },
                ),
                10 + "users".len() + 2,
            ),
        ];
        for (cell, fixed) in cells {
            assert!(decode_entry(&cell).is_ok());
            for len in 0..cell.len() {
                match decode_entry(&cell[..len]) {
                    Ok(_) => assert!(len >= fixed, "{len} of {cell:?} decoded"),
                    Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
                }
            }
        }
    }

    #[test]
    fn overlong_field_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();
        let longest = "x".repeat(MAX_COLLECTION_NAME_LEN);
        catalog.create_collection(&mut store, &longest).unwrap();

        catalog
            .create_index(
                &mut store,
                &longest,
                &["f".repeat(MAX_FIELD_NAME_LEN)],
                PLAIN,
            )
            .unwrap();
        let too_long = "f".repeat(MAX_FIELD_NAME_LEN + 1);
        let Err(crate::Error::Io(err)) =
            catalog.create_index(&mut store, &longest, &[too_long], PLAIN)
        else {
            panic!("an overlong field name must be an I/O InvalidInput error");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
