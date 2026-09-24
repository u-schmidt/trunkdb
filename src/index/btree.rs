use super::Index;
use super::branch::{decode_branch_entry, encode_branch_entry};
use super::key::{KeyRange, MAX_KEY_LEN};
use super::leaf::{decode_index_entry, encode_index_entry};
use crate::storage::{PageId, PageStore, PageType, RecordLocation, SlottedPage};

/// The real, disk-backed `Index` implementation — a B-tree with linked
/// leaves (leaves double as a sorted linked list via `next_page`; see §10
/// in SPEC.md for why branch pages reuse the same field for something
/// else entirely).
///
/// Keys are byte strings of up to `MAX_KEY_LEN` bytes, compared byte by
/// byte; what they mean is `key.rs`'s business. The primary index uses
/// 16-byte `DocId`s, a secondary index an encoded field value plus the
/// `DocId` (SPEC §28).
///
/// Root page id never changes once an index is created (`Catalog` hands
/// out its root once, permanently) — but the page *at* that id can change
/// from a leaf into a branch, and later from a branch into a taller
/// branch, as the tree grows. See `grow_new_root`.
pub struct BTreeIndex {
    root: PageId,
}

impl BTreeIndex {
    pub fn new(root: PageId) -> Self {
        Self { root }
    }

    /// Frees every page of the tree, root included — for dropping an
    /// index. Leaves aren't reached through their sibling links but
    /// through their parents, like every other page.
    pub fn free_all(&self, store: &mut dyn PageStore) -> std::io::Result<()> {
        // Only once the whole tree was readable: freeing overwrites pages.
        for page_id in self.pages(store)? {
            store.free_page(page_id)?;
        }
        Ok(())
    }

    /// Every page of the tree, root first. A page reached twice is an
    /// error: a tree is a tree.
    pub fn pages(&self, store: &dyn PageStore) -> std::io::Result<Vec<PageId>> {
        let mut pending = vec![self.root];
        let mut pages = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(page_id) = pending.pop() {
            if !seen.insert(page_id) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("index page {page_id} is reached twice — file may be corrupt"),
                ));
            }
            let page = SlottedPage::from_bytes(store.read_page(page_id)?)?;
            match page.page_type() {
                PageType::IndexLeaf => {}
                PageType::IndexBranch => {
                    pending.extend(page.iter_cells().map(|(_slot, c)| decode_branch_entry(c).1));
                    pending.push(page.next_page());
                }
                other => return Err(corrupt_page_type(page_id, other)),
            }
            pages.push(page_id);
        }
        Ok(pages)
    }
}

/// What happened after inserting into a page: either it fit, or the page
/// split and the caller (the parent branch, or `insert` itself for a root
/// split) needs to route around the new sibling.
enum InsertOutcome {
    Done,
    Split {
        separator: Vec<u8>,
        new_right: PageId,
    },
}

impl Index for BTreeIndex {
    fn insert(
        &mut self,
        store: &mut dyn PageStore,
        key: &[u8],
        loc: RecordLocation,
    ) -> std::io::Result<()> {
        if key.len() > MAX_KEY_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "index key is {} bytes, longer than {MAX_KEY_LEN}",
                    key.len()
                ),
            ));
        }
        match insert_into(store, self.root, key, loc)? {
            InsertOutcome::Done => Ok(()),
            InsertOutcome::Split {
                separator,
                new_right,
            } => grow_new_root(store, self.root, &separator, new_right),
        }
    }

    fn remove(&mut self, store: &mut dyn PageStore, key: &[u8]) -> std::io::Result<()> {
        let (page_id, mut page) = find_leaf(store, self.root, key)?;
        let found = page
            .iter_cells()
            .find(|(_slot, cell)| decode_index_entry(cell).0 == key)
            .map(|(slot, _cell)| slot);
        if let Some(slot) = found {
            page.delete_cell(slot);
            store.write_page(page_id, &page.into_bytes())?;
        }
        Ok(()) // not found anywhere is a no-op, matching InMemoryIndex
    }

    fn lookup(&self, store: &dyn PageStore, key: &[u8]) -> std::io::Result<Option<RecordLocation>> {
        let (_page_id, page) = find_leaf(store, self.root, key)?;
        Ok(page
            .iter_cells()
            .map(|(_slot, cell)| decode_index_entry(cell))
            .find(|(k, _loc)| *k == key)
            .map(|(_key, loc)| loc))
    }

    /// Every entry of the walk forward, collected (`walk`).
    fn range(
        &self,
        store: &dyn PageStore,
        range: &KeyRange,
    ) -> std::io::Result<Vec<(Vec<u8>, RecordLocation)>> {
        self.walk(store, range.clone(), false).collect()
    }
}

