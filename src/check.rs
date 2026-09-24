//! `Database::check` and `Database::file_info` (SPEC §39): what a file
//! holds, and whether it's consistent — every page owned by exactly one
//! thing, every document readable, every index agreeing with the
//! documents.

use crate::catalog::{Catalog, IndexMeta};
use crate::collection::secondary_key;
use crate::data;
use crate::database::Database;
use crate::document::{DocId, Document};
use crate::index::{BTreeIndex, Index, key};
use crate::storage::{FileStore, PAGE_SIZE, PageId, RecordLocation};
use std::collections::{BTreeMap, BTreeSet};

/// What the file is, from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// The format version in the header (SPEC §21.2, §33.4).
    pub format_version: u32,
    pub page_size: usize,
    /// Pages in the file, the header included.
    pub pages: u64,
    /// Pages on the free list, ready for reuse.
    pub free_pages: usize,
}

/// What `Database::check` found. `problems` is empty for a consistent
/// file; each entry says what's wrong and where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub collections: usize,
    pub documents: usize,
    pub pages: u64,
    pub problems: Vec<String>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

impl Database {
    /// The header's facts: format, page size, how many pages, how many
    /// of them free.
    pub fn file_info(&self) -> crate::Result<FileInfo> {
        let state = self.read()?;
        Ok(FileInfo {
            format_version: state.store.format_version()?,
            page_size: PAGE_SIZE,
            pages: state.store.page_count(),
            free_pages: state.store.free_pages()?.len(),
        })
    }

    /// Reads the whole file and checks that it's consistent (SPEC §39):
    /// - every page belongs to exactly one thing — the header, the
    ///   catalog, the free list, or one collection's data, overflow or
    ///   index pages — none to two, none to nothing (a leak);
    /// - every document is readable and stored under its own id;
    /// - every index holds exactly one entry per document with an
    ///   indexed value, in order, and a unique index no two equal values.
    ///
    /// Problems are collected, not returned as errors: a damaged index
    /// doesn't stop the rest from being checked. Holds the read lock:
    /// writers wait, readers don't.
    pub fn check(&self) -> crate::Result<CheckReport> {
        let state = self.read()?;
        let (catalog, store) = (&state.catalog, &state.store);
        let mut check = Check {
            owners: BTreeMap::new(),
            page_count: store.page_count(),
            problems: Vec::new(),
        };
        check.claim(0, "the header");
        match Catalog::pages(store) {
            Ok(pages) => pages
                .into_iter()
                .for_each(|p| check.claim(p, "the catalog")),
            Err(e) => check.problem(format!("the catalog: {e}")),
        }
        match store.free_pages() {
            Ok(pages) => pages
                .into_iter()
                .for_each(|p| check.claim(p, "the free list")),
            Err(e) => check.problem(e.to_string()),
        }

        let mut names: Vec<&str> = catalog.names().collect();
        names.sort();
        let mut documents = 0;
        for name in &names {
            match check_collection(&mut check, catalog, store, name) {
                Ok(n) => documents += n,
                Err(e) => check.problem(format!("collection {name:?}: {e}")),
            }
        }

        for page in 0..check.page_count {
            if !check.owners.contains_key(&page) {
                check.problem(format!("page {page} belongs to nothing (leaked)"));
            }
        }
        Ok(CheckReport {
            collections: names.len(),
            documents,
            pages: check.page_count,
            problems: check.problems,
        })
    }
}

struct Check {
    owners: BTreeMap<PageId, String>,
    page_count: u64,
    problems: Vec<String>,
}

impl Check {
    fn problem(&mut self, problem: String) {
        self.problems.push(problem);
    }

    fn claim(&mut self, page: PageId, owner: &str) {
        if page >= self.page_count {
            self.problem(format!("{owner} uses page {page}, past the file's end"));
        } else if let Some(other) = self.owners.get(&page) {
            let problem = format!("page {page} belongs to both {other} and {owner}");
            self.problem(problem);
        } else {
            self.owners.insert(page, owner.to_string());
        }
    }
}

