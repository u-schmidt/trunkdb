//! Where a file's space goes (SPEC §93): `Database::stats`.

use crate::Database;
use crate::catalog::{Catalog, IndexMeta};
use crate::check::FileInfo;
use crate::data::{CellShape, PackedBuild, cell_shape, read_data_page};
use crate::database::ReadGuard;
use crate::index::{BTreeIndex, KeyRange, SortedBuild};
use crate::storage::{PageId, PageStore, PendingPages, RecordLocation, SlottedPage};
use std::collections::HashSet;

/// What `Database::stats` found, on one snapshot.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Stats {
    /// As `Database::file_info`: format, page size, pages, free pages.
    pub file: FileInfo,
    /// Every page of the file by what it holds.
    pub pages: PageUse,
    /// One per collection, by name.
    pub collections: Vec<CollectionStats>,
    /// Bytes in the WAL file at the call; `None` if there is none.
    pub wal_bytes: Option<u64>,
    /// What the open of this handle restored from the WAL, if anything.
    pub recovered: Option<Recovered>,
    compacted_pages: u64,
}

/// The file's pages by what they hold. The fields add up to the file's
/// pages.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct PageUse {
    /// The header: always 1.
    pub header: u64,
    pub catalog: u64,
    /// On the free list: `FileInfo::free_pages`.
    pub free: u64,
    /// Documents' pages, every collection's.
    pub data: u64,
    /// Large documents' overflow chains.
    pub overflow: u64,
    /// Primary and secondary indexes.
    pub index: u64,
    /// Pages nothing above claimed: leaked, normally none. `check` says
    /// which.
    pub unaccounted: u64,
}

/// One collection's documents and indexes.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CollectionStats {
    pub name: String,
    pub documents: u64,
    /// Its data pages, the current one included.
    pub data_pages: u64,
    pub overflow_pages: u64,
    /// The documents as stored, encoded: inline and in chains.
    pub document_bytes: u64,
    /// Room left in its data pages: what deletes and shrinking updates
    /// left, and the current page's tail.
    pub free_bytes: u64,
    /// The `_id` index.
    pub primary: IndexStats,
    /// Its secondary indexes, in the catalog's order.
    pub indexes: Vec<IndexStats>,
}

/// One index's size.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct IndexStats {
    /// `["_id"]` for the primary index.
    pub fields: Vec<String>,
    pub unique: bool,
    pub sparse: bool,
    /// One per document with a value — one per element in a multikey
    /// index (SPEC §42).
    pub entries: u64,
    pub pages: u64,
    /// Levels: 1 for a lone leaf.
    pub depth: u32,
    /// The entries' keys, together.
    pub key_bytes: u64,
}

/// What an open restored from the WAL after an unclean shutdown.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Recovered {
    /// Page images written back — or, read-only, kept in memory.
    pub pages: u64,
    /// A read-only open (SPEC §88): restored in memory, the file
    /// untouched.
    pub in_memory: bool,
}

impl Recovered {
    pub(crate) fn of(pending: &impl PendingPages, in_memory: bool) -> Option<Self> {
        let pages = pending.count();
        (pages > 0).then_some(Recovered { pages, in_memory })
    }
}

impl Stats {
    /// How many pages the file would have after `Database::compact`
    /// (SPEC §93.4): the same packing worked out from the entries' and
    /// documents' sizes instead of done, and like `compact`, never more
    /// than the file has now. The difference to `file.pages` is what a
    /// compaction would give back.
    pub fn compacted_pages_estimate(&self) -> u64 {
        self.compacted_pages
    }
}

impl Database {
    /// Where the file's space goes (SPEC §93): its pages by kind, each
    /// collection's documents, bytes and free room, each index's entries,
    /// pages and depth, the WAL's size, and what the open recovered.
    ///
    /// A read on one snapshot, like `check` (SPEC §80): every index page
    /// and every data page holding a document is read once, but no
    /// document is decoded and no index key rebuilt. A page it can't read
    /// fails it with `Error::Io` of kind `InvalidData`; a page nothing
    /// claims is only counted (`PageUse::unaccounted`).
    pub fn stats(&self) -> crate::Result<Stats> {
        let mut stats = Self::stats_of(&self.read()?)?;
        stats.wal_bytes = std::fs::metadata(self.wal_path())
            .ok()
            .map(|metadata| metadata.len());
        stats.recovered = self.recovered();
        Ok(stats)
    }