impl BTreeIndex {
    /// The entries with keys in `range`, one leaf page read at a time,
    /// in ascending key order — or descending, `backward` (SPEC §49).
    /// Stopping early reads no further: "the oldest 20" reads the pages
    /// those 20 are on.
    ///
    /// Forward, it follows the leaves' sibling links from the leaf where
    /// `range.start` would be. Backward there are none: it keeps the
    /// branch pages above the current leaf, with the child it's in, and
    /// steps to the child before — up as far as needed, then down the
    /// last children to a leaf.
    pub fn walk<'s>(&self, store: &'s dyn PageStore, range: KeyRange, backward: bool) -> Walk<'s> {
        Walk {
            store,
            root: self.root,
            range,
            backward,
            pending: Vec::new(),
            started: false,
            next_leaf: 0,
            path: Vec::new(),
            done: false,
        }
    }
}

/// `BTreeIndex::walk`: an iterator of index entries. An I/O error ends
/// it, after it's been handed out.
pub struct Walk<'s> {
    store: &'s dyn PageStore,
    root: PageId,
    range: KeyRange,
    backward: bool,
    /// The current leaf's entries in the range, still to hand out: the
    /// next one last, so `pop` hands it out.
    pending: Vec<(Vec<u8>, RecordLocation)>,
    /// Whether the first leaf has been read.
    started: bool,
    /// Forward: the leaf after the current one, 0 for none.
    next_leaf: PageId,
    /// Backward: from the root down, each branch page above the current
    /// leaf as its children in key order, and which of them the walk is
    /// in.
    path: Vec<(Vec<PageId>, usize)>,
    /// No entry left in the range.
    done: bool,
}

impl Walk<'_> {
    /// Reads the next leaf in the walk's direction into `pending`; sets
    /// `done` where the range or the tree ends. `pending` may stay empty:
    /// a leaf can be (all its entries removed, SPEC §10).
    fn read_next_leaf(&mut self) -> std::io::Result<()> {
        let leaf = if !self.started {
            self.started = true;
            match (self.backward, &self.range.end) {
                (false, _) => self.descend(self.root, Some(&self.range.start.clone()))?,
                (true, Some(end)) => self.descend(self.root, Some(&end.clone()))?,
                (true, None) => self.descend(self.root, None)?,
            }
        } else if !self.backward {
            match self.next_leaf {
                0 => {
                    self.done = true;
                    return Ok(());
                }
                next => SlottedPage::from_bytes(self.store.read_page(next)?)?,
            }
        } else {
            // Up to the nearest branch with a child before this one, then
            // down its last children.
            loop {
                match self.path.last_mut() {
                    None => {
                        self.done = true;
                        return Ok(());
                    }
                    Some((_, 0)) => {
                        self.path.pop();
                    }
                    Some((children, at)) => {
                        *at -= 1;
                        let child = children[*at];
                        break self.descend(child, None)?;
                    }
                }
            }
        };
        self.next_leaf = leaf.next_page();
        let (start, end) = (self.range.start.as_slice(), self.range.end.as_deref());
        let mut entries = Vec::new();
        for (_slot, cell) in leaf.iter_cells() {
            let (key, loc) = decode_index_entry(cell);
            if end.is_some_and(|end| key >= end) {
                // Every later key is past the end too.
                if !self.backward {
                    self.done = true;
                }
                break;
            }
            if key < start {
                // Every earlier key is before the start too.
                if self.backward {
                    self.done = true;
                }
                continue;
            }
            entries.push((key.to_vec(), loc));
        }
        if !self.backward {
            entries.reverse();
        }
        self.pending = entries;
        Ok(())
    }

    /// From `page_id` down to a leaf: towards `key`, or along the last
    /// children for `None` — noting each branch passed in `path`.
    fn descend(&mut self, mut page_id: PageId, key: Option<&[u8]>) -> std::io::Result<SlottedPage> {
        loop {
            let page = SlottedPage::from_bytes(self.store.read_page(page_id)?)?;
            match page.page_type() {
                PageType::IndexLeaf => return Ok(page),
                PageType::IndexBranch => {
                    let mut children: Vec<PageId> = page
                        .iter_cells()
                        .map(|(_slot, cell)| decode_branch_entry(cell).1)
                        .collect();
                    children.push(page.next_page());
                    let at = match key {
                        Some(key) => page
                            .iter_cells()
                            .position(|(_slot, cell)| decode_branch_entry(cell).0 > key)
                            .unwrap_or(children.len() - 1),
                        None => children.len() - 1,
                    };
                    page_id = children[at];
                    if self.backward {
                        self.path.push((children, at));
                    }
                }
                other => return Err(corrupt_page_type(page_id, other)),
            }
        }
    }
}

