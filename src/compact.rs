//! `Database::compact` (SPEC §41): rebuilds the whole file with every
//! page full and nothing free, and gives the rest back to the file
//! system.

use crate::catalog::{Catalog, IndexMeta};
use crate::collection::{secondary_entries, secondary_keys};
use crate::data;
use crate::database::Database;
use crate::index::{BTreeIndex, Index, key};
use crate::storage::{FileStore, MemoryStore, RecordLocation};

/// What `Database::compact` did: the file's pages before and after, the
/// header included. Equal when it was already as small as a rebuild
/// makes it — then nothing was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    pub pages_before: u64,
    pub pages_after: u64,
}

impl Database {
    /// Rebuilds the database into as few pages as it needs, and cuts the
    /// file to that (SPEC §41): every collection's documents packed into
    /// full data pages, every index rebuilt from sorted keys into full
    /// leaves, no free pages. Documents keep their ids; what `find`
    /// returns doesn't change.
    ///
    /// The new file is built in memory, then written like any batch —
    /// through the WAL, so a crash leaves the old file or the new one,
    /// never a mix. That needs memory for the whole new file, twice (the
    /// pages, and the WAL record of them). It holds the write lock
    /// throughout, so reads and writes wait.
    pub fn compact(&self) -> crate::Result<Compacted> {
        self.transact(|catalog, store| {
            let pages_before = store.page_count();
            let (rebuilt, image) = rebuild(catalog, store)?;
            let pages_after = image.page_count();
            if pages_after >= pages_before {
                // Already this small: rewriting it would gain nothing.
                return Ok(Compacted {
                    pages_before,
                    pages_after: pages_before,
                });
            }
            store.replace_all(image.into_pages(), pages_after)?;
            *catalog = rebuilt;
            Ok(Compacted {
                pages_before,
                pages_after,
            })
        })
    }
}

/// Copies every collection, in name order, into a new image: its
/// documents in id order (so data pages fill one after another and the
/// primary index grows at its end), then each index from its keys,
/// sorted. Returns the image's catalog along with it.
fn rebuild(catalog: &Catalog, store: &FileStore) -> crate::Result<(Catalog, MemoryStore)> {
    let mut image = MemoryStore::new();
    let mut rebuilt = Catalog::load(&mut image)?;
    let mut names: Vec<&str> = catalog.names().collect();
    names.sort();
    for name in names {
        let meta = *catalog.get(name).expect("a listed collection");
        let new_meta = rebuilt.create_collection(&mut image, name)?;
        let indexes = catalog.indexes(name);
        let mut index_entries: Vec<Vec<(Vec<u8>, RecordLocation)>> =
            vec![Vec::new(); indexes.len()];

        let mut primary = BTreeIndex::new(new_meta.index_root);
        let mut current = 0;
        let mut records = data::Records::new(store);
        for (primary_key, loc) in BTreeIndex::new(meta.index_root).scan(store)? {
            let (id, doc) = records.get(loc)?;
            let new_loc = data::insert_record(&mut image, &mut current, id, &doc)?;
            primary.insert(&mut image, &primary_key, new_loc)?;
            for (entries, index) in index_entries.iter_mut().zip(indexes) {
                for key in secondary_keys(&doc, &index.field, id) {
                    entries.push((key, new_loc));
                }
            }
        }
        if current != 0 {
            rebuilt.set_current_data_page(&mut image, name, current)?;
        }

        for (index, mut entries) in indexes.iter().zip(index_entries) {
            let new_index = rebuilt.create_index(&mut image, name, &index.field, index.unique)?;
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            if index.unique {
                refuse_duplicates(&image, name, &new_index, &entries)?;
            }
            let mut tree = BTreeIndex::new(new_index.root);
            for (key, loc) in entries {
                tree.insert(&mut image, &key, loc)?;
            }
        }
    }
    Ok((rebuilt, image))
}