/// Checks one collection, claiming its pages; returns how many documents
/// it has. An `Err` is a problem that stopped the check of this
/// collection partway.
fn check_collection(
    check: &mut Check,
    catalog: &Catalog,
    store: &FileStore,
    name: &str,
) -> std::io::Result<usize> {
    let meta = *catalog.get(name).expect("a listed collection");
    let primary = BTreeIndex::new(meta.index_root);
    for page in primary.pages(store)? {
        check.claim(page, &format!("{name:?}'s primary index"));
    }
    let entries = primary.scan(store)?;
    check_order(check, name, "primary index", &entries);

    let mut documents: Vec<(DocId, Document, RecordLocation)> = Vec::new();
    for (key, loc) in &entries {
        let expected = key::doc_id(key);
        match data::get_record(store, *loc) {
            Ok((id, doc)) if id == expected => documents.push((id, doc, *loc)),
            Ok((id, _)) => check.problem(format!(
                "{name:?}: the primary index files document {expected} at page {} slot {}, \
                 which holds document {id}",
                loc.page, loc.slot
            )),
            Err(e) => check.problem(format!("{name:?}: document {expected} can't be read: {e}")),
        }
    }

    let locs = documents.iter().map(|(_, _, loc)| *loc);
    let (data_pages, chain_pages) = data::collection_pages(store, meta.current_data_page, locs)?;
    for page in data_pages {
        check.claim(page, &format!("{name:?}'s documents"));
    }
    for page in chain_pages {
        check.claim(page, &format!("{name:?}'s large documents"));
    }

    for index in catalog.indexes(name) {
        if let Err(e) = check_index(check, store, name, index, &documents) {
            check.problem(format!("{name:?}'s index on {:?}: {e}", index.field));
        }
    }
    Ok(documents.len())
}