impl Iterator for Walk<'_> {
    type Item = std::io::Result<(Vec<u8>, RecordLocation)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.pending.pop() {
                return Some(Ok(entry));
            }
            if self.done {
                return None;
            }
            if let Err(e) = self.read_next_leaf() {
                self.done = true;
                return Some(Err(e));
            }
        }
    }
}

/// Descends from `root` to the leaf that holds (or would hold) `key`.
fn find_leaf(
    store: &dyn PageStore,
    root: PageId,
    key: &[u8],
) -> std::io::Result<(PageId, SlottedPage)> {
    let mut page_id = root;
    loop {
        let page = SlottedPage::from_bytes(store.read_page(page_id)?)?;
        match page.page_type() {
            PageType::IndexLeaf => return Ok((page_id, page)),
            PageType::IndexBranch => page_id = find_child(&page, key),
            other => return Err(corrupt_page_type(page_id, other)),
        }
    }
}

/// Reads `page_id` and dispatches to the leaf or branch insert logic —
/// the single recursive entry point every level of the descent calls.
fn insert_into(
    store: &mut dyn PageStore,
    page_id: PageId,
    key: &[u8],
    loc: RecordLocation,
) -> std::io::Result<InsertOutcome> {
    let page = SlottedPage::from_bytes(store.read_page(page_id)?)?;
    match page.page_type() {
        PageType::IndexLeaf => insert_into_leaf(store, page_id, page, key, loc),
        PageType::IndexBranch => insert_into_branch(store, page_id, page, key, loc),
        other => Err(corrupt_page_type(page_id, other)),
    }
}

fn insert_into_leaf(
    store: &mut dyn PageStore,
    page_id: PageId,
    page: SlottedPage,
    key: &[u8],
    loc: RecordLocation,
) -> std::io::Result<InsertOutcome> {
    let mut entries: Vec<(Vec<u8>, RecordLocation)> = page
        .iter_cells()
        .map(|(_slot, c)| {
            let (k, l) = decode_index_entry(c);
            (k.to_vec(), l)
        })
        .collect();
    let next_page = page.next_page(); // this leaf's sibling link, preserved either way

    let pos = entries.partition_point(|(k, _)| k.as_slice() < key);
    entries.insert(pos, (key.to_vec(), loc));

    if let Some(page) = build_leaf_page(&entries, next_page) {
        store.write_page(page_id, &page.into_bytes())?;
        return Ok(InsertOutcome::Done);
    }

    // Doesn't fit: split in place, by bytes (SPEC §28.2) — unless the new
    // key goes past the end of the rightmost leaf. Then only it moves to
    // the new page, and this one stays full: keys arriving in order (ids
    // in time order, a compaction's sorted rebuild) would otherwise leave
    // every leaf half empty, since nothing lands left of them again
    // (SPEC §41).
    let appends = pos == entries.len() - 1 && next_page == 0;
    let mid = match appends {
        true => pos,
        false => byte_midpoint(entries.iter().map(|(k, _)| k.len() + 10)),
    };
    let right_entries = entries.split_off(mid);
    // Smallest key of the right half, copied up (leaves keep it too).
    let separator = right_entries[0].0.clone();

    let new_right_id = store.allocate_page()?;
    let left = build_leaf_page(&entries, new_right_id).expect("each half of a split fits");
    let right = build_leaf_page(&right_entries, next_page).expect("each half of a split fits");
    store.write_page(page_id, &left.into_bytes())?;
    store.write_page(new_right_id, &right.into_bytes())?;

    Ok(InsertOutcome::Split {
        separator,
        new_right: new_right_id,
    })
}