    fn stats_of(state: &ReadGuard<'_>) -> crate::Result<Stats> {
        let at = state.snapshot();
        let (catalog, store) = (at.catalog, &at.store);
        let file = Self::file_info_of(state)?;
        let mut names: Vec<&str> = catalog.names().collect();
        names.sort();
        let mut collections = Vec::with_capacity(names.len());
        let mut compacted = 1 + catalog.rebuilt_pages();
        for name in names {
            let (collection, pages_after) = collection_stats(catalog, store, name)?;
            compacted += pages_after;
            collections.push(collection);
        }

        let catalog_pages = Catalog::pages(store)?.len() as u64;
        let mut pages = PageUse {
            header: 1,
            catalog: catalog_pages,
            free: file.free_pages as u64,
            ..PageUse::default()
        };
        for collection in &collections {
            pages.data += collection.data_pages;
            pages.overflow += collection.overflow_pages;
            pages.index += std::iter::once(&collection.primary)
                .chain(&collection.indexes)
                .map(|index| index.pages)
                .sum::<u64>();
        }
        let claimed =
            pages.header + pages.catalog + pages.free + pages.data + pages.overflow + pages.index;
        pages.unaccounted = file.pages.saturating_sub(claimed);
        Ok(Stats {
            compacted_pages: compacted.min(file.pages),
            file,
            pages,
            collections,
            wal_bytes: None,
            recovered: None,
        })
    }
}

/// One collection's numbers, and the pages `compact` would give it.
fn collection_stats(
    catalog: &Catalog,
    store: &dyn PageStore,
    name: &str,
) -> crate::Result<(CollectionStats, u64)> {
    let meta = *catalog.get(name).expect("a listed collection");

    let mut packed = PackedBuild::default();
    let (mut documents, mut document_bytes, mut overflow_pages) = (0, 0, 0);
    let mut data_pages: HashSet<PageId> = HashSet::new();
    let mut free_bytes = 0;
    // Documents in id order lie mostly page after page: each page is
    // read once while its documents come.
    let mut page: Option<(PageId, SlottedPage)> = None;
    let mut visit_page = |page_id: PageId, data_pages: &mut HashSet<PageId>| {
        let read = read_data_page(store, page_id)?;
        if data_pages.insert(page_id) {
            free_bytes += read.reclaimable_space() as u64;
        }
        Ok::<_, std::io::Error>(read)
    };
    let primary = index_stats(store, meta.index_root, |_key_len, loc| {
        if page.as_ref().is_none_or(|(id, _)| *id != loc.page) {
            page = Some((loc.page, visit_page(loc.page, &mut data_pages)?));
        }
        let (_, on_page) = page.as_ref().expect("just read");
        let CellShape {
            cell_len,
            document_bytes: bytes,
            chain_pages,
        } = cell_shape(on_page, loc)?;
        documents += 1;
        document_bytes += bytes;
        overflow_pages += chain_pages;
        packed.push(cell_len);
        Ok(())
    })?;
    let (mut primary, primary_after) = primary;
    primary.fields = vec!["_id".to_string()];
    primary.unique = true;
    if meta.current_data_page != 0 && !data_pages.contains(&meta.current_data_page) {
        visit_page(meta.current_data_page, &mut data_pages)?;
    }

    let mut indexes = Vec::new();
    let mut pages_after = packed.pages() + overflow_pages + primary_after;
    for index in catalog.indexes(name) {
        let (stats, after) = secondary_stats(store, index)?;
        pages_after += after;
        indexes.push(stats);
    }
    Ok((
        CollectionStats {
            name: name.to_string(),
            documents,
            data_pages: data_pages.len() as u64,
            overflow_pages,
            document_bytes,
            free_bytes,
            primary,
            indexes,
        },
        pages_after,
    ))
}

fn secondary_stats(store: &dyn PageStore, index: &IndexMeta) -> crate::Result<(IndexStats, u64)> {
    let (mut stats, after) = index_stats(store, index.root, |_key_len, _loc| Ok(()))?;
    stats.fields = index.fields.clone();
    stats.unique = index.unique;
    stats.sparse = index.sparse;
    Ok((stats, after))
}