/// A secondary index against the documents: its pages, its order, one
/// entry per document with an indexed value and nothing else, and for a
/// unique one no two equal non-null values.
fn check_index(
    check: &mut Check,
    store: &FileStore,
    name: &str,
    index: &IndexMeta,
    documents: &[(DocId, Document, RecordLocation)],
) -> std::io::Result<()> {
    let what = format!("{name:?}'s index on {:?}", index.field);
    let tree = BTreeIndex::new(index.root);
    for page in tree.pages(store)? {
        check.claim(page, &what);
    }
    let entries = tree.scan(store)?;
    check_order(
        check,
        name,
        &format!("index on {:?}", index.field),
        &entries,
    );

    let found: BTreeSet<(Vec<u8>, PageId, u16)> = entries
        .into_iter()
        .map(|(key, loc)| (key, loc.page, loc.slot))
        .collect();
    let expected: BTreeSet<(Vec<u8>, PageId, u16)> = documents
        .iter()
        .filter_map(|(id, doc, loc)| {
            Some((secondary_key(doc, &index.field, *id)?, loc.page, loc.slot))
        })
        .collect();
    for (key, _, _) in expected.difference(&found) {
        check.problem(format!("{what} lacks document {}", key::doc_id(key)));
    }
    for (key, _, _) in found.difference(&expected) {
        check.problem(format!(
            "{what} has an entry for document {} that doesn't match it",
            key::doc_id(key)
        ));
    }

    if index.unique {
        // Equal values share a key's value part (SPEC §28.1): compare
        // within each group.
        let mut groups: BTreeMap<Vec<u8>, Vec<(DocId, &Document)>> = BTreeMap::new();
        for (id, doc, _) in documents {
            let value = crate::query::value_or_null(doc, &index.field);
            if matches!(value, Document::Null) {
                continue;
            }
            if let Some(encoded) = key::encode_value(value) {
                groups.entry(encoded).or_default().push((*id, value));
            }
        }
        for group in groups.values() {
            for (i, (a, a_value)) in group.iter().enumerate() {
                if let Some((b, _)) = group[i + 1..]
                    .iter()
                    .find(|(_, b_value)| crate::query::equal(a_value, b_value))
                {
                    check.problem(format!(
                        "{what} is unique, but documents {a} and {b} share a value"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// An index scan must come back in strictly ascending key order.
fn check_order(check: &mut Check, name: &str, what: &str, entries: &[(Vec<u8>, RecordLocation)]) {
    if let Some(pair) = entries.windows(2).find(|pair| pair[0].0 >= pair[1].0) {
        check.problem(format!(
            "{name:?}'s {what} is out of order at document {}",
            key::doc_id(&pair[1].0)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Filter;
    use crate::storage::PageStore;

    fn object(pairs: &[(&str, Document)]) -> Document {
        Document::Object(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// Two collections with indexes, a unique one among them, a large
    /// document in overflow pages, and deletes that free pages.
    fn sample(dir: &tempfile::TempDir) -> Database {
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        for name in ["people", "notes"] {
            let docs = db.collection::<Document>(name);
            docs.ensure_index("age").unwrap();
            docs.ensure_unique_index("email").unwrap();
            for i in 0..120 {
                let pad = if i == 3 { 50_000 } else { i * 53 % 2000 };
                docs.insert(object(&[
                    ("age", Document::Int(i as i64 % 30)),
                    ("email", Document::String(format!("{i}@x"))),
                    ("pad", Document::String("p".repeat(pad))),
                ]))
                .unwrap();
            }
            docs.delete_many(Filter::new().lt("age", 5)).unwrap();
        }
        db
    }

    fn problems(db: &Database) -> Vec<String> {
        db.check().unwrap().problems
    }

    fn assert_found(db: &Database, needle: &str) {
        let problems = problems(db);
        assert!(
            problems.iter().any(|p| p.contains(needle)),
            "{needle:?} not in {problems:#?}"
        );
    }

    #[test]
    fn a_consistent_file_has_no_problems() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let report = db.check().unwrap();
        assert_eq!(report.problems, Vec::<String>::new());
        assert_eq!((report.collections, report.documents), (2, 200));
        let info = db.file_info().unwrap();
        assert_eq!((info.format_version, info.page_size), (5, PAGE_SIZE));
        assert_eq!(info.pages, report.pages);
        assert!(info.free_pages > 0, "the deletes freed pages");

        drop(db);
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        assert!(db.check().unwrap().is_ok());
        db.drop_collection("notes").unwrap();
        assert!(db.check().unwrap().is_ok());
    }

    #[test]
    fn a_page_nothing_owns_is_a_leak() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let leaked = db.transact(|_, store| Ok(store.allocate_page()?)).unwrap();
        assert_found(&db, &format!("page {leaked} belongs to nothing"));
    }

    #[test]
    fn an_index_missing_an_entry_or_with_a_wrong_one_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let people = db.collection::<Document>("people");
        let (id, doc) = people
            .find_one_with_id(Filter::new().eq("age", 7))
            .unwrap()
            .unwrap();
        db.transact(|catalog, store| {
            let index = &catalog.indexes("people")[0];
            let mut tree = BTreeIndex::new(index.root);
            tree.remove(store, &secondary_key(&doc, "age", id).unwrap())?;
            // And an entry claiming the document holds 99.
            let loc = BTreeIndex::new(catalog.get("people").unwrap().index_root)
                .lookup(store, &key::primary(id))?
                .unwrap();
            tree.insert(store, &key::secondary(&Document::Int(99), id).unwrap(), loc)?;
            Ok(())
        })
        .unwrap();
        assert_found(&db, &format!("index on \"age\" lacks document {id}"));
        assert_found(&db, &format!("entry for document {id} that doesn't match"));
    }

    #[test]
    fn a_page_with_two_owners_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        // Make `notes` insert into a page full of `people`'s documents.
        // (Collections are checked in name order: `notes` claims it first.)
        let people = db.collection::<Document>("people");
        let (id, _) = people
            .find_one_with_id(Filter::new().eq("age", 20))
            .unwrap()
            .unwrap();
        let page = db
            .transact(|catalog, store| {
                let root = catalog.get("people").unwrap().index_root;
                let loc = BTreeIndex::new(root)
                    .lookup(store, &key::primary(id))?
                    .unwrap();
                catalog.set_current_data_page(store, "notes", loc.page)?;
                Ok(loc.page)
            })
            .unwrap();
        assert_found(
            &db,
            &format!(
                "page {page} belongs to both \"notes\"'s documents and \"people\"'s documents"
            ),
        );
    }

    #[test]
    fn a_duplicate_in_a_unique_index_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let people = db.collection::<Document>("people");
        let (id, _) = people
            .find_one_with_id(Filter::new().eq("email", "7@x"))
            .unwrap()
            .unwrap();
        // Rewrite the document in place, past the index's check.
        let copy = object(&[
            ("_id", Document::Id(id)),
            ("age", Document::Int(8)),
            ("email", Document::String("8@x".into())),
        ]);
        db.transact(|catalog, store| {
            let meta = *catalog.get("people").unwrap();
            let loc = BTreeIndex::new(meta.index_root)
                .lookup(store, &key::primary(id))?
                .unwrap();
            let mut current = meta.current_data_page;
            let new_loc = data::update_record(store, &mut current, loc, id, &copy)?;
            assert_eq!(new_loc, loc, "it shrank, so it stays in place");
            Ok(())
        })
        .unwrap();
        assert_found(&db, "is unique, but documents");
        assert_found(&db, &format!("lacks document {id}"));
    }
}