fn insert_into_branch(
    store: &mut dyn PageStore,
    page_id: PageId,
    page: SlottedPage,
    key: &[u8],
    loc: RecordLocation,
) -> std::io::Result<InsertOutcome> {
    let mut entries: Vec<(Vec<u8>, PageId)> = page
        .iter_cells()
        .map(|(_slot, c)| {
            let (k, child) = decode_branch_entry(c);
            (k.to_vec(), child)
        })
        .collect();
    let mut rightmost = page.next_page();

    let idx = entries.partition_point(|(k, _)| k.as_slice() <= key);
    let child_id = if idx < entries.len() {
        entries[idx].1
    } else {
        rightmost
    };

    let outcome = insert_into(store, child_id, key, loc)?;
    let InsertOutcome::Split {
        separator,
        new_right,
    } = outcome
    else {
        return Ok(InsertOutcome::Done); // child absorbed the insert, this page is unchanged
    };

    // `child_id` used to own every key routed through position `idx` (or
    // through `rightmost`, if `idx` fell off the end). It just split into
    // (child_id, new_right); child_id keeps the low half, so the position
    // that used to point at it now needs a separator in front of it and
    // must point at new_right for everything at or above that separator.
    if idx < entries.len() {
        entries[idx].1 = new_right;
        entries.insert(idx, (separator, child_id));
    } else {
        entries.push((separator, child_id));
        rightmost = new_right;
    }

    if let Some(page) = build_branch_page(&entries, rightmost) {
        store.write_page(page_id, &page.into_bytes())?;
        return Ok(InsertOutcome::Done);
    }

    // Doesn't fit: split by bytes, promoting the entry at the split point
    // into the parent instead of copying it (branch keys are routing
    // information, not data, so unlike a leaf split nothing needs to keep
    // a copy). `byte_midpoint` is at least 2 here (SPEC §28.2), so the
    // left half keeps at least one entry.
    let mid = byte_midpoint(entries.iter().map(|(k, _)| k.len() + 8)) - 1;
    let right_entries = entries.split_off(mid + 1);
    let (promoted_key, promoted_child) = entries.pop().expect("mid is within entries");

    let new_right_id = store.allocate_page()?;
    let left = build_branch_page(&entries, promoted_child).expect("each half of a split fits");
    let right = build_branch_page(&right_entries, rightmost).expect("each half of a split fits");
    store.write_page(page_id, &left.into_bytes())?;
    store.write_page(new_right_id, &right.into_bytes())?;

    Ok(InsertOutcome::Split {
        separator: promoted_key,
        new_right: new_right_id,
    })
}

/// Where to split a page's overflowing entries (given their cell sizes)
/// so both halves hold about the same number of bytes: the number of
/// entries before the point where the running total first reaches half.
/// Each half then fits, given every cell is at most a quarter page
/// (`MAX_KEY_LEN`) — see SPEC §28.2 for the arithmetic.
fn byte_midpoint(cell_sizes: impl Iterator<Item = usize> + Clone) -> usize {
    let total: usize = cell_sizes.clone().sum();
    let mut running = 0;
    for (i, size) in cell_sizes.enumerate() {
        running += size;
        if running * 2 >= total {
            return i + 1;
        }
    }
    unreachable!("the running total reaches the total")
}

/// The root page's id is permanent (`Catalog` never reassigns it), so a
/// root split can't just allocate a fresh top page the way every other
/// split does. Instead the root's current content (already rewritten as
/// the "left" half by the split that bubbled up to here) is relocated to
/// a fresh page, and the root id itself is overwritten with a new branch
/// pointing at that relocated page and at `new_right`. The tree grows one
/// level taller; every existing pointer into the root id stays valid.
fn grow_new_root(
    store: &mut dyn PageStore,
    root: PageId,
    separator: &[u8],
    new_right: PageId,
) -> std::io::Result<()> {
    let left_id = store.allocate_page()?;
    store.write_page(left_id, &store.read_page(root)?)?;

    let mut new_root = SlottedPage::new(PageType::IndexBranch);
    new_root.set_next_page(new_right);
    new_root
        .insert_cell(&encode_branch_entry(separator, left_id))
        .expect("a single entry always fits in a fresh page");
    store.write_page(root, &new_root.into_bytes())
}

fn build_leaf_page(
    entries: &[(Vec<u8>, RecordLocation)],
    next_page: PageId,
) -> Option<SlottedPage> {
    let mut page = SlottedPage::new(PageType::IndexLeaf);
    page.set_next_page(next_page);
    for (key, loc) in entries {
        page.insert_cell(&encode_index_entry(key, *loc))?;
    }
    Some(page)
}

fn build_branch_page(entries: &[(Vec<u8>, PageId)], rightmost: PageId) -> Option<SlottedPage> {
    let mut page = SlottedPage::new(PageType::IndexBranch);
    page.set_next_page(rightmost);
    for (key, child) in entries {
        page.insert_cell(&encode_branch_entry(key, *child))?;
    }
    Some(page)
}

