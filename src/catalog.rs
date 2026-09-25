use crate::Result;
use crate::decode::{take, take_u8, take_u64};
use crate::storage::{PageId, PageStore, PageType, SlottedPage};
use std::collections::HashMap;

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
}

/// How `Collection::ensure_index_with` builds an index: the default is
/// what `ensure_index` builds.
///
/// ```
/// use trunkdb::IndexOptions;
///
/// let options = IndexOptions { sparse: true, ..IndexOptions::default() };
/// # assert!(options.sparse && !options.unique);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexOptions {
    /// No two documents may have equal values in it (SPEC §33) —
    /// `ensure_unique_index`.
    pub unique: bool,
    /// Documents with a null or missing value get no entry (SPEC §44):
    /// smaller for a field few documents have, but only used for
    /// queries that rule nulls out.
    pub sparse: bool,
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

    /// The field of a one-field index.
    pub fn single(&self) -> Option<&str> {
        match self.fields.as_slice() {
            [field] => Some(field),
            _ => None,
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
}

impl Catalog {
    /// Reads the catalog page chain (from page 1), decoding its cells into
    /// collections and indexes. If the file is fresh and page 1 doesn't
    /// exist yet, creates and initializes an empty catalog page first,
    /// then returns an empty `Catalog` — this is why `store` needs to be
    /// `&mut`, not `&`.
    pub fn load(store: &mut dyn PageStore) -> std::io::Result<Self> {
        let mut collections = HashMap::new();
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
                    let bytes = SlottedPage::new(PageType::Catalog).into_bytes();
                    store.write_page(id, &bytes)?;
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
        Ok(Catalog {
            collections,
            indexes,
        })
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
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.collections.keys().map(String::as_str)
    }

    /// The collection's secondary indexes — empty if it has none, or
    /// doesn't exist.
    pub fn indexes(&self, collection: &str) -> &[IndexMeta] {
        self.indexes.get(collection).map_or(&[], Vec::as_slice)
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
        let index = IndexMeta {
            fields: fields.to_vec(),
            root: allocate_index_root(store)?,
            unique: options.unique,
            sparse: options.sparse,
        };
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
        store.write_page(page_id, &page.into_bytes())?;
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
        store.write_page(page_id, &page.into_bytes())?;
        self.collections.remove(name);
        self.indexes.remove(name);
        Ok(Some((meta, indexes)))
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
        store.write_page(page_id, &page.into_bytes())?;
        Ok(())
    }
}

/// An index's root starts out as an empty leaf, written right away — the
/// index's first read would otherwise misread whatever the page held
/// (SPEC §10.7).
fn allocate_index_root(store: &mut dyn PageStore) -> std::io::Result<PageId> {
    let root = store.allocate_page()?;
    store.write_page(root, &SlottedPage::new(PageType::IndexLeaf).into_bytes())?;
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
        store.write_page(page_id, &page.into_bytes())?;

        page = SlottedPage::new(PageType::Catalog);
        page_id = new_page_id;
    }

    page.insert_cell(cell)
        .expect("just verified has_room_for(cell.len())");
    store.write_page(page_id, &page.into_bytes())
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
/// byte does.
fn encode_index(collection: &str, index: &IndexMeta) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(12 + collection.len() + 256 * index.fields.len());
    let plain = !index.is_compound() && !index.sparse;
    buffer.push(match (plain, index.unique) {
        (true, false) => KIND_INDEX,
        (true, true) => KIND_UNIQUE_INDEX,
        (false, _) => KIND_INDEX_WITH_FLAGS,
    });
    buffer.extend_from_slice(&index.root.to_le_bytes());
    buffer.push(collection.len() as u8);
    buffer.extend_from_slice(collection.as_bytes());
    if plain {
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
                },
            ))
        }
        KIND_INDEX_WITH_FLAGS => {
            let name_len = take_u8(&mut rest, "an index's collection name")? as usize;
            let collection = take(&mut rest, name_len, "an index's collection name")?;
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
            Ok(Entry::Index(
                utf8(collection)?,
                IndexMeta {
                    fields,
                    root,
                    unique: flags & FLAG_UNIQUE != 0,
                    sparse: flags & FLAG_SPARSE != 0,
                },
            ))
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
        store
            .write_page(CATALOG_PAGE_ID, &page.into_bytes())
            .unwrap();

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