/// A unique index's sorted entries, checked the way `check_unique` checks
/// one insert (SPEC §33): equal values share their key's value part, so
/// only neighbors with the same value part can be duplicates, and those
/// are compared by value. A file whose unique index holds a duplicate
/// isn't compacted into another one.
fn refuse_duplicates(
    image: &MemoryStore,
    collection: &str,
    index: &IndexMeta,
    entries: &[(Vec<u8>, RecordLocation)],
) -> crate::Result<()> {
    let groups = entries.chunk_by(|(a, _), (b, _)| key::value_part(a) == key::value_part(b));
    for group in groups.filter(|group| group.len() > 1) {
        let mut seen: Vec<(crate::DocId, crate::Document)> = Vec::new();
        for (key, loc) in group {
            let (id, doc) = data::get_record(image, *loc)?;
            // The values this entry is for — in a multikey index, some of
            // the document's elements (SPEC §42.2).
            let entries = secondary_entries(&doc, &index.field, id);
            let (_, values) = entries
                .iter()
                .find(|(k, _)| k == key)
                .expect("the entry came from this document");
            for value in values {
                if matches!(value, crate::Document::Null) {
                    continue;
                }
                if let Some((existing, _)) = seen
                    .iter()
                    .find(|(other_id, other)| *other_id != id && crate::query::equal(other, value))
                {
                    return Err(crate::Error::DuplicateValue {
                        collection: collection.to_string(),
                        field: index.field.clone(),
                        id,
                        existing: *existing,
                    });
                }
                seen.push((id, (*value).clone()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocId, Document};
    use crate::id::IdGenerator;
    use crate::query::Filter;
    use crate::storage::PAGE_SIZE;
    use std::path::Path;

    fn object(pairs: &[(&str, Document)]) -> Document {
        Document::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// A database with room to compact: three collections (one
    /// emptied), indexes of every kind — unique, nested, on a field few
    /// documents have — large documents in overflow pages, documents
    /// deleted, grown and shrunk, and a dropped collection.
    fn churned(path: &Path) -> Database {
        let db = Database::open(path).unwrap();
        for name in ["people", "notes", "gone", "emptied"] {
            let docs = db.collection::<Document>(name);
            docs.ensure_index("age").unwrap();
            docs.ensure_unique_index("email").unwrap();
            docs.ensure_index("address.city").unwrap();
            let mut ops = Vec::new();
            for i in 0..400i64 {
                let pad = match i % 97 {
                    0 => 30_000, // overflow pages
                    n => (n * 41 % 1500) as usize,
                };
                let mut fields = vec![
                    ("age", Document::Int(i % 60)),
                    ("email", Document::String(format!("{i}@{name}"))),
                    ("pad", Document::String("p".repeat(pad))),
                ];
                if i % 7 == 0 {
                    let city = object(&[("city", Document::String(format!("c{}", i % 5)))]);
                    fields.push(("address", city));
                }
                if i % 11 == 0 {
                    fields.push(("email", Document::Null)); // nulls are exempt
                }
                let id = db.id_gen().generate();
                ops.push(crate::txn::WriteOp::Insert(
                    name.into(),
                    id,
                    object(&fields),
                ));
            }
            db.write_batch(ops).unwrap();
            docs.delete_many(Filter::new().lt("age", 25)).unwrap();
            docs.update_many(Filter::new().gte("age", 50), |doc| {
                if let Document::Object(fields) = doc {
                    fields.insert("pad".into(), Document::String("q".repeat(2500)));
                }
            })
            .unwrap();
            docs.update_many(Filter::new().eq("age", 30), |doc| {
                if let Document::Object(fields) = doc {
                    fields.insert("pad".into(), Document::String(String::new()));
                }
            })
            .unwrap();
        }
        db.collection::<Document>("emptied")
            .delete_many(Filter::new())
            .unwrap();
        assert!(db.drop_collection("gone").unwrap());
        db
    }

    /// Everything a caller can see: each collection's documents with
    /// their ids, its indexes, which of them are unique, and what an
    /// indexed query finds.
    fn contents(db: &Database) -> Vec<String> {
        let mut names = db.collections().unwrap();
        names.sort();
        let mut contents = Vec::new();
        for name in names {
            let docs = db.collection::<Document>(&name);
            let mut all = docs.find_with_ids(Filter::new()).unwrap();
            all.sort_by_key(|(id, _)| *id);
            contents.push(format!(
                "{name}: {:?} {:?} {all:?}",
                docs.indexes().unwrap(),
                docs.unique_indexes().unwrap()
            ));
            for filter in [
                Filter::new().gte("age", 40).sort_desc("age").limit(7),
                Filter::new().eq("address.city", "c3"),
                Filter::new().eq("email", "331@people"),
            ] {
                contents.push(format!("{:?}", docs.find(filter).unwrap()));
            }
        }
        contents
    }

    fn file_pages(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len() / PAGE_SIZE as u64
    }

    #[test]
    fn compacting_keeps_everything_and_shrinks_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = churned(&path);
        let before = contents(&db);
        let pages_before = db.file_info().unwrap().pages;

        let compacted = db.compact().unwrap();
        assert_eq!(compacted.pages_before, pages_before);
        assert!(
            compacted.pages_after * 10 < pages_before * 7,
            "{compacted:?}: expected at least 30% smaller"
        );
        let info = db.file_info().unwrap();
        assert_eq!((info.pages, info.free_pages), (compacted.pages_after, 0));
        assert_eq!(contents(&db), before);
        assert!(db.check().unwrap().is_ok(), "{:?}", db.check().unwrap());

        // Writes go on as usual, the current data pages included.
        let people = db.collection::<Document>("people");
        people
            .insert(object(&[("age", Document::Int(99))]))
            .unwrap();
        assert!(
            people
                .insert(object(&[("email", "40@people".into())]))
                .is_err()
        );
        people.delete_many(Filter::new().eq("age", 99)).unwrap();
        db.collection::<Document>("emptied")
            .insert(object(&[("age", Document::Int(1))]))
            .unwrap();
        db.collection::<Document>("emptied")
            .delete_many(Filter::new())
            .unwrap();
        assert!(db.check().unwrap().is_ok());
        let pages_now = db.file_info().unwrap().pages;
        drop((people, db));

        assert_eq!(file_pages(&path), pages_now);
        let db = Database::open(&path).unwrap();
        assert_eq!(contents(&db), before);
        assert!(db.check().unwrap().is_ok());
    }

    /// As small as inserting the same documents into a new file in id
    /// order — the best case of ordinary writes — or smaller.
    #[test]
    fn a_compacted_file_is_no_larger_than_the_same_documents_inserted_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let db = churned(&dir.path().join("test.trunkdb"));
        let compacted = db.compact().unwrap();

        let fresh = Database::open(dir.path().join("fresh.trunkdb")).unwrap();
        for name in db.collections().unwrap() {
            let (from, to) = (
                db.collection::<Document>(&name),
                fresh.collection::<Document>(&name),
            );
            let unique = from.unique_indexes().unwrap();
            for field in from.indexes().unwrap() {
                match unique.contains(&field) {
                    true => to.ensure_unique_index(&field).unwrap(),
                    false => to.ensure_index(&field).unwrap(),
                };
            }
            let mut all = from.find_with_ids(Filter::new()).unwrap();
            all.sort_by_key(|(id, _)| *id);
            fresh
                .write_batch(
                    all.into_iter()
                        .map(|(id, doc)| crate::txn::WriteOp::Insert(name.clone(), id, doc))
                        .collect(),
                )
                .unwrap();
        }
        let fresh_pages = fresh.file_info().unwrap().pages;
        assert!(
            compacted.pages_after <= fresh_pages,
            "{compacted:?}, fresh: {fresh_pages}"
        );
    }

    /// Compacting a compact file writes nothing: the file stays exactly
    /// as it was, and a write-back set up to fail never happens.
    #[test]
    fn a_compact_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = churned(&path);
        let first = db.compact().unwrap();
        drop(db);
        let bytes = std::fs::read(&path).unwrap();

        let db = Database::open(&path).unwrap();
        db.state().store.failing_write_backs = 2;
        let again = db.compact().unwrap();
        assert_eq!(
            (again.pages_before, again.pages_after),
            (first.pages_after, first.pages_after)
        );
        drop(db);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    /// A secondary index whose values arrived in random order is rebuilt
    /// from sorted keys: every leaf full but the last.
    #[test]
    fn a_rebuilt_index_has_full_leaves() {
        use crate::storage::{PageStore, PageType, SlottedPage};
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.ensure_index("v").unwrap();
        let mut rng = crate::testing::XorShift(0x9E37_79B9_7F4A_7C15);
        let ops = (0..3000)
            .map(|_| {
                let v = Document::Int(1_000_000 + rng.below(1_000_000) as i64);
                let id = db.id_gen().generate();
                crate::txn::WriteOp::Insert("docs".into(), id, object(&[("v", v)]))
            })
            .collect();
        db.write_batch(ops).unwrap();
        db.compact().unwrap();

        let state = db.read().unwrap();
        let root = state.catalog.indexes("docs")[0].root;
        let mut leaves: Vec<usize> = BTreeIndex::new(root)
            .pages(&state.store)
            .unwrap()
            .into_iter()
            .map(|page| SlottedPage::from_bytes(state.store.read_page(page).unwrap()).unwrap())
            .filter(|page| page.page_type() == PageType::IndexLeaf)
            .map(|page| page.iter_cells().count())
            .collect();
        leaves.sort_unstable();
        let full = *leaves.last().unwrap();
        assert!(leaves.len() > 5, "{leaves:?}");
        assert!(leaves[1..].iter().all(|&n| n == full), "{leaves:?}");
    }

    #[test]
    fn an_empty_database_compacts_to_its_header_and_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        docs.insert(Document::Int(1)).unwrap();
        assert!(db.drop_collection("docs").unwrap());
        assert_eq!(db.compact().unwrap().pages_after, 2);
        assert!(db.check().unwrap().is_ok());
    }

    /// A unique index holding a duplicate — damage `check` reports — isn't
    /// compacted into a new file that holds it too. Nothing changes.
    #[test]
    fn a_duplicate_in_a_unique_index_stops_the_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = churned(&path);
        let people = db.collection::<Document>("people");
        let (id, _) = people
            .find_one_with_id(Filter::new().eq("email", "331@people"))
            .unwrap()
            .unwrap();
        // Rewritten in place, past the unique index's check, with
        // another document's email.
        let copy = object(&[
            ("_id", Document::Id(id)),
            ("age", Document::Int(41)),
            ("email", Document::String("332@people".into())),
        ]);
        db.transact(|catalog, store| {
            let meta = *catalog.get("people").unwrap();
            let loc = BTreeIndex::new(meta.index_root)
                .lookup(store, &key::primary(id))?
                .unwrap();
            let mut current = meta.current_data_page;
            assert_eq!(
                data::update_record(store, &mut current, loc, id, &copy)?,
                loc
            );
            Ok(())
        })
        .unwrap();
        drop(people);
        drop(db);
        let bytes = std::fs::read(&path).unwrap();

        let db = Database::open(&path).unwrap();
        match db.compact() {
            Err(crate::Error::DuplicateValue { field, .. }) => assert_eq!(field, "email"),
            other => panic!("expected DuplicateValue, got {other:?}"),
        }
        drop(db);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    /// The compaction's write-back fails twice, after part of the file:
    /// the database is poisoned, and the next open completes it from the
    /// WAL — the file cut to the new length, everything still there.
    #[test]
    fn a_compaction_cut_short_is_completed_by_the_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = churned(&path);
        let before = contents(&db);
        let pages_before = db.file_info().unwrap().pages;
        {
            let mut state = db.state();
            state.store.failing_write_backs = 2;
            state.store.write_back_fails_after = 5;
        }
        assert!(db.compact().is_err());
        assert!(matches!(db.check(), Err(crate::Error::Poisoned)));
        drop(db);
        assert_eq!(file_pages(&path), pages_before, "not cut yet");

        let db = Database::open(&path).unwrap();
        let pages_after = db.file_info().unwrap().pages;
        assert!(pages_after < pages_before);
        assert_eq!(contents(&db), before);
        assert!(db.check().unwrap().is_ok());
        drop(db);
        assert_eq!(file_pages(&path), pages_after, "cut by the recovery");
    }

    /// The rebuilt collection's last data page is its current one: the
    /// next small document goes into the room left there, not onto a
    /// new page.
    #[test]
    fn the_next_insert_uses_the_room_on_the_last_page() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        // About 8 to a page: 2 pages, the second mostly empty.
        let ids: Vec<DocId> = (0..20)
            .map(|_| docs.insert(Document::String("d".repeat(1000))).unwrap())
            .collect();
        for id in &ids[10..] {
            docs.delete(id).unwrap();
        }
        let compacted = db.compact().unwrap();
        assert!(compacted.pages_after < compacted.pages_before);
        docs.insert(Document::String("small".into())).unwrap();
        assert_eq!(db.file_info().unwrap().pages, compacted.pages_after);
        assert!(db.check().unwrap().is_ok());
    }

    #[test]
    fn ids_stay_what_they_were() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let ids: Vec<DocId> = (0..50)
            .map(|i| docs.insert(Document::Int(i)).unwrap())
            .collect();
        for id in &ids[..25] {
            docs.delete(id).unwrap();
        }
        db.compact().unwrap();
        for (i, id) in ids.iter().enumerate().skip(25) {
            assert_eq!(docs.get(id).unwrap(), Some(Document::Int(i as i64)));
        }
    }
}