/// Which child of this branch page would hold `target`: the classic "n
/// keys route to n+1 children" rule. Cells are (separator_key, child)
/// pairs where `child` handles everything below `separator_key`; the
/// first cell whose separator exceeds `target` supplies the answer. If
/// none do, `target` is at or above every separator here, so the
/// rightmost pointer answers instead — reusing the page's `next_page`
/// field, which means something different on a branch page (the child
/// past the last separator) than it does on a leaf (the next sibling).
/// Relies on cells being stored in sorted order, which every branch page
/// is: it's always rebuilt from a sorted `Vec` (see `insert_into_branch`),
/// never mutated cell-by-cell in place.
fn find_child(page: &SlottedPage, target: &[u8]) -> PageId {
    for (_slot, cell) in page.iter_cells() {
        let (key, child) = decode_branch_entry(cell);
        if key > target {
            return child;
        }
    }
    page.next_page()
}

fn corrupt_page_type(page_id: PageId, found: PageType) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "page {page_id} was expected to be an index leaf or branch, found {found:?} — file may be corrupt"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::DocId;
    use crate::index::InMemoryIndex;
    use crate::storage::FileStore;
    use crate::testing::XorShift;

    fn fresh_index_root(store: &mut dyn PageStore) -> PageId {
        let root = store.allocate_page().unwrap();
        store
            .write_page(root, &SlottedPage::new(PageType::IndexLeaf).into_bytes())
            .unwrap();
        root
    }

    fn fresh() -> (tempfile::TempDir, FileStore, BTreeIndex) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        let root = fresh_index_root(&mut store);
        (dir, store, BTreeIndex::new(root))
    }

    /// The leaves of `index`, in key order, each with its entry count.
    fn leaf_sizes(store: &FileStore, index: &BTreeIndex) -> Vec<usize> {
        let (_id, mut page) = find_leaf(store, index.root, &[]).unwrap();
        let mut sizes = vec![page.iter_cells().count()];
        while page.next_page() != 0 {
            page = SlottedPage::from_bytes(store.read_page(page.next_page()).unwrap()).unwrap();
            sizes.push(page.iter_cells().count());
        }
        sizes
    }

    /// Keys arriving in order fill every leaf but the last completely
    /// (SPEC §41): each split leaves the old leaf as it was, full. In
    /// reverse order they still split in the middle, by bytes.
    /// A read-only view of a store that counts page reads.
    struct Counting<'a> {
        store: &'a FileStore,
        reads: std::cell::Cell<usize>,
    }

    impl PageStore for Counting<'_> {
        fn allocate_page(&mut self) -> std::io::Result<PageId> {
            unreachable!("read only")
        }
        fn read_page(&self, id: PageId) -> std::io::Result<Vec<u8>> {
            self.reads.set(self.reads.get() + 1);
            self.store.read_page(id)
        }
        fn try_read_page(&self, id: PageId) -> std::io::Result<Option<Vec<u8>>> {
            self.reads.set(self.reads.get() + 1);
            self.store.try_read_page(id)
        }
        fn write_page(&mut self, _id: PageId, _data: &[u8]) -> std::io::Result<()> {
            unreachable!("read only")
        }
        fn free_page(&mut self, _id: PageId) -> std::io::Result<()> {
            unreachable!("read only")
        }
    }

    /// Random keys in, a third taken out again — whole leaves of them
    /// too, so some leaves are empty — then random ranges walked both
    /// ways: the same entries as a `BTreeMap` has, in its order or the
    /// reverse (SPEC §49).
    #[test]
    fn walks_match_a_btreemap_both_ways() {
        let (_dir, mut store, mut index) = fresh();
        let mut rng = XorShift(0x243F_6A88_85A3_08D3);
        let mut expected = std::collections::BTreeMap::new();
        for i in 0..6000u32 {
            let len = 1 + rng.below(40);
            let key: Vec<u8> = (0..len).map(|_| rng.below(4) as u8 * 60).collect();
            let key = [key, i.to_be_bytes().to_vec()].concat();
            let loc = RecordLocation {
                page: i as u64,
                slot: 0,
            };
            index.insert(&mut store, &key, loc).unwrap();
            expected.insert(key, loc);
        }
        let keys: Vec<Vec<u8>> = expected.keys().cloned().collect();
        // A stretch of neighbors, emptying the leaves they were on.
        for key in keys[1000..2500].iter().chain(keys.iter().step_by(3)) {
            index.remove(&mut store, key).unwrap();
            expected.remove(key);
        }
        let bound = |rng: &mut XorShift| -> Vec<u8> {
            match rng.below(3) {
                0 => keys[rng.below(keys.len())].clone(),
                _ => (0..rng.below(4)).map(|_| rng.below(256) as u8).collect(),
            }
        };
        for _ in 0..300 {
            let (a, b) = (bound(&mut rng), bound(&mut rng));
            let range = match rng.below(4) {
                0 => KeyRange::everything(),
                1 => KeyRange {
                    start: a,
                    end: None,
                },
                _ => KeyRange {
                    start: a.clone().min(b.clone()),
                    end: Some(a.max(b)),
                },
            };
            let want: Vec<(Vec<u8>, RecordLocation)> = expected
                .iter()
                .filter(|(k, _)| **k >= range.start && range.end.as_ref().is_none_or(|e| *k < e))
                .map(|(k, l)| (k.clone(), *l))
                .collect();
            let forward: Vec<_> = index
                .walk(&store, range.clone(), false)
                .map(Result::unwrap)
                .collect();
            let mut backward: Vec<_> = index
                .walk(&store, range.clone(), true)
                .map(Result::unwrap)
                .collect();
            backward.reverse();
            assert_eq!(forward, want, "{range:?}");
            assert_eq!(backward, want, "{range:?}");
            assert_eq!(index.range(&store, &range).unwrap(), want);
        }
    }

    /// How many pages from the root to a leaf, both counted.
    fn depth(store: &FileStore, index: &BTreeIndex) -> usize {
        let mut depth = 1;
        let mut page = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
        while page.page_type() == PageType::IndexBranch {
            let child = page.next_page();
            page = SlottedPage::from_bytes(store.read_page(child).unwrap()).unwrap();
            depth += 1;
        }
        depth
    }

    /// Long keys make a tall tree: a backward walk has to climb more than
    /// one branch to find the previous leaf (SPEC §49.1).
    #[test]
    fn walks_climb_as_far_as_needed_in_a_tall_tree() {
        let (_dir, mut store, mut index) = fresh();
        let mut expected = Vec::new();
        for i in 0..4000u32 {
            let key = [&i.to_be_bytes()[..], &[b'k'; 600]].concat();
            let loc = RecordLocation {
                page: i as u64,
                slot: 0,
            };
            index.insert(&mut store, &key, loc).unwrap();
            expected.push((key, loc));
        }
        assert!(depth(&store, &index) >= 4, "{}", depth(&store, &index));
        let all: Vec<_> = index
            .walk(&store, KeyRange::everything(), true)
            .map(Result::unwrap)
            .collect();
        expected.reverse();
        assert_eq!(all, expected);
    }

    /// Taking the first few entries reads the pages they're on, not the
    /// range's: a descent and a leaf or two, either way.
    #[test]
    fn a_walk_stopped_early_reads_only_what_it_handed_out() {
        let (_dir, mut store, mut index) = fresh();
        for i in 0..20_000u32 {
            let key = [&i.to_be_bytes()[..], &[7u8; 12]].concat();
            index
                .insert(&mut store, &key, RecordLocation { page: 1, slot: 0 })
                .unwrap();
        }
        let counting = Counting {
            store: &store,
            reads: std::cell::Cell::new(0),
        };
        let depth = depth(&store, &index);
        assert!(depth >= 2, "{depth}");
        for backward in [false, true] {
            counting.reads.set(0);
            let first: Vec<_> = index
                .walk(&counting, KeyRange::everything(), backward)
                .take(20)
                .map(Result::unwrap)
                .collect();
            assert_eq!(first.len(), 20);
            assert!(
                counting.reads.get() <= depth + 1,
                "{backward}: {} reads",
                counting.reads.get()
            );
            counting.reads.set(0);
            let all = index
                .walk(&counting, KeyRange::everything(), backward)
                .count();
            assert_eq!(all, 20_000);
            let leaves = counting.reads.get();
            assert!(leaves > 50, "{leaves}");
            // A thousand keys in the middle: their leaves and a descent,
            // not the leaves before or after them.
            let key = |i: u32| [&i.to_be_bytes()[..], &[7u8; 12]].concat();
            let middle = KeyRange {
                start: key(9_000),
                end: Some(key(10_000)),
            };
            counting.reads.set(0);
            assert_eq!(
                index.walk(&counting, middle.clone(), backward).count(),
                1000
            );
            let most = depth + 2 + 1000 * leaves / 20_000;
            assert!(
                counting.reads.get() <= most,
                "{backward}: {} > {most}",
                counting.reads.get()
            );
            counting.reads.set(0);
            assert_eq!(index.walk(&counting, middle, backward).take(20).count(), 20);
            assert!(
                counting.reads.get() <= depth + 1,
                "{backward}: {}",
                counting.reads.get()
            );
        }
    }

    #[test]
    fn keys_in_order_fill_the_leaves() {
        let keys: Vec<[u8; 16]> = (0..3000u32)
            .map(|i| {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&i.to_be_bytes());
                key
            })
            .collect();
        let loc = RecordLocation { page: 1, slot: 0 };

        let (_dir, mut store, mut index) = fresh();
        for key in &keys {
            index.insert(&mut store, key, loc).unwrap();
        }
        let sizes = leaf_sizes(&store, &index);
        let full = sizes[0];
        assert!(sizes.len() > 5, "{sizes:?}");
        assert!(
            sizes[..sizes.len() - 1].iter().all(|&n| n == full),
            "{sizes:?}"
        );
        assert_eq!(sizes.iter().sum::<usize>(), keys.len());

        let (_dir, mut store, mut index) = fresh();
        for key in keys.iter().rev() {
            index.insert(&mut store, key, loc).unwrap();
        }
        let sizes = leaf_sizes(&store, &index);
        assert!(sizes.iter().all(|&n| n < full * 2 / 3), "{sizes:?}");
        for key in &keys {
            assert_eq!(index.lookup(&store, key).unwrap(), Some(loc));
        }
    }

    #[test]
    fn insert_then_lookup_finds_it() {
        let (_dir, mut store, mut index) = fresh();

        let id = [7; 16];
        let loc = RecordLocation { page: 5, slot: 1 };
        index.insert(&mut store, &id, loc).unwrap();

        assert_eq!(index.lookup(&store, &id).unwrap(), Some(loc));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let (_dir, store, index) = fresh();
        assert_eq!(index.lookup(&store, &[9; 16]).unwrap(), None);
    }

    #[test]
    fn remove_then_lookup_returns_none() {
        let (_dir, mut store, mut index) = fresh();

        let id = [3; 16];
        let loc = RecordLocation { page: 2, slot: 0 };
        index.insert(&mut store, &id, loc).unwrap();
        index.remove(&mut store, &id).unwrap();

        assert_eq!(index.lookup(&store, &id).unwrap(), None);
    }

    #[test]
    fn rejects_overlong_keys() {
        let (_dir, mut store, mut index) = fresh();
        let loc = RecordLocation { page: 1, slot: 0 };
        let err = index
            .insert(&mut store, &vec![0; MAX_KEY_LEN + 1], loc)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        index
            .insert(&mut store, &vec![0; MAX_KEY_LEN], loc)
            .unwrap();
    }

    fn id_from(i: u16) -> [u8; 16] {
        // Big-endian so byte order matches numeric order.
        let mut bytes = [0u8; 16];
        bytes[0..2].copy_from_slice(&i.to_be_bytes());
        bytes
    }

    #[test]
    fn insert_enough_entries_to_force_a_leaf_split() {
        let (_dir, mut store, mut index) = fresh();

        // A leaf holds ~270 fixed-26-byte entries; 500 guarantees at
        // least one split, promoting the root from a leaf to a branch.
        for i in 0..500u16 {
            let loc = RecordLocation { page: 100, slot: i };
            index.insert(&mut store, &id_from(i), loc).unwrap();
        }

        let root_page = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
        assert_eq!(
            root_page.page_type(),
            PageType::IndexBranch,
            "500 entries must have split the root leaf into a branch"
        );

        for i in 0..500u16 {
            let expected = RecordLocation { page: 100, slot: i };
            assert_eq!(index.lookup(&store, &id_from(i)).unwrap(), Some(expected));
        }
    }

    #[test]
    fn scan_after_many_inserts_is_sorted_and_complete() {
        let (_dir, mut store, mut index) = fresh();

        // Insert out of order, on purpose, to prove scan sorts rather
        // than just replaying insertion order.
        let mut order: Vec<u16> = (0..2000).collect();
        order.sort_by_key(|i| i.wrapping_mul(7919) % 2000); // cheap shuffle

        for &i in &order {
            let loc = RecordLocation { page: 1, slot: i };
            index.insert(&mut store, &id_from(i), loc).unwrap();
        }

        let keys: Vec<Vec<u8>> = index
            .scan(&store)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let expected: Vec<Vec<u8>> = (0..2000).map(|i| id_from(i).to_vec()).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn remove_works_after_the_tree_has_multiple_leaves() {
        let (_dir, mut store, mut index) = fresh();

        for i in 0..500u16 {
            let loc = RecordLocation { page: 1, slot: i };
            index.insert(&mut store, &id_from(i), loc).unwrap();
        }

        index.remove(&mut store, &id_from(250)).unwrap();

        assert_eq!(index.lookup(&store, &id_from(250)).unwrap(), None);
        // Neighbors must be untouched.
        assert!(index.lookup(&store, &id_from(249)).unwrap().is_some());
        assert!(index.lookup(&store, &id_from(251)).unwrap().is_some());
    }

    /// Keys of every length up to `MAX_KEY_LEN`, many sharing long
    /// prefixes, inserted and removed in random order: the tree must
    /// always agree with a `BTreeMap` on every range. Long keys make
    /// splits frequent, uneven in entry count, and several levels deep.
    #[test]
    fn variable_length_keys_match_a_btreemap() {
        let (_dir, mut store, mut index) = fresh();
        let mut model = InMemoryIndex::default();
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut keys: Vec<Vec<u8>> = Vec::new();

        for round in 0..3000u16 {
            if !keys.is_empty() && rng.below(4) == 0 {
                let key = keys.swap_remove(rng.below(keys.len()));
                index.remove(&mut store, &key).unwrap();
                model.remove(&mut store, &key).unwrap();
                continue;
            }
            let len = match rng.below(3) {
                0 => rng.below(8),
                1 => rng.below(64),
                _ => MAX_KEY_LEN - rng.below(64),
            };
            // A small alphabet, so keys share long prefixes.
            let key: Vec<u8> = (0..len).map(|_| b"ab\0\xFF"[rng.below(4)]).collect();
            if keys.contains(&key) {
                continue;
            }
            let loc = RecordLocation {
                page: 1,
                slot: round,
            };
            index.insert(&mut store, &key, loc).unwrap();
            model.insert(&mut store, &key, loc).unwrap();
            keys.push(key);
        }

        assert_eq!(index.scan(&store).unwrap(), model.scan(&store).unwrap());
        for _ in 0..200 {
            let a = &keys[rng.below(keys.len())];
            let b = &keys[rng.below(keys.len())];
            let range = KeyRange {
                start: a.min(b).clone(),
                end: Some(a.max(b).clone()),
            };
            assert_eq!(
                index.range(&store, &range).unwrap(),
                model.range(&store, &range).unwrap()
            );
            let open_ended = KeyRange {
                start: a.clone(),
                end: None,
            };
            assert_eq!(
                index.range(&store, &open_ended).unwrap(),
                model.range(&store, &open_ended).unwrap()
            );
        }
        for key in &keys {
            assert_eq!(
                index.lookup(&store, key).unwrap(),
                model.lookup(&store, key).unwrap()
            );
        }
        let root_page = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
        assert_eq!(root_page.page_type(), PageType::IndexBranch);
    }

    #[test]
    fn free_all_returns_every_page() {
        let (_dir, mut store, mut index) = fresh();
        let high_water_before = store.allocate_page().unwrap();
        store.free_page(high_water_before).unwrap();
        for i in 0..2000u16 {
            let loc = RecordLocation { page: 1, slot: i };
            index.insert(&mut store, &id_from(i), loc).unwrap();
        }
        let high_water = store.allocate_page().unwrap();
        store.free_page(high_water).unwrap();

        index.free_all(&mut store).unwrap();

        // Every page from the root up is free again: reallocating them
        // all doesn't grow the file.
        let tree_pages = (high_water - high_water_before) as usize + 1;
        let mut again: Vec<PageId> = (0..=tree_pages)
            .map(|_| store.allocate_page().unwrap())
            .collect();
        again.sort();
        let mut expected: Vec<PageId> = (high_water_before..=high_water).collect();
        expected.push(index.root);
        expected.sort();
        assert_eq!(again, expected);
    }

    #[test]
    fn keys_from_the_key_module_work_as_is() {
        use crate::document::Document;
        use crate::index::key;
        let (_dir, mut store, mut index) = fresh();
        for (n, id) in [(3, 1), (1, 2), (2, 3), (2, 4)] {
            let k = key::secondary(&Document::Int(n), DocId([id; 16])).unwrap();
            let loc = RecordLocation {
                page: n as u64,
                slot: id as u16,
            };
            index.insert(&mut store, &k, loc).unwrap();
        }
        let range = key::range_for(&crate::query::Op::Eq, &Document::Int(2)).unwrap();
        let slots: Vec<u16> = index
            .range(&store, &range)
            .unwrap()
            .into_iter()
            .map(|(_, loc)| loc.slot)
            .collect();
        assert_eq!(slots, vec![3, 4]);
    }
}
