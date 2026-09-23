use crate::Result;
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

#[derive(Clone, Copy)]
pub struct CollectionMeta {
    /// Root of the primary (`_id`) index.
    pub index_root: PageId,
    /// The data page this collection's next insert tries first (see
    /// `data::insert_record`); `0` until its first insert.
    pub current_data_page: PageId,
}

/// A secondary index on one top-level field of a collection (SPEC §28).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexMeta {
    pub field: String,
    pub root: PageId,
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

        loop {
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

    /// Records a new, empty secondary index on `field` — building it from
    /// the collection's documents is the caller's job. `collection` must
    /// exist and not have an index on `field` yet.
    pub fn create_index(
        &mut self,
        store: &mut dyn PageStore,
        collection: &str,
        field: &str,
    ) -> Result<IndexMeta> {
        check_len("field name", field, MAX_FIELD_NAME_LEN)?;
        assert!(self.collections.contains_key(collection));
        assert!(!self.indexes(collection).iter().any(|i| i.field == field));
        let index = IndexMeta {
            field: field.to_string(),
            root: allocate_index_root(store)?,
        };
        append_cell(store, &encode_index(collection, &index))?;
        self.indexes
            .entry(collection.to_string())
            .or_default()
            .push(index.clone());
        Ok(index)
    }

    /// Removes the index on `field` from the catalog and returns it, so
    /// the caller can free its pages; `None` if there's no such index.
    pub fn drop_index(
        &mut self,
        store: &mut dyn PageStore,
        collection: &str,
        field: &str,
    ) -> Result<Option<IndexMeta>> {
        let Some(list) = self.indexes.get_mut(collection) else {
            return Ok(None);
        };
        let Some(pos) = list.iter().position(|i| i.field == field) else {
            return Ok(None);
        };
        let index = list.remove(pos);
        let cell = encode_index(collection, &index);
        let (page_id, mut page, slot) = find_cell(store, |c| c == cell.as_slice())?
            .ok_or_else(|| corrupt(format!("index {collection}.{field} has no catalog entry")))?;
        page.delete_cell(slot);
        store.write_page(page_id, &page.into_bytes())?;
        Ok(Some(index))
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

        let is_this_collection =
            |c: &[u8]| c[0] == KIND_COLLECTION && c[COLLECTION_NAME_AT..] == *name.as_bytes();
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

/// An index cell: `[u8 kind = 1][u64 root][u8 collection name
/// length][collection name][field name]` — a collection name is at most
/// 255 bytes, so one length byte does.
fn encode_index(collection: &str, index: &IndexMeta) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(10 + collection.len() + index.field.len());
    buffer.push(KIND_INDEX);
    buffer.extend_from_slice(&index.root.to_le_bytes());
    buffer.push(collection.len() as u8);
    buffer.extend_from_slice(collection.as_bytes());
    buffer.extend_from_slice(index.field.as_bytes());
    buffer
}

fn decode_entry(cell: &[u8]) -> std::io::Result<Entry> {
    let utf8 = |bytes: &[u8]| {
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("bad utf8 in catalog".into()))
    };
    let u64_at = |at: usize| PageId::from_le_bytes(cell[at..at + 8].try_into().unwrap());
    match cell[0] {
        KIND_COLLECTION => Ok(Entry::Collection(
            utf8(&cell[COLLECTION_NAME_AT..])?,
            CollectionMeta {
                index_root: u64_at(1),
                current_data_page: u64_at(9),
            },
        )),
        KIND_INDEX => {
            let name_len = cell[9] as usize;
            let (collection, field) = cell[10..].split_at(name_len);
            Ok(Entry::Index(
                utf8(collection)?,
                IndexMeta {
                    field: utf8(field)?,
                    root: u64_at(1),
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

    #[test]
    fn indexes_are_created_dropped_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");

        let (email, age) = {
            let mut store = FileStore::open(&path).unwrap();
            let mut catalog = Catalog::load(&mut store).unwrap();
            catalog.create_collection(&mut store, "users").unwrap();
            catalog.create_collection(&mut store, "posts").unwrap();
            let email = catalog.create_index(&mut store, "users", "email").unwrap();
            let age = catalog.create_index(&mut store, "users", "age").unwrap();
            catalog.create_index(&mut store, "posts", "title").unwrap();
            assert_eq!(catalog.indexes("users"), [email.clone(), age.clone()]);

            let dropped = catalog.drop_index(&mut store, "posts", "title").unwrap();
            assert_eq!(dropped.map(|i| i.field), Some("title".to_string()));
            assert_eq!(
                catalog.drop_index(&mut store, "posts", "title").unwrap(),
                None
            );
            // Still findable after an index cell came before it.
            catalog
                .set_current_data_page(&mut store, "posts", 7)
                .unwrap();
            (email, age)
        };

        let mut store = FileStore::open(&path).unwrap();
        let catalog = Catalog::load(&mut store).unwrap();
        assert_eq!(catalog.indexes("users"), [email, age]);
        assert_eq!(catalog.indexes("posts"), []);
        assert_eq!(catalog.indexes("nothing"), []);
        assert_eq!(catalog.get("posts").unwrap().current_data_page, 7);
    }

    #[test]
    fn overlong_field_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        let mut catalog = Catalog::load(&mut store).unwrap();
        let longest = "x".repeat(MAX_COLLECTION_NAME_LEN);
        catalog.create_collection(&mut store, &longest).unwrap();

        catalog
            .create_index(&mut store, &longest, &"f".repeat(MAX_FIELD_NAME_LEN))
            .unwrap();
        let too_long = "f".repeat(MAX_FIELD_NAME_LEN + 1);
        let Err(crate::Error::Io(err)) = catalog.create_index(&mut store, &longest, &too_long)
        else {
            panic!("an overlong field name must be an I/O InvalidInput error");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