/// An index's pages, depth, entries and key bytes, handing each entry to
/// `visit` in key order; and the pages `compact`'s sorted rebuild of it
/// would take.
fn index_stats(
    store: &dyn PageStore,
    root: PageId,
    mut visit: impl FnMut(usize, RecordLocation) -> std::io::Result<()>,
) -> crate::Result<(IndexStats, u64)> {
    let tree = BTreeIndex::new(root);
    let pages = tree.pages(store)?.len() as u64;
    let depth = tree.depth(store)?;
    let (mut entries, mut key_bytes) = (0, 0);
    let mut rebuilt = SortedBuild::default();
    let all = KeyRange {
        start: Vec::new(),
        end: None,
    };
    for entry in tree.walk(store, all, false) {
        let (key, loc) = entry?;
        entries += 1;
        key_bytes += key.len() as u64;
        rebuilt.push(key.len());
        visit(key.len(), loc)?;
    }
    let stats = IndexStats {
        fields: Vec::new(),
        unique: false,
        sparse: false,
        entries,
        pages,
        depth,
        key_bytes,
    };
    Ok((stats, rebuilt.pages()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IndexOptions;
    use crate::document::Document;
    use crate::id::IdGenerator;
    use crate::index::Index;
    use crate::query::Filter;
    use std::path::Path;

    fn object(pairs: &[(&str, Document)]) -> Document {
        Document::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn text(len: usize) -> Document {
        Document::String("x".repeat(len))
    }

    /// Inserts `docs` into `collection` in one batch.
    fn insert_all(db: &Database, collection: &str, docs: Vec<Document>) {
        let ops = docs
            .into_iter()
            .map(|doc| {
                crate::txn::WriteOp::Insert(collection.to_string(), db.id_gen().generate(), doc)
            })
            .collect();
        db.write_batch(ops).unwrap();
    }

    /// Documents of every size — tiny, page-filling, overflowing — with
    /// a plain, a unique, a sparse, a compound, a nested and a multikey
    /// index, then deletes and updates that leave room behind, and a
    /// dropped collection.
    fn mixed(path: &Path) -> Database {
        let db = Database::open(path).unwrap();
        for name in ["people", "notes", "gone"] {
            let docs = db.collection::<Document>(name);
            docs.ensure_index("age").unwrap();
            docs.ensure_index_with("email", IndexOptions::new().unique())
                .unwrap();
            docs.ensure_index_with("nick", IndexOptions::new().sparse())
                .unwrap();
            docs.ensure_index_with(["team", "age"], IndexOptions::new())
                .unwrap();
            docs.ensure_index("address.city").unwrap();
            docs.ensure_index("tags[*]").unwrap();
            let mut batch = Vec::new();
            for i in 0..600i64 {
                let pad = match i % 101 {
                    0 => 20_000 + i as usize, // overflow pages
                    n => (n * 37 % 1900) as usize,
                };
                let mut fields = vec![
                    ("age", Document::Int(i % 70)),
                    ("email", Document::String(format!("{i}@{name}.example"))),
                    ("team", Document::String(format!("team-{}", i % 9))),
                    ("pad", text(pad)),
                    (
                        "tags",
                        Document::Array((0..i % 4).map(|t| Document::Int(t * i)).collect()),
                    ),
                ];
                if i % 3 == 0 {
                    fields.push(("nick", Document::String(format!("n{i}"))));
                }
                if i % 5 == 0 {
                    let city = object(&[("city", Document::String(format!("c{}", i % 7)))]);
                    fields.push(("address", city));
                }
                batch.push(object(&fields));
            }
            insert_all(&db, name, batch);
            docs.delete_many(Filter::new().eq("age", 3)).unwrap();
            docs.update_many(Filter::new().eq("age", 40), |doc| {
                if let Document::Object(fields) = doc {
                    fields.insert("pad".into(), text(4));
                }
            })
            .unwrap();
        }
        assert!(db.drop_collection("gone").unwrap());
        db
    }

    /// Many small documents with long index keys: indexes several levels
    /// deep, their branches split.
    fn deep(path: &Path) -> Database {
        let db = Database::open(path).unwrap();
        let docs = db.collection::<Document>("long");
        docs.ensure_index("key").unwrap();
        let batch: Vec<Document> = (0..6000)
            .map(|i| object(&[("key", Document::String(format!("{i:0>200}")))]))
            .collect();
        insert_all(&db, "long", batch);
        docs.delete_many(Filter::new().lt("key", Document::String(format!("{:0>200}", 1500))))
            .unwrap();
        db
    }

    /// Only documents that overflow.
    fn large(path: &Path) -> Database {
        let db = Database::open(path).unwrap();
        let docs = db.collection::<Document>("blobs");
        for i in 0..20 {
            docs.insert(object(&[("blob", text(9000 + i * 3000))]))
                .unwrap();
        }
        docs.delete_many(Filter::new().limit(5)).unwrap();
        db
    }

    /// A collection with indexes and no documents, and one with nothing.
    fn empty(path: &Path) -> Database {
        let db = Database::open(path).unwrap();
        let docs = db.collection::<Document>("none");
        docs.ensure_index("a").unwrap();
        docs.insert(object(&[("a", Document::Int(1))])).unwrap();
        docs.delete_many(Filter::new()).unwrap();
        db.collection::<Document>("bare").ensure_index("b").unwrap();
        db
    }

    /// Documents whose sizes leave pages a quarter empty behind them, so
    /// compaction's packing goes back to them through the free-space map
    /// (SPEC §75).
    fn uneven(path: &Path) -> Database {
        let db = Database::open(path).unwrap();
        let docs = (0..480)
            .map(|i| {
                let size = [5000, 5000, 3000, 3000][i % 4];
                object(&[("i", Document::Int(i as i64)), ("pad", text(size))])
            })
            .collect();
        insert_all(&db, "uneven", docs);
        // Every second group of four: the pattern stays, and compacting
        // has pages to give back.
        let gone = (0..480)
            .filter(|i| i % 8 >= 4)
            .map(|i| crate::query::Condition::eq("i", i));
        db.collection::<Document>("uneven")
            .delete_many(Filter::new().any_of(gone))
            .unwrap();
        db
    }

    /// A catalog over several pages: many collections, their names long
    /// and short, each with an index, some dropped.
    fn many(path: &Path) -> Database {
        let db = Database::open(path).unwrap();
        for i in 0..300 {
            let name = match i % 2 {
                0 => format!("{i:0>100}"),
                _ => format!("c{i}"),
            };
            let docs = db.collection::<Document>(&name);
            docs.ensure_index(format!("{i:x>255}").as_str()).unwrap();
            docs.insert(object(&[("i", Document::Int(i))])).unwrap();
        }
        for i in (1..300).step_by(14) {
            db.drop_collection(&format!("c{i}")).unwrap();
        }
        db
    }

    /// A named way to make a database to measure.
    type Shape = (&'static str, fn(&Path) -> Database);

    const SHAPES: [Shape; 6] = [
        ("mixed", mixed),
        ("deep", deep),
        ("large", large),
        ("empty", empty),
        ("uneven", uneven),
        ("many", many),
    ];

    /// What each collection holds, read independently of `stats`: its
    /// documents re-encoded, its index keys worked out from them, and the
    /// pages `check` expects it to own.
    struct Expected {
        documents: u64,
        document_bytes: u64,
        data_pages: u64,
        overflow_pages: u64,
        /// Per secondary index: entries and key bytes.
        indexes: Vec<(u64, u64)>,
    }

    fn expected(db: &Database, name: &str) -> Expected {
        let state = db.read().unwrap();
        let at = state.snapshot();
        let meta = *at.catalog.get(name).unwrap();
        let entries = BTreeIndex::new(meta.index_root).scan(&at.store).unwrap();
        let documents: Vec<_> = entries
            .iter()
            .map(|(_key, loc)| {
                let (id, doc) = crate::data::get_record(&at.store, *loc).unwrap();
                (id, doc, *loc)
            })
            .collect();
        let (data_pages, chain_pages) = crate::data::collection_pages(
            &at.store,
            meta.current_data_page,
            documents.iter().map(|(_, _, loc)| *loc),
        )
        .unwrap();
        let indexes = at
            .catalog
            .indexes(name)
            .iter()
            .map(|index| {
                let keys: Vec<Vec<u8>> = documents
                    .iter()
                    .flat_map(|(id, doc, _)| crate::collection::index_keys(doc, index, *id))
                    .collect();
                let bytes = keys.iter().map(|key| key.len() as u64).sum();
                (keys.len() as u64, bytes)
            })
            .collect();
        Expected {
            documents: documents.len() as u64,
            document_bytes: documents
                .iter()
                .map(|(_, doc, _)| crate::document::encode_stored(doc).len() as u64)
                .sum(),
            data_pages: data_pages.len() as u64,
            overflow_pages: chain_pages.len() as u64,
            indexes,
        }
    }

    /// Every number against what was read the other way, and the pages
    /// by kind adding up to the file, with nothing unaccounted in a file
    /// `check` passes.
    #[test]
    fn the_numbers_agree_with_the_documents_and_the_pages() {
        for (shape, make) in SHAPES {
            let dir = tempfile::tempdir().unwrap();
            let db = make(&dir.path().join("db.trunkdb"));
            assert!(db.check().unwrap().is_ok(), "{shape}");
            let stats = db.stats().unwrap();
            let pages = stats.pages;
            assert_eq!(
                pages.header
                    + pages.catalog
                    + pages.free
                    + pages.data
                    + pages.overflow
                    + pages.index
                    + pages.unaccounted,
                stats.file.pages,
                "{shape}"
            );
            assert_eq!(pages.unaccounted, 0, "{shape}");
            assert_eq!(pages.free, stats.file.free_pages as u64, "{shape}");

            let mut names = db.collections().unwrap();
            names.sort();
            let listed: Vec<&str> = stats.collections.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(listed, names, "{shape}");
            for collection in &stats.collections {
                let what = format!("{shape}/{}", collection.name);
                let expected = expected(&db, &collection.name);
                let count = db
                    .collection::<Document>(&collection.name)
                    .count(Filter::new())
                    .unwrap() as u64;
                assert_eq!(collection.documents, count, "{what}");
                assert_eq!(collection.documents, expected.documents, "{what}");
                assert_eq!(collection.primary.entries, count, "{what}");
                assert_eq!(collection.primary.key_bytes, 16 * count, "{what}");
                assert_eq!(collection.primary.fields, ["_id"], "{what}");
                assert_eq!(collection.document_bytes, expected.document_bytes, "{what}");
                assert_eq!(collection.data_pages, expected.data_pages, "{what}");
                assert_eq!(collection.overflow_pages, expected.overflow_pages, "{what}");
                let indexes: Vec<(u64, u64)> = collection
                    .indexes
                    .iter()
                    .map(|index| (index.entries, index.key_bytes))
                    .collect();
                assert_eq!(indexes, expected.indexes, "{what}");
            }
        }
    }

    #[test]
    fn indexes_say_their_options_and_depth() {
        let dir = tempfile::tempdir().unwrap();
        let stats = mixed(&dir.path().join("db.trunkdb")).stats().unwrap();
        let people = &stats.collections[1];
        assert_eq!(people.name, "people");
        let options: Vec<(String, bool, bool)> = people
            .indexes
            .iter()
            .map(|index| (index.fields.join(","), index.unique, index.sparse))
            .collect();
        assert_eq!(
            options,
            [
                ("age".to_string(), false, false),
                ("email".to_string(), true, false),
                ("nick".to_string(), false, true),
                ("team,age".to_string(), false, false),
                ("address.city".to_string(), false, false),
                ("tags[*]".to_string(), false, false),
            ]
        );
        // Several elements a document: more entries than documents.
        let tags = &people.indexes[5];
        assert!(tags.entries > people.documents, "{tags:?}");

        let deep = deep(&dir.path().join("deep.trunkdb")).stats().unwrap();
        let long = &deep.collections[0];
        assert!(long.indexes[0].depth >= 3, "{:?}", long.indexes[0]);
        assert!(long.indexes[0].pages > long.primary.pages);

        let empty = empty(&dir.path().join("empty.trunkdb")).stats().unwrap();
        for collection in &empty.collections {
            assert_eq!(collection.documents, 0);
            for index in std::iter::once(&collection.primary).chain(&collection.indexes) {
                assert_eq!((index.entries, index.pages, index.depth), (0, 1, 1));
            }
        }
    }

    /// Deletes leave room in the pages they thin out, and pages they
    /// empty go to the free list.
    #[test]
    fn deletes_show_as_free_room_and_free_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("db.trunkdb")).unwrap();
        let docs: Vec<Document> = (0..400)
            .map(|i| object(&[("n", Document::Int(i)), ("pad", text(300))]))
            .collect();
        insert_all(&db, "c", docs);
        let collection = db.collection::<Document>("c");
        let before = db.stats().unwrap();

        // Every other one of the first half: each of their pages thinned.
        for n in (0..200).step_by(2) {
            collection.delete_many(Filter::new().eq("n", n)).unwrap();
        }
        let thinned = db.stats().unwrap();
        let (b, t) = (&before.collections[0], &thinned.collections[0]);
        assert_eq!(t.documents, 300);
        assert_eq!(t.data_pages, b.data_pages);
        assert!(
            t.free_bytes > b.free_bytes + 100 * 300,
            "{} > {}",
            t.free_bytes,
            b.free_bytes
        );

        // All of the second half: its pages emptied and freed.
        collection.delete_many(Filter::new().gte("n", 200)).unwrap();
        let emptied = db.stats().unwrap();
        assert!(emptied.collections[0].data_pages < t.data_pages);
        assert!(emptied.pages.free > thinned.pages.free);
        assert!(emptied.compacted_pages_estimate() < emptied.file.pages);
    }

    /// A page nothing owns is counted, not an error.
    #[test]
    fn a_leaked_page_is_unaccounted() {
        let dir = tempfile::tempdir().unwrap();
        let db = mixed(&dir.path().join("db.trunkdb"));
        db.transact(|_, store| Ok(store.allocate_page()?)).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(stats.pages.unaccounted, 1);
    }

    /// A data page it has to read and can't fails the call, as damage.
    #[test]
    fn a_damaged_data_page_fails_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.trunkdb");
        let db = mixed(&path);
        let page = {
            let state = db.read().unwrap();
            state
                .snapshot()
                .catalog
                .get("people")
                .unwrap()
                .current_data_page
        };
        drop(db);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[page as usize * crate::storage::PAGE_SIZE + 500] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let db = Database::open(&path).unwrap();
        match db.stats() {
            Err(crate::Error::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}")
            }
            other => panic!("expected damage, got {other:?}"),
        }
    }

    /// Read-only, the same numbers; the WAL's size is the file's, and
    /// `None` where there is no WAL.
    #[test]
    fn a_read_only_open_sees_the_same_and_the_wal_is_measured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.trunkdb");
        let db = mixed(&path);
        let stats = db.stats().unwrap();
        let wal = path.with_extension("trunkdb.wal");
        assert_eq!(
            stats.wal_bytes,
            Some(std::fs::metadata(&wal).unwrap().len())
        );
        assert_eq!(stats.recovered, None);
        drop(db);
        std::fs::remove_file(&wal).unwrap();
        let before = std::fs::read(&path).unwrap();

        let read_only =
            Database::open_with(&path, crate::OpenOptions::default().read_only(true)).unwrap();
        let seen = read_only.stats().unwrap();
        assert_eq!(seen.wal_bytes, None);
        assert_eq!(
            (
                &seen.file,
                seen.pages.clone(),
                &seen.collections,
                seen.compacted_pages_estimate()
            ),
            (
                &stats.file,
                stats.pages.clone(),
                &stats.collections,
                stats.compacted_pages_estimate()
            )
        );
        drop(read_only);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// The estimate is what `compact` then makes, page for page.
    #[test]
    fn the_estimate_is_what_compact_makes() {
        for (shape, make) in SHAPES {
            let dir = tempfile::tempdir().unwrap();
            let db = make(&dir.path().join("db.trunkdb"));
            let stats = db.stats().unwrap();
            let compacted = db.compact().unwrap();
            assert_eq!(
                (stats.file.pages, stats.compacted_pages_estimate()),
                (compacted.pages_before, compacted.pages_after),
                "{shape}"
            );
            // And of the compacted file, itself.
            let again = db.stats().unwrap();
            assert_eq!(
                again.compacted_pages_estimate(),
                again.file.pages,
                "{shape}"
            );
        }
    }
}
