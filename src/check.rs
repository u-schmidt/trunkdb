//! `Database::check` and `Database::file_info` (SPEC §39): what a file
//! holds, and whether it's consistent — every page owned by exactly one
//! thing, every document readable, every index agreeing with the
//! documents.

use crate::catalog::{Catalog, IndexMeta};
use crate::collection::{index_keys, same_values, unique_tuples};
use crate::data;
use crate::database::{Database, ReadGuard};
use crate::document::{DocId, Document};
use crate::index::{BTreeIndex, Index, KeyRange, key};
use crate::storage::{PAGE_SIZE, PageId, RecordLocation, SnapshotStore};
use std::collections::BTreeSet;

/// What the file is, from its header.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
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
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
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
        Self::file_info_of(&self.read()?)
    }

    /// `file_info`, of the commit `state` reads.
    pub(crate) fn file_info_of(state: &ReadGuard<'_>) -> crate::Result<FileInfo> {
        let store = state.snapshot().store;
        Ok(FileInfo {
            format_version: store.format_version()?,
            page_size: PAGE_SIZE,
            pages: store.page_count(),
            free_pages: store.free_pages()?.len(),
        })
    }

    /// Reads the whole file and checks that it's consistent (SPEC §39):
    /// - every page's checksum matches its bytes (SPEC §40);
    /// - every page belongs to exactly one thing — the header, the
    ///   catalog, the free list, or one collection's data, overflow or
    ///   index pages — none to two, none to nothing (a leak);
    /// - every document is readable and stored under its own id;
    /// - every index holds exactly one entry per document with an
    ///   indexed value, in order, and a unique index no two equal values.
    ///
    /// Problems are collected, not returned as errors: a damaged index
    /// doesn't stop the rest from being checked. A read like any other
    /// on one snapshot (SPEC §80): reads and writes go on beside it.
    pub fn check(&self) -> crate::Result<CheckReport> {
        Self::check_of(&self.read()?)
    }

    /// `check`, of the commit `state` reads.
    ///
    /// In memory that doesn't grow with the file (SPEC §109): documents
    /// are read and checked one at a time, and an index is checked entry
    /// by entry against the documents its entries name. Only a page's
    /// owner is kept for every page, four bytes of it.
    pub(crate) fn check_of(state: &ReadGuard<'_>) -> crate::Result<CheckReport> {
        let at = state.snapshot();
        let (catalog, store) = (at.catalog, &at.store);
        let page_count = store.page_count();
        let mut check = Check {
            owners: vec![NOBODY; page_count as usize],
            names: Vec::new(),
            page_count,
            problems: Vec::new(),
            incomplete: false,
        };
        // First, so a damaged page is named as such before whatever
        // couldn't be read because of it (SPEC §40.4).
        let damaged: BTreeSet<PageId> = match store.damaged_pages() {
            Ok(pages) => pages.into_iter().collect(),
            Err(e) => {
                check.unreadable(e.to_string());
                BTreeSet::new()
            }
        };
        for page in &damaged {
            check.unreadable(format!(
                "page {page} is damaged: its checksum doesn't match"
            ));
        }
        let header = check.owner("the header".to_string());
        check.claim(0, header);
        let catalog_pages = check.owner("the catalog".to_string());
        match Catalog::pages(store) {
            Ok(pages) => pages
                .into_iter()
                .for_each(|p| check.claim(p, catalog_pages)),
            Err(e) => check.unreadable(format!("the catalog: {e}")),
        }
        let free_list = check.owner("the free list".to_string());
        match store.free_pages() {
            Ok(pages) => pages.into_iter().for_each(|p| check.claim(p, free_list)),
            Err(e) => check.unreadable(e.to_string()),
        }

        let mut names: Vec<&str> = catalog.names().collect();
        names.sort();
        let mut documents = 0;
        for name in &names {
            match check_collection(&mut check, catalog, store, name, &damaged) {
                Ok(n) => documents += n,
                Err(e) => check.unreadable(format!("collection {name:?}: {e}")),
            }
        }

        if check.incomplete {
            // Whatever couldn't be read may own pages: each would look
            // leaked.
            check.problem("leaks not checked: not everything could be read".to_string());
        } else {
            for page in 0..check.page_count {
                if check.owners[page as usize] == NOBODY {
                    check.problem(format!("page {page} belongs to nothing (leaked)"));
                }
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

/// Who owns a page, as `Check::owner` numbered them.
type Owner = u32;

/// A page nobody has claimed yet.
const NOBODY: Owner = Owner::MAX;

struct Check {
    /// Every page's owner, by page id (SPEC §109): four bytes a page,
    /// where a name each took tens.
    owners: Vec<Owner>,
    /// The owners' names, by number.
    names: Vec<String>,
    page_count: u64,
    problems: Vec<String>,
    /// Something couldn't be read, so the pages it owns are unknown.
    incomplete: bool,
}

impl Check {
    fn problem(&mut self, problem: String) {
        self.problems.push(problem);
    }

    /// A problem that leaves part of the file unread.
    fn unreadable(&mut self, problem: String) {
        self.incomplete = true;
        self.problem(problem);
    }

    /// A new owner of pages, by its name in problems.
    fn owner(&mut self, name: String) -> Owner {
        self.names.push(name);
        (self.names.len() - 1) as Owner
    }

    /// `page` is `owner`'s alone: a second claim is a problem, even by
    /// the same owner.
    fn claim(&mut self, page: PageId, owner: Owner) {
        self.claim_as(page, owner, false);
    }

    /// `page` is `owner`'s, which may claim it again: a data page holds
    /// many documents.
    fn claim_shared(&mut self, page: PageId, owner: Owner) {
        self.claim_as(page, owner, true);
    }

    fn claim_as(&mut self, page: PageId, owner: Owner, shared: bool) {
        if page >= self.page_count {
            let problem = format!(
                "{} uses page {page}, past the file's end",
                self.names[owner as usize]
            );
            self.problem(problem);
            return;
        }
        let other = self.owners[page as usize];
        if other == NOBODY {
            self.owners[page as usize] = owner;
        } else if !(shared && other == owner) {
            let problem = format!(
                "page {page} belongs to both {} and {}",
                self.names[other as usize], self.names[owner as usize]
            );
            self.problem(problem);
        }
    }
}

/// Checks one collection, claiming its pages; returns how many documents
/// it has. An `Err` is a problem that stopped the check of this
/// collection partway. Documents on `damaged` pages aren't read: one
/// problem names them all, and their index entries are left alone.
///
/// The documents are read one at a time, in id order, and let go: what
/// each index should hold is counted, not kept (SPEC §109).
fn check_collection(
    check: &mut Check,
    catalog: &Catalog,
    store: &SnapshotStore,
    name: &str,
    damaged: &BTreeSet<PageId>,
) -> std::io::Result<usize> {
    let meta = *catalog.get(name).expect("a listed collection");
    let primary = BTreeIndex::new(meta.index_root);
    let primary_pages = check.owner(format!("{name:?}'s primary index"));
    for page in primary.pages(store)? {
        check.claim(page, primary_pages);
    }
    let data_pages = check.owner(format!("{name:?}'s documents"));
    let chain_pages = check.owner(format!("{name:?}'s large documents"));
    let indexes = catalog.indexes(name);
    // How many entries the documents call for, index by index.
    let mut expected = vec![0usize; indexes.len()];

    let mut documents = 0;
    let mut unreadable = BTreeSet::new();
    let (mut on_damaged, mut on_damaged_count) = (BTreeSet::new(), 0);
    let mut records = data::Records::new(store);
    let mut order = Order::new(format!("{name:?}'s primary index"));
    let mut walk = primary.walk(store, KeyRange::everything(), false);
    while let Some(entry) = walk.step() {
        let (key, loc) = entry?;
        order.see(check, key);
        let expected_id = key::doc_id(key);
        if damaged.contains(&loc.page) {
            unreadable.insert(expected_id);
            on_damaged.insert(loc.page);
            on_damaged_count += 1;
            continue;
        }
        match records.get(loc) {
            Ok((id, doc)) if id == expected_id => {
                // A file says format 12 once it is open, and so that
                // every document is an object (SPEC §95).
                if !matches!(doc, Document::Object(_)) {
                    check.problem(format!(
                        "{name:?}: document {id} isn't an object, which the file's format promises"
                    ));
                }
                documents += 1;
                let (pages, chain) = data::collection_pages(store, 0, [loc])?;
                pages
                    .into_iter()
                    .for_each(|p| check.claim_shared(p, data_pages));
                chain.into_iter().for_each(|p| check.claim(p, chain_pages));
                for (expected, index) in expected.iter_mut().zip(indexes) {
                    *expected += index_keys(&doc, index, id).len();
                }
            }
            Ok((id, _)) => check.problem(format!(
                "{name:?}: the primary index files document {expected_id} at page {} slot {}, \
                 which holds document {id}",
                loc.page, loc.slot
            )),
            Err(e) => {
                unreadable.insert(expected_id);
                check.unreadable(format!(
                    "{name:?}: document {expected_id} can't be read: {e}"
                ));
            }
        }
    }
    if !on_damaged.is_empty() {
        let pages: Vec<String> = on_damaged.iter().map(PageId::to_string).collect();
        check.unreadable(format!(
            "{name:?}: {} can't be read, on damaged {} {}",
            count(on_damaged_count, "document"),
            if pages.len() == 1 { "page" } else { "pages" },
            pages.join(", ")
        ));
    }

    // A damaged current page is already reported, and holds nothing
    // else to find.
    if meta.current_data_page != 0 && !damaged.contains(&meta.current_data_page) {
        let (pages, _) = data::collection_pages(store, meta.current_data_page, [])?;
        pages
            .into_iter()
            .for_each(|p| check.claim_shared(p, data_pages));
    }

    for (index, expected) in indexes.iter().zip(expected) {
        let checked = check_index(check, store, name, &primary, index, expected, &unreadable);
        if let Err(e) = checked {
            check.unreadable(format!("{name:?}'s index on {:?}: {e}", index.name()));
        }
    }
    Ok(documents)
}

/// A secondary index against the documents: its pages, its order, one
/// entry per document with an indexed value and nothing else, and for a
/// unique one no two equal non-null values. Entries for `unreadable`
/// documents are skipped: there's nothing to compare them with.
///
/// Entry by entry (SPEC §109): an entry is right if its document is where
/// the primary index has it, and the document gives the entry's key.
/// Right entries are distinct, as the index is in order, so as many of
/// them as the documents call for (`expected`) is all of them. Only if
/// there are fewer, or the order is broken, does a second pass go
/// through the documents to name what's missing (`name_missing`).
///
/// A unique index's equal values have equal value parts (SPEC §28.1), so
/// their entries are neighbors: each run of them is compared by value.
fn check_index(
    check: &mut Check,
    store: &SnapshotStore,
    name: &str,
    primary: &BTreeIndex,
    index: &IndexMeta,
    expected: usize,
    unreadable: &BTreeSet<DocId>,
) -> std::io::Result<()> {
    let what = format!("{name:?}'s index on {:?}", index.name());
    let tree = BTreeIndex::new(index.root);
    let owner = check.owner(what.clone());
    for page in tree.pages(store)? {
        check.claim(page, owner);
    }

    let mut order = Order::new(format!("{name:?}'s index on {:?}", index.name()));
    let mut records = data::Records::new(store);
    let mut right = 0;
    let mut reported = BTreeSet::new();
    // The run of right entries with one value part, of a unique index.
    let mut run: Vec<(DocId, RecordLocation)> = Vec::new();
    let mut run_part: Vec<u8> = Vec::new();
    let mut walk = tree.walk(store, KeyRange::everything(), false);
    while let Some(entry) = walk.step() {
        let (key, loc) = entry?;
        order.see(check, key);
        let id = key::doc_id(key);
        if unreadable.contains(&id) {
            continue;
        }
        if !entry_is_right(store, &mut records, primary, index, key, loc)? {
            check.problem(format!(
                "{what} has an entry for document {id} that doesn't match it"
            ));
            continue;
        }
        right += 1;
        if index.unique {
            if key::value_part(key) != run_part.as_slice() {
                same_in_run(check, store, &what, index, &run_part, &run, &mut reported)?;
                run.clear();
                run_part.clear();
                run_part.extend_from_slice(key::value_part(key));
            }
            run.push((id, loc));
        }
    }
    same_in_run(check, store, &what, index, &run_part, &run, &mut reported)?;

    if right < expected || order.broken {
        name_missing(
            check,
            store,
            primary,
            &tree,
            index,
            &what,
            unreadable,
            &mut reported,
        )?;
    }
    Ok(())
}

/// Whether an index entry is right: its document where the primary index
/// has it, under its own id, and giving the entry's key.
fn entry_is_right(
    store: &SnapshotStore,
    records: &mut data::Records,
    primary: &BTreeIndex,
    index: &IndexMeta,
    key: &[u8],
    loc: RecordLocation,
) -> std::io::Result<bool> {
    let id = key::doc_id(key);
    if primary.lookup(store, &key::primary(id))? != Some(loc) {
        return Ok(false);
    }
    Ok(match records.get(loc) {
        Ok((at, doc)) => at == id && index_keys(&doc, index, id).iter().any(|k| k == key),
        Err(_) => false,
    })
}

/// A run of a unique index's entries with one value part, compared by
/// value: two documents with equal values are a problem, reported once
/// for each pair (`reported`).
fn same_in_run(
    check: &mut Check,
    store: &SnapshotStore,
    what: &str,
    index: &IndexMeta,
    value_part: &[u8],
    run: &[(DocId, RecordLocation)],
    reported: &mut BTreeSet<(DocId, DocId)>,
) -> std::io::Result<()> {
    if run.len() < 2 {
        return Ok(());
    }
    let mut records = data::Records::new(store);
    let mut seen: Vec<(DocId, Vec<Document>)> = Vec::new();
    for (id, loc) in run {
        let (_, doc) = records.get(*loc)?;
        for (part, values) in unique_tuples(&doc, index) {
            if part == value_part {
                seen.push((*id, values.into_iter().cloned().collect()));
            }
        }
    }
    report_equal_values(check, what, &seen, reported);
    Ok(())
}

/// Of documents' values under one value part, in id order, each pair of
/// two documents with equal values, reported once (SPEC §42.2: values of
/// two documents, not two elements of one).
fn report_equal_values(
    check: &mut Check,
    what: &str,
    seen: &[(DocId, Vec<Document>)],
    reported: &mut BTreeSet<(DocId, DocId)>,
) {
    for (i, (a, a_values)) in seen.iter().enumerate() {
        let a_values: Vec<&Document> = a_values.iter().collect();
        let equal = seen[i + 1..].iter().find(|(b, b_values)| {
            let b_values: Vec<&Document> = b_values.iter().collect();
            b != a && same_values(&a_values, &b_values)
        });
        if let Some((b, _)) = equal
            && reported.insert((*a.min(b), *a.max(b)))
        {
            check.problem(format!(
                "{what} is unique, but documents {a} and {b} share a value"
            ));
        }
    }
}

/// The second pass, for an index that isn't all it should be: every
/// document's keys looked up, each one missing named; and for a unique
/// index, each document's values compared with those of the entries
/// under the same value part, which the run of entries alone can't see
/// when an entry is wrong or missing.
#[allow(clippy::too_many_arguments)]
fn name_missing(
    check: &mut Check,
    store: &SnapshotStore,
    primary: &BTreeIndex,
    tree: &BTreeIndex,
    index: &IndexMeta,
    what: &str,
    unreadable: &BTreeSet<DocId>,
    reported: &mut BTreeSet<(DocId, DocId)>,
) -> std::io::Result<()> {
    let mut records = data::Records::new(store);
    let mut others = data::Records::new(store);
    let mut walk = primary.walk(store, KeyRange::everything(), false);
    while let Some(entry) = walk.step() {
        let (key, loc) = entry?;
        let id = key::doc_id(key);
        if unreadable.contains(&id) {
            continue;
        }
        let doc = match records.get(loc) {
            Ok((at, doc)) if at == id => doc,
            // Already reported, as the primary index was walked.
            _ => continue,
        };
        for key in index_keys(&doc, index, id) {
            if tree.lookup(store, &key)? != Some(loc) {
                check.problem(format!("{what} lacks document {}", key::doc_id(&key)));
            }
        }
        if !index.unique {
            continue;
        }
        for (part, values) in unique_tuples(&doc, index) {
            let mut seen = vec![(id, values.into_iter().cloned().collect::<Vec<_>>())];
            let mut under = tree.walk(store, KeyRange::prefixed(&part), false);
            while let Some(entry) = under.step() {
                let (key, at) = entry?;
                let other = key::doc_id(key);
                if key::value_part(key) != part.as_slice() || other == id {
                    continue;
                }
                if let Ok((found, doc)) = others.get(at)
                    && found == other
                {
                    for (other_part, values) in unique_tuples(&doc, index) {
                        if other_part == part {
                            seen.push((other, values.into_iter().cloned().collect()));
                        }
                    }
                }
            }
            seen.sort_by_key(|(id, _)| *id);
            report_equal_values(check, what, &seen, reported);
        }
    }
    Ok(())
}

/// `1 document`, `2 documents`.
fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// An index walk must come in strictly ascending key order: the first
/// key that doesn't is a problem, the rest of the walk goes on.
struct Order {
    what: String,
    last: Option<Vec<u8>>,
    broken: bool,
}

impl Order {
    fn new(what: String) -> Self {
        Order {
            what,
            last: None,
            broken: false,
        }
    }

    fn see(&mut self, check: &mut Check, key: &[u8]) {
        if let Some(last) = &mut self.last {
            if !self.broken && last.as_slice() >= key {
                self.broken = true;
                check.problem(format!(
                    "{} is out of order at document {}",
                    self.what,
                    key::doc_id(key)
                ));
            }
            last.clear();
            last.extend_from_slice(key);
        } else {
            self.last = Some(key.to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Filter;
    use crate::storage::{PageStore, PageType};

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
            docs.ensure_index_with("email", crate::IndexOptions::new().unique())
                .unwrap();
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

    /// A byte changed on disk in a data page, a collection's current data
    /// page, then an index page (the first leaf, a primary index's):
    /// the damaged page is the first problem, named by id; then what
    /// couldn't be read because of it — for a data page its documents,
    /// all in one problem, and no index entries or pages blamed for
    /// them; leaks aren't checked. With the byte restored, the file
    /// checks clean.
    #[test]
    fn a_damaged_page_is_the_first_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        let db = sample(&dir);
        let current = db
            .read()
            .unwrap()
            .catalog()
            .get("notes")
            .unwrap()
            .current_data_page;
        drop(db);
        let clean = std::fs::read(&path).unwrap();
        let first_of = |kind: PageType| {
            (1..clean.len() / PAGE_SIZE)
                .find(|&page| clean[page * PAGE_SIZE] == kind as u8)
                .unwrap()
        };
        let data_page = first_of(PageType::Data);
        let index_page = first_of(PageType::IndexLeaf);

        for (page, then) in [
            (data_page, "documents can't be read, on damaged page"),
            // One `collection_pages` would otherwise read, and fail on.
            (current as usize, "documents can't be read, on damaged page"),
            (
                index_page,
                "is damaged: its checksum doesn't match its bytes",
            ),
        ] {
            let mut bytes = clean.clone();
            bytes[page * PAGE_SIZE + 500] ^= 0xFF;
            std::fs::write(&path, &bytes).unwrap();

            let problems = problems(&Database::open(&path).unwrap());
            assert_eq!(problems.len(), 3, "{problems:#?}");
            assert_eq!(
                problems[0],
                format!("page {page} is damaged: its checksum doesn't match")
            );
            assert!(problems[1].contains(then), "{problems:#?}");
            assert!(problems[1].contains(&page.to_string()), "{problems:#?}");
            assert_eq!(
                problems[2],
                "leaks not checked: not everything could be read"
            );
        }
        std::fs::write(&path, &clean).unwrap();
        assert!(problems(&Database::open(&path).unwrap()).is_empty());
    }

    #[test]
    fn a_consistent_file_has_no_problems() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let report = db.check().unwrap();
        assert_eq!(report.problems, Vec::<String>::new());
        assert_eq!((report.collections, report.documents), (2, 200));
        let info = db.file_info().unwrap();
        assert_eq!((info.format_version, info.page_size), (12, PAGE_SIZE));
        assert_eq!(info.pages, report.pages);
        assert!(info.free_pages > 0, "the deletes freed pages");

        drop(db);
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        assert!(db.check().unwrap().is_ok());
        db.drop_collection("notes").unwrap();
        assert!(db.check().unwrap().is_ok());
    }

    /// A consistent file is checked in one pass over the documents and
    /// one over each index, which reads each entry's document (SPEC
    /// §109): each document read once, and once more per index. The
    /// second pass, which names what's missing, doesn't run.
    #[test]
    fn a_consistent_file_reads_each_document_once_and_once_per_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        crate::data::RECORDS_READ.with(|n| n.set(0));
        let report = db.check().unwrap();
        let read = crate::data::RECORDS_READ.with(|n| n.get());
        assert!(report.is_ok(), "{report:#?}");
        assert_eq!(report.documents, 200);
        assert_eq!(read, 200 * (1 + 2));
    }

    /// An entry with the right key, but at another document's place: it
    /// doesn't match, and the document it should point to lacks one.
    #[test]
    fn an_entry_at_another_documents_place_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let people = db.collection::<Document>("people");
        let (id, doc) = people
            .find_one_with_id(Filter::new().eq("age", 7))
            .unwrap()
            .unwrap();
        let (other, _) = people
            .find_one_with_id(Filter::new().eq("age", 8))
            .unwrap()
            .unwrap();
        db.transact(|catalog, store| {
            let index = &catalog.indexes("people")[0];
            let mut tree = BTreeIndex::new(index.root);
            let key = key::secondary(crate::query::value_or_null(&doc, "age"), id).unwrap();
            tree.remove(store, &key)?;
            let elsewhere = BTreeIndex::new(catalog.get("people").unwrap().index_root)
                .lookup(store, &key::primary(other))?
                .unwrap();
            tree.insert(store, &key, elsewhere)?;
            Ok(())
        })
        .unwrap();
        let problems = problems(&db);
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert_found(&db, &format!("entry for document {id} that doesn't match"));
        assert_found(&db, &format!("index on \"age\" lacks document {id}"));
    }

    /// An entry with its document's key, at a copy of the document that
    /// the primary index doesn't have: it doesn't match, and the
    /// document lacks its entry — though the copy has the same id and the
    /// same values.
    #[test]
    fn an_entry_at_a_stale_copy_of_its_document_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let people = db.collection::<Document>("people");
        let (id, doc) = people
            .find_one_with_id(Filter::new().eq("age", 7))
            .unwrap()
            .unwrap();
        db.transact(|catalog, store| {
            let meta = *catalog.get("people").unwrap();
            let mut current = meta.current_data_page;
            let free = catalog.free_space_mut("people");
            let copy = data::insert_record(store, &mut current, free, id, &doc)?;
            let index = &catalog.indexes("people")[0];
            let mut tree = BTreeIndex::new(index.root);
            let key = key::secondary(crate::query::value_or_null(&doc, "age"), id).unwrap();
            tree.remove(store, &key)?;
            tree.insert(store, &key, copy)?;
            Ok(())
        })
        .unwrap();
        let problems = problems(&db);
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert_found(&db, &format!("entry for document {id} that doesn't match"));
        assert_found(&db, &format!("index on \"age\" lacks document {id}"));
    }

    /// An index entry changed on its leaf to a value that sorts after the
    /// next one: the index is out of order there, and the second pass
    /// names the document as missing.
    #[test]
    fn an_index_out_of_order_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = sample(&dir);
        let people = db.collection::<Document>("people");
        let (id, _) = people
            .find_one_with_id(Filter::new().eq("age", 7))
            .unwrap()
            .unwrap();
        let (was, now) = (
            key::secondary(&Document::Int(7), id).unwrap(),
            key::secondary(&Document::Int(1_000), id).unwrap(),
        );
        assert_eq!(was.len(), now.len());
        db.transact(|catalog, store| {
            let root = catalog.indexes("people")[0].root;
            for page_id in BTreeIndex::new(root).pages(store)? {
                let mut page = store.read_page(page_id)?;
                let bytes = page.make_mut();
                if let Some(at) = bytes.windows(was.len()).position(|w| w == was) {
                    bytes[at..at + now.len()].copy_from_slice(&now);
                    store.write_page(page_id, page)?;
                }
            }
            Ok(())
        })
        .unwrap();
        assert_found(&db, "index on \"age\" is out of order at document");
        assert_found(&db, &format!("entry for document {id} that doesn't match"));
        assert_found(&db, &format!("index on \"age\" lacks document {id}"));
    }

    /// Two large documents of one collection on one chain of overflow
    /// pages: each of its pages is claimed twice by the same owner, which
    /// a data page may be and an overflow page may not.
    #[test]
    fn an_overflow_chain_under_two_documents_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let docs = db.collection::<Document>("docs");
        let big = |c: &str| object(&[("pad", Document::String(c.repeat(20_000)))]);
        let a = docs.insert(big("a")).unwrap();
        let b = docs.insert(big("b")).unwrap();
        db.transact(|catalog, store| {
            let meta = *catalog.get("docs").unwrap();
            let primary = BTreeIndex::new(meta.index_root);
            let cell = |store: &dyn PageStore, id: DocId| -> std::io::Result<Vec<u8>> {
                let loc = primary.lookup(store, &key::primary(id))?.unwrap();
                let page = crate::storage::SlottedPage::from_bytes(store.read_page(loc.page)?)?;
                Ok(page.get_cell(loc.slot).unwrap().to_vec())
            };
            let (cell_a, cell_b) = (cell(store, a)?, cell(store, b)?);
            assert_eq!(cell_a.len(), cell_b.len(), "two overflow cells");
            // A's cell, B's id: B's cell, pointing to A's chain.
            let mut shared = cell_a.clone();
            let at = shared.windows(16).position(|w| w == a.0).unwrap();
            shared[at..at + 16].copy_from_slice(&b.0);
            let loc = primary.lookup(store, &key::primary(b))?.unwrap();
            let mut page = store.read_page(loc.page)?;
            let bytes = page.make_mut();
            let at = bytes
                .windows(cell_b.len())
                .position(|w| w == cell_b)
                .unwrap();
            bytes[at..at + shared.len()].copy_from_slice(&shared);
            Ok(store.write_page(loc.page, page)?)
        })
        .unwrap();
        assert_found(
            &db,
            "belongs to both \"docs\"'s large documents and \"docs\"'s large documents",
        );
    }

    /// A duplicate in a unique index that holds both documents rightly is
    /// seen by the walk of the index, and again by the second pass, which
    /// another document's missing entry sets off: it's reported once.
    #[test]
    fn a_duplicate_seen_by_both_passes_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let long = |end: &str| Document::String("x".repeat(1200) + end);
        let aliases = |names: Vec<Document>| object(&[("aliases", Document::Array(names))]);
        users
            .ensure_index_with("aliases[*]", crate::IndexOptions::new().unique())
            .unwrap();
        users.insert(aliases(vec![long("b")])).unwrap();
        let id = users.insert(aliases(vec![long("a"), long("c")])).unwrap();
        let other = users
            .insert(aliases(vec![Document::String("z".into())]))
            .unwrap();
        let copy = object(&[
            ("_id", Document::Id(id)),
            ("aliases", Document::Array(vec![long("a"), long("b")])),
        ]);
        db.transact(|catalog, store| {
            let meta = *catalog.get("users").unwrap();
            let loc = BTreeIndex::new(meta.index_root)
                .lookup(store, &key::primary(id))?
                .unwrap();
            let mut current = meta.current_data_page;
            let free = catalog.free_space_mut("users");
            assert_eq!(
                data::update_record(store, &mut current, free, loc, id, &copy)?,
                loc
            );
            let index = &catalog.indexes("users")[0];
            let key = key::secondary(&Document::String("z".into()), other).unwrap();
            BTreeIndex::new(index.root).remove(store, &key)?;
            Ok(())
        })
        .unwrap();
        let problems = problems(&db);
        assert_eq!(problems.len(), 2, "{problems:#?}");
        assert_found(&db, "is unique, but documents");
        assert_found(&db, &format!("lacks document {other}"));
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
            tree.remove(
                store,
                &key::secondary(crate::query::value_or_null(&doc, "age"), id).unwrap(),
            )?;
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

    /// In a unique multikey index, a duplicate hiding behind a shared key:
    /// one document's `…a` and `…b` (one entry, the key cut, SPEC §28.1),
    /// another's `…b`. Each value counts, not each key.
    #[test]
    fn a_duplicate_among_elements_under_a_shared_key_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.trunkdb")).unwrap();
        let users = db.collection::<Document>("users");
        let long = |end: &str| Document::String("x".repeat(1200) + end);
        let aliases = |names: Vec<Document>| object(&[("aliases", Document::Array(names))]);
        users
            .ensure_index_with("aliases[*]", crate::IndexOptions::new().unique())
            .unwrap();
        users.insert(aliases(vec![long("b")])).unwrap();
        let id = users.insert(aliases(vec![long("a"), long("c")])).unwrap();
        // Rewritten in place, past the index's check: `…c` becomes `…b`,
        // the same length, under the same key.
        let copy = object(&[
            ("_id", Document::Id(id)),
            ("aliases", Document::Array(vec![long("a"), long("b")])),
        ]);
        db.transact(|catalog, store| {
            let meta = *catalog.get("users").unwrap();
            let loc = BTreeIndex::new(meta.index_root)
                .lookup(store, &key::primary(id))?
                .unwrap();
            let mut current = meta.current_data_page;
            let free = catalog.free_space_mut("users");
            assert_eq!(
                data::update_record(store, &mut current, free, loc, id, &copy)?,
                loc
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(problems(&db).len(), 1, "{:#?}", problems(&db));
        assert_found(&db, "is unique, but documents");
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
            let free = catalog.free_space_mut("people");
            let new_loc = data::update_record(store, &mut current, free, loc, id, &copy)?;
            assert_eq!(new_loc, loc, "it shrank, so it stays in place");
            Ok(())
        })
        .unwrap();
        assert_found(&db, "is unique, but documents");
        assert_found(&db, &format!("lacks document {id}"));
    }
}
