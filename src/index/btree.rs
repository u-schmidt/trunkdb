use super::Index;
use super::branch::{MIN_BRANCH_ENTRY_LEN, decode_branch_entry, encode_branch_entry};
use super::key::{KeyRange, MAX_KEY_LEN};
use super::leaf::{
    KeyParts, decode_anchor, decode_index_entry, decode_packed, encode_anchor, encode_packed,
};
use crate::storage::{PageId, PageStore, PageType, RecordLocation, SlottedPage};

/// The real, disk-backed `Index` implementation — a B-tree with linked
/// leaves (leaves double as a sorted linked list via `next_page`; see SPEC
/// §10 for why branch pages reuse the same field for something
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

    /// Empties the tree, keeping its root page — where the catalog finds
    /// it — as an empty leaf, and freeing every other page: for
    /// rebuilding an index in place (SPEC §62).
    pub fn clear(&self, store: &mut dyn PageStore) -> std::io::Result<()> {
        let pages = self.pages(store)?;
        for &page_id in &pages[1..] {
            store.free_page(page_id)?;
        }
        store.write_page(
            self.root,
            SlottedPage::new(PageType::IndexLeafPacked).into_page(),
        )
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
            let page = read_node(store, page_id)?;
            match page.page_type() {
                PageType::IndexLeaf | PageType::IndexLeafPacked => {}
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

impl BTreeIndex {
    /// How many levels the tree has: 1 for a lone leaf. Down the first
    /// children, which every level has as deep as any other.
    pub fn depth(&self, store: &dyn PageStore) -> std::io::Result<u32> {
        let mut page_id = self.root;
        for depth in 1..=MAX_DEPTH as u32 + 1 {
            let page = read_node(store, page_id)?;
            match page.page_type() {
                PageType::IndexLeaf | PageType::IndexLeafPacked => return Ok(depth),
                PageType::IndexBranch => {
                    page_id = page
                        .iter_cells()
                        .next()
                        .map_or(page.next_page(), |(_slot, cell)| {
                            decode_branch_entry(cell).1
                        });
                }
                other => return Err(corrupt_page_type(page_id, other)),
            }
        }
        Err(too_deep(page_id))
    }
}

/// The tree `insert` builds from keys handed to it in ascending order —
/// as `compact` rebuilds every index (SPEC §41) — counted instead of
/// built (SPEC §93.4). A full rightmost leaf is rebuilt with a better
/// anchor if that makes room, or stays full and the new key starts the
/// next one; a full branch splits by bytes and promotes its middle
/// entry, as `insert_into_branch` does. It holds one page's worth of
/// entries per level, whatever the number of keys. The locations are the
/// entries' own, where a rebuild's differ a little: an estimate.
#[derive(Default)]
pub(crate) struct SortedBuild {
    /// Leaves finished or begun; 0 before the first key.
    leaves: u64,
    /// The last leaf's entries, and its anchor.
    leaf: Vec<(Vec<u8>, RecordLocation)>,
    anchor: Vec<u8>,
    /// Room left in the last leaf.
    leaf_room: usize,
    /// Above the leaves, lowest first: each level's rightmost branch.
    levels: Vec<BranchLevel>,
}

struct BranchLevel {
    /// Pages of this level already left behind by a split.
    finished: u64,
    /// The rightmost page's separator key lengths, in order.
    keys: Vec<usize>,
    room: usize,
}

impl SortedBuild {
    /// The next entry: its key at or above every key before it.
    pub(crate) fn push(&mut self, key: &[u8], loc: RecordLocation) {
        if self.leaves == 0 {
            self.leaves = 1;
            self.leaf_room = 0;
        }
        let needed = SlottedPage::space_needed(encode_packed(&self.anchor, key, loc).len());
        if !self.leaf.is_empty() && self.leaf_room >= needed {
            self.leaf_room -= needed;
            self.leaf.push((key.to_vec(), loc));
            return;
        }
        self.leaf.push((key.to_vec(), loc));
        if let Some((anchor, room)) = packed_room(&self.leaf) {
            (self.anchor, self.leaf_room) = (anchor, room);
            return;
        }
        // Full: the new key starts the next leaf, and is the separator
        // above it.
        self.leaf = vec![(key.to_vec(), loc)];
        let (anchor, room) = packed_room(&self.leaf).expect("one entry fits");
        (self.anchor, self.leaf_room) = (anchor, room);
        self.leaves += 1;
        self.separate(0, key.len());
    }

    fn separate(&mut self, level: usize, key_len: usize) {
        let cell = |key_len: usize| SlottedPage::space_needed(key_len + MIN_BRANCH_ENTRY_LEN);
        if level == self.levels.len() {
            // The root split: a new root above, with this one entry.
            self.levels.push(BranchLevel {
                finished: 0,
                keys: vec![key_len],
                room: empty_room(PageType::IndexBranch) - cell(key_len),
            });
            return;
        }
        let branch = &mut self.levels[level];
        if branch.room >= cell(key_len) {
            branch.keys.push(key_len);
            branch.room -= cell(key_len);
            return;
        }
        branch.keys.push(key_len);
        let mid = byte_midpoint(branch.keys.iter().map(|len| len + 8)) - 1;
        let right = branch.keys.split_off(mid + 1);
        let promoted = branch.keys.pop().expect("mid is within the keys");
        branch.finished += 1;
        branch.room =
            empty_room(PageType::IndexBranch) - right.iter().map(|&len| cell(len)).sum::<usize>();
        branch.keys = right;
        self.separate(level + 1, promoted);
    }

    /// The tree's pages: an empty one is its root leaf alone.
    pub(crate) fn pages(&self) -> u64 {
        self.leaves.max(1)
            + self
                .levels
                .iter()
                .map(|level| level.finished + 1)
                .sum::<u64>()
    }
}

fn empty_room(page_type: PageType) -> usize {
    SlottedPage::new(page_type).free_space()
}

/// The anchor a packed leaf of `entries` gets, and the room it leaves;
/// `None` if they don't fit.
fn packed_room(entries: &[(Vec<u8>, RecordLocation)]) -> Option<(Vec<u8>, usize)> {
    let anchor = anchor_of(entries).to_vec();
    let mut needed = SlottedPage::space_needed(encode_anchor(&anchor).len());
    for (key, loc) in entries {
        needed += SlottedPage::space_needed(encode_packed(&anchor, key, *loc).len());
    }
    let room = empty_room(PageType::IndexLeafPacked);
    (needed <= room).then(|| (anchor, room - needed))
}

/// The anchor of a leaf of `entries`: the middle one's key, which
/// shares the most with the others in the middle of a sorted run.
fn anchor_of(entries: &[(Vec<u8>, RecordLocation)]) -> &[u8] {
    entries.get(entries.len() / 2).map_or(&[], |(key, _)| key)
}

/// A leaf page read for its entries, either layout: a plain leaf's
/// cells are whole entries; a packed one's first cell is its anchor, and
/// its entries follow, each after the anchor's first bytes (`leaf.rs`).
#[derive(Clone, Copy)]
struct Leaf<'p> {
    page: &'p SlottedPage,
    /// The anchor of a packed leaf.
    anchor: Option<&'p [u8]>,
}

impl<'p> Leaf<'p> {
    fn of(page: &'p SlottedPage) -> Self {
        let anchor = (page.page_type() == PageType::IndexLeafPacked)
            .then(|| page.get_cell(0).map_or(&[][..], decode_anchor));
        Leaf { page, anchor }
    }

    /// The first slot that can hold an entry: past a packed leaf's anchor.
    fn first(&self) -> u16 {
        u16::from(self.anchor.is_some() && self.page.slot_count() > 0)
    }

    /// Its live entries.
    fn len(&self) -> usize {
        self.page.live_cells() - usize::from(self.first())
    }

    fn entry(&self, cell: &'p [u8]) -> (KeyParts<'p>, RecordLocation) {
        match self.anchor {
            Some(anchor) => decode_packed(cell, anchor),
            None => {
                let (key, loc) = decode_index_entry(cell);
                (KeyParts::whole(key), loc)
            }
        }
    }

    fn entry_at(&self, slot: u16) -> Option<(KeyParts<'p>, RecordLocation)> {
        self.page.get_cell(slot).map(|cell| self.entry(cell))
    }

    /// The first live slot whose key is at least `key`, by a binary search.
    fn first_from(&self, key: &[u8]) -> Option<u16> {
        self.first_from_slot(self.first(), key)
    }

    /// `first_from`, from slot `from` on.
    fn first_from_slot(&self, from: u16, key: &[u8]) -> Option<u16> {
        first_slot_where(self.page, from, |cell| {
            self.entry(cell).0.cmp(key) != std::cmp::Ordering::Less
        })
    }

    /// The slot of `key`, if the leaf holds it.
    fn slot_of(&self, key: &[u8]) -> Option<u16> {
        self.first_from(key).filter(|&slot| {
            self.entry_at(slot).expect("a live slot").0.cmp(key) == std::cmp::Ordering::Equal
        })
    }

    /// Every entry, copied out, in key order.
    fn entries(&self) -> Vec<(Vec<u8>, RecordLocation)> {
        (self.first()..self.page.slot_count())
            .filter_map(|slot| self.entry_at(slot))
            .map(|(key, loc)| (key.to_vec(), loc))
            .collect()
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
        match insert_into(store, self.root, key, loc, 0)? {
            InsertOutcome::Done => Ok(()),
            InsertOutcome::Split {
                separator,
                new_right,
            } => grow_new_root(store, self.root, &separator, new_right),
        }
    }

    fn remove(&mut self, store: &mut dyn PageStore, key: &[u8]) -> std::io::Result<()> {
        let (page_id, page) = find_leaf(store, self.root, key)?;
        if let Some(slot) = Leaf::of(&page).slot_of(key) {
            store.change_page(page_id, page, &mut |page| page.remove_cell_at(slot))?;
        }
        Ok(()) // not found anywhere is a no-op, matching InMemoryIndex
    }

    fn lookup(&self, store: &dyn PageStore, key: &[u8]) -> std::io::Result<Option<RecordLocation>> {
        let (_page_id, page) = find_leaf(store, self.root, key)?;
        let leaf = Leaf::of(&page);
        Ok(leaf
            .slot_of(key)
            .map(|slot| leaf.entry_at(slot).expect("a live slot").1))
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
            leaf: None,
            at: 0,
            started: false,
            next_leaf: 0,
            path: Vec::new(),
            done: false,
            seen: std::collections::HashSet::new(),
            leaves: 0,
            key: Vec::new(),
            kept: 0,
            clear: false,
            offered: None,
            run_end: 0,
            lowest: Vec::new(),
            highest: Vec::new(),
        }
    }
}

impl BTreeIndex {
    /// How many entries have keys in `range`: a leaf all inside it by
    /// its slot directory, without looking at a key; only the leaves at
    /// the range's ends entry by entry (SPEC §101.4).
    pub fn count(&self, store: &dyn PageStore, range: KeyRange) -> std::io::Result<usize> {
        let counted = self.count_at_most(store, range, usize::MAX)?;
        Ok(counted.entries)
    }

    /// `count`, entering at most `leaves` leaves (SPEC §133.3): past
    /// them, the entries counted so far, and `whole` false. A leaf all
    /// inside the range costs its page and no key decoded.
    pub fn count_at_most(
        &self,
        store: &dyn PageStore,
        range: KeyRange,
        leaves: usize,
    ) -> std::io::Result<Counted> {
        let mut walk = self.walk(store, range, false);
        let mut entries = 0;
        loop {
            if walk.leaves >= leaves && walk.used_up() {
                // The next step would enter a leaf past the cap: whole
                // only if there's no entry left at all.
                let whole = match walk.step() {
                    None => true,
                    Some(Ok(_)) => false,
                    Some(Err(e)) => return Err(e),
                };
                return Ok(Counted {
                    entries,
                    whole,
                    leaves: walk.leaves,
                });
            }
            if let Some(leaf) = walk.run(usize::MAX)? {
                entries += leaf.entries;
                walk.pass_leaf();
                continue;
            }
            match walk.step() {
                Some(entry) => {
                    entry?;
                    entries += 1;
                }
                None => {
                    return Ok(Counted {
                        entries,
                        whole: true,
                        leaves: walk.leaves,
                    });
                }
            }
        }
    }
}

/// What `BTreeIndex::count_at_most` counted: the entries, and whether
/// that is all of the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counted {
    pub entries: usize,
    pub whole: bool,
    /// The leaves entered to count them.
    pub leaves: usize,
}

/// `BTreeIndex::walk`: an iterator of index entries. An I/O error ends
/// it, after it's been handed out.
///
/// It keeps its current leaf and steps through it, copying an entry's
/// key only when it hands the entry out (SPEC §97): a walk that stops
/// at a limit copies what it handed out, not the rest of the leaf.
pub struct Walk<'s> {
    store: &'s dyn PageStore,
    root: PageId,
    range: KeyRange,
    backward: bool,
    /// The leaf the walk is in — its page, shared with the cache, not
    /// copied.
    leaf: Option<SlottedPage>,
    /// The cursor in `leaf`: forward the next slot to look at, backward
    /// the slot after it.
    at: u16,
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
    /// Forward: every leaf read, so sibling links that go round a loop
    /// are damage, not a walk that never ends (SPEC §55).
    seen: std::collections::HashSet<PageId>,
    /// How many leaves it has entered.
    leaves: usize,
    /// The key `step` hands out, put together from its leaf.
    key: Vec<u8>,
    /// How many of `key`'s first bytes are the current leaf's anchor's,
    /// which the next key needn't copy again.
    kept: usize,
    /// Whether every key of the current leaf left in the walk's direction
    /// lies inside the range's far end, so no step need check it.
    clear: bool,
    /// The cursor a run of the current leaf was described at (`run`),
    /// until it's passed.
    offered: Option<u16>,
    /// Where `pass_leaf` puts the cursor: past the run, or the whole
    /// leaf, just described.
    run_end: u16,
    /// The keys `whole_leaf` hands out.
    lowest: Vec<u8>,
    highest: Vec<u8>,
}

impl Walk<'_> {
    /// The next entry, its key lent from the walk, not copied out (SPEC
    /// §101.1): it lives until the next step. An I/O error ends the
    /// walk, after it's been handed out.
    pub fn step(&mut self) -> Option<std::io::Result<(&[u8], RecordLocation)>> {
        let loc = loop {
            let Walk {
                leaf,
                at,
                range,
                backward,
                done,
                clear,
                key,
                kept,
                ..
            } = &mut *self;
            if let Some(page) = leaf.as_ref().filter(|_| !*done)
                && let Some((parts, loc)) = next_in_leaf(page, at, range, *backward, done, *clear)
            {
                parts.write_over(key, kept);
                break loc;
            }
            if self.done {
                return None;
            }
            if let Err(e) = self.read_next_leaf() {
                self.done = true;
                return Some(Err(e));
            }
        };
        Some(Ok((&self.key, loc)))
    }

    /// The leaf the walk is about to enter, if all of it lies inside the
    /// range and none of it has been handed out (SPEC §101.3): its lowest
    /// and highest keys and how many live entries it holds. Reads that
    /// leaf if the current one is used up. `None` anywhere else — the
    /// first and last leaves of a range, mostly — and for an empty leaf;
    /// `step` then goes on entry by entry.
    pub fn whole_leaf(&mut self) -> std::io::Result<Option<WholeLeaf<'_>>> {
        if self.done {
            return Ok(None);
        }
        if self.used_up() {
            if let Err(e) = self.read_next_leaf() {
                self.done = true;
                return Err(e);
            }
            if self.done {
                return Ok(None);
            }
        }
        let Some(page) = &self.leaf else {
            return Ok(None);
        };
        let leaf = Leaf::of(page);
        let untouched = match self.backward {
            true => self.at == page.slot_count(),
            false => self.at == leaf.first(),
        };
        if !untouched {
            return Ok(None);
        }
        let mut live = (leaf.first()..page.slot_count()).filter_map(|slot| leaf.entry_at(slot));
        let Some(lowest) = live.next() else {
            return Ok(None);
        };
        let highest = live.next_back().unwrap_or(lowest);
        let entries = leaf.len();
        lowest.0.write_to(&mut self.lowest);
        highest.0.write_to(&mut self.highest);
        self.run_end = match self.backward {
            true => leaf.first(),
            false => page.slot_count(),
        };
        let inside = self.lowest >= self.range.start
            && self
                .range
                .end
                .as_deref()
                .is_none_or(|end| self.highest.as_slice() < end);
        Ok(inside.then_some(WholeLeaf {
            lowest: &self.lowest,
            highest: &self.highest,
            entries,
        }))
    }

    /// Whether the current leaf has no entry left to hand out in the
    /// walk's direction: the next step enters another leaf.
    fn used_up(&self) -> bool {
        match (&self.leaf, self.backward) {
            (None, _) => true,
            (Some(page), true) => self.at <= Leaf::of(page).first(),
            (Some(page), false) => self.at >= page.slot_count(),
        }
    }

    /// Passes over what `whole_leaf` or `run` just described.
    pub fn pass_leaf(&mut self) {
        if self.leaf.is_some() {
            self.at = self.run_end;
            self.offered = None;
        }
    }

    /// The live entries of the current leaf from the cursor on, in the
    /// walk's direction, that lie inside the range — at most `most` of
    /// them, the nearest: their lowest and highest keys and how many.
    /// Reads the next leaf if the current one is used up. Found by binary
    /// searches, no entry between them decoded, so a count or a skip
    /// passes them at once where it would step through them (SPEC
    /// §172.4). `None` for no such entry, for `most` 0, and once a run
    /// offered on this leaf has been stepped into instead of passed: a
    /// walk going entry by entry isn't offered one at every step.
    pub fn run(&mut self, most: usize) -> std::io::Result<Option<WholeLeaf<'_>>> {
        if self.done || most == 0 {
            return Ok(None);
        }
        if self.used_up() {
            if let Err(e) = self.read_next_leaf() {
                self.done = true;
                return Err(e);
            }
            if self.done {
                return Ok(None);
            }
        }
        let Some(page) = &self.leaf else {
            return Ok(None);
        };
        if self.offered.is_some_and(|at| at != self.at) {
            return Ok(None);
        }
        self.offered = Some(self.at);
        let leaf = Leaf::of(page);
        let (mut lo, mut hi) = match (self.backward, self.clear) {
            (false, true) => (self.at, page.slot_count()),
            (false, false) => {
                let end = self.range.end.as_deref().unwrap_or_default();
                let to = leaf.first_from_slot(self.at, end);
                (self.at, to.unwrap_or(page.slot_count()))
            }
            (true, true) => (leaf.first(), self.at),
            (true, false) => {
                let from = leaf.first_from(&self.range.start).unwrap_or(self.at);
                (from.min(self.at), self.at)
            }
        };
        let live = |slot: &u16| page.get_cell(*slot).is_some();
        let mut entries = page.live_in(lo..hi);
        if entries == 0 {
            return Ok(None);
        }
        if entries > most {
            // The nearest `most`: arithmetic where no slot is a tombstone.
            let whole = entries == usize::from(hi - lo);
            let most_ = most as u16; // < entries <= u16::MAX
            match self.backward {
                false if whole => hi = lo + most_,
                true if whole => lo = hi - most_,
                false => hi = (lo..hi).filter(live).nth(most - 1).expect("live") + 1,
                true => lo = (lo..hi).rev().filter(live).nth(most - 1).expect("live"),
            }
            entries = most;
        }
        let lowest = (lo..hi).find(live).expect("live");
        let highest = (lo..hi).rev().find(live).expect("live");
        (leaf.entry_at(lowest).expect("live").0).write_to(&mut self.lowest);
        (leaf.entry_at(highest).expect("live").0).write_to(&mut self.highest);
        self.run_end = match self.backward {
            false => hi,
            true => lo,
        };
        Ok(Some(WholeLeaf {
            lowest: &self.lowest,
            highest: &self.highest,
            entries,
        }))
    }

    /// Makes the next leaf in the walk's direction the current one; sets
    /// `done` where the tree ends. A leaf may have no entries at all
    /// (all its entries removed, SPEC §10).
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
                next if !self.seen.insert(next) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "index leaf {next} comes twice along the leaves: their links go round in a loop — file may be corrupt"
                        ),
                    ));
                }
                next => read_node(self.store, next)?,
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
        self.leaves += 1;
        self.next_leaf = leaf.next_page();
        let view = Leaf::of(&leaf);
        // The first leaf is entered where the range begins in the walk's
        // direction, found by a binary search: forward the first key from
        // `start` on, backward the first from `end` on, the walk taking the
        // keys before it (SPEC §172.1).
        let first = self.leaves == 1;
        self.at = match (self.backward, &self.range.end) {
            (false, _) if first => view.first_from(&self.range.start),
            (true, Some(end)) if first => view.first_from(end),
            (true, _) => None,
            (false, _) => Some(view.first()),
        }
        .unwrap_or(leaf.slot_count());
        self.clear = {
            let mut live = (view.first()..leaf.slot_count()).filter_map(|slot| view.entry_at(slot));
            let last = match self.backward {
                true => live.next(),
                false => live.next_back(),
            };
            last.is_none_or(|(key, _)| holds_far_end(&self.range, &key, self.backward))
        };
        self.kept = 0;
        self.offered = None;
        self.leaf = Some(leaf);
        Ok(())
    }

    /// From `page_id` down to a leaf: towards `key`, or along the last
    /// children for `None` — noting each branch passed in `path`.
    fn descend(&mut self, mut page_id: PageId, key: Option<&[u8]>) -> std::io::Result<SlottedPage> {
        // Backward, the path above counts too.
        for _ in self.path.len()..=MAX_DEPTH {
            let page = read_node(self.store, page_id)?;
            match page.page_type() {
                PageType::IndexLeaf | PageType::IndexLeafPacked => return Ok(page),
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
        Err(too_deep(page_id))
    }
}

/// The next entry of `page`, a walk's current leaf, from the cursor `at`
/// in the walk's direction and in `range` — `None` when the leaf is used
/// up, or the range is, which sets `done`. Keys before the range are
/// never stepped over: the first leaf's cursor starts at the range, by a
/// binary search (`Walk::read_next_leaf`). Only the far end is checked,
/// and only on a leaf whose last key in the walk's direction lies past it
/// (`clear` says it doesn't). A tombstoned slot, which a leaf written
/// before §52 may hold, is skipped.
fn next_in_leaf<'p>(
    page: &'p SlottedPage,
    at: &mut u16,
    range: &KeyRange,
    backward: bool,
    done: &mut bool,
    clear: bool,
) -> Option<(KeyParts<'p>, RecordLocation)> {
    let leaf = Leaf::of(page);
    loop {
        let slot = match backward {
            true if *at <= leaf.first() => return None,
            true => {
                *at -= 1;
                *at
            }
            false if *at < page.slot_count() => {
                *at += 1;
                *at - 1
            }
            false => return None,
        };
        let Some((key, loc)) = leaf.entry_at(slot) else {
            continue;
        };
        if !clear && !holds_far_end(range, &key, backward) {
            *done = true; // every later key in this direction is past it too
            return None;
        }
        return Some((key, loc));
    }
}

/// Whether `key` lies inside `range` at the end a walk goes towards:
/// before `end` forward, from `start` on backward.
fn holds_far_end(range: &KeyRange, key: &KeyParts<'_>, backward: bool) -> bool {
    match backward {
        false => range
            .end
            .as_deref()
            .is_none_or(|end| key.cmp(end) == std::cmp::Ordering::Less),
        true => key.cmp(&range.start) != std::cmp::Ordering::Less,
    }
}

impl Iterator for Walk<'_> {
    type Item = std::io::Result<(Vec<u8>, RecordLocation)>;

    /// `step`, its key copied out (SPEC §97).
    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.step()?;
        #[cfg(test)]
        KEYS_COPIED.with(|n| n.set(n.get() + 1));
        Some(entry.map(|(key, loc)| (key.to_vec(), loc)))
    }
}

/// A leaf all inside a walk's range, untouched (`Walk::whole_leaf`).
pub struct WholeLeaf<'w> {
    /// Its lowest key and its highest: every key of the leaf lies
    /// between them.
    pub lowest: &'w [u8],
    pub highest: &'w [u8],
    /// Its live entries.
    pub entries: usize,
}

#[cfg(test)]
thread_local! {
    /// Keys a walk copied out (SPEC §97), for tests: only those it
    /// handed out.
    pub(crate) static KEYS_COPIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Deeper than any tree can be: every page holds at least four entries
/// (`MAX_KEY_LEN`), so each level multiplies the leaves by five or more,
/// and 28 levels would already hold more pages than a file can have. A
/// descent past this has gone round a loop of damaged child pointers
/// (SPEC §55).
const MAX_DEPTH: usize = 64;

fn too_deep(page_id: PageId) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "index page {page_id} is more than {MAX_DEPTH} levels down: its child pointers go round in a loop — file may be corrupt"
        ),
    )
}

/// Reads B-tree page `page_id`, which must be a leaf or a branch whose
/// cells are all long enough for their entries (SPEC §55): the entry
/// decoders trust that, so a damaged page is an error here rather than a
/// panic there. The layout check holds the cells' lengths (SPEC §101),
/// so a page from the cache, checked as it went in, isn't checked again.
fn read_node(store: &dyn PageStore, page_id: PageId) -> std::io::Result<SlottedPage> {
    let page = SlottedPage::from_bytes(store.read_page(page_id)?).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("index page {page_id}: {e} — file may be corrupt"),
        )
    })?;
    match page.page_type() {
        PageType::IndexLeaf | PageType::IndexLeafPacked | PageType::IndexBranch => Ok(page),
        other => Err(corrupt_page_type(page_id, other)),
    }
}

/// Descends from `root` to the leaf that holds (or would hold) `key`.
fn find_leaf(
    store: &dyn PageStore,
    root: PageId,
    key: &[u8],
) -> std::io::Result<(PageId, SlottedPage)> {
    let mut page_id = root;
    for _ in 0..=MAX_DEPTH {
        let page = read_node(store, page_id)?;
        match page.page_type() {
            PageType::IndexLeaf | PageType::IndexLeafPacked => return Ok((page_id, page)),
            PageType::IndexBranch => page_id = find_child(&page, key),
            other => return Err(corrupt_page_type(page_id, other)),
        }
    }
    Err(too_deep(page_id))
}

/// Reads `page_id` and dispatches to the leaf or branch insert logic —
/// the single recursive entry point every level of the descent calls.
fn insert_into(
    store: &mut dyn PageStore,
    page_id: PageId,
    key: &[u8],
    loc: RecordLocation,
    depth: usize,
) -> std::io::Result<InsertOutcome> {
    if depth > MAX_DEPTH {
        return Err(too_deep(page_id));
    }
    let page = read_node(store, page_id)?;
    match page.page_type() {
        PageType::IndexLeaf | PageType::IndexLeafPacked => {
            insert_into_leaf(store, page_id, page, key, loc)
        }
        PageType::IndexBranch => insert_into_branch(store, page_id, page, key, loc, depth),
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
    // In place if it fits: before the first key not below it (SPEC §52).
    // Changed in place, not copied, if the batch changed it already
    // (SPEC §151). Only into a packed leaf with its anchor: a plain one,
    // as files before format 14 have, is rewritten packed below.
    let leaf = Leaf::of(&page);
    let slot = leaf.first_from(key).unwrap_or(page.slot_count());
    if let (Some(anchor), true) = (leaf.anchor, page.slot_count() > 0) {
        let entry = encode_packed(anchor, key, loc);
        if page.fits(entry.len()) {
            store.change_page(page_id, page, &mut |page| {
                assert!(page.insert_cell_at(slot, &entry))
            })?;
            return Ok(InsertOutcome::Done);
        }
    }

    // Full: rebuild it from its entries, with an anchor in their middle,
    // which drops any tombstones an older version left, or split it.
    let mut entries = leaf.entries();
    let next_page = page.next_page(); // this leaf's sibling link, preserved either way

    let pos = entries.partition_point(|(k, _)| k.as_slice() < key);
    entries.insert(pos, (key.to_vec(), loc));

    if let Some(page) = build_leaf_page(&entries, next_page) {
        store.write_page(page_id, page.into_page())?;
        return Ok(InsertOutcome::Done);
    }

    // Doesn't fit: split in place, by bytes (SPEC §28.2) — unless the new
    // key goes past the end of the rightmost leaf. Then only it moves to
    // the new page, and this one stays full: keys arriving in order (ids
    // in time order, a compaction's sorted rebuild) would otherwise leave
    // every leaf half empty, since nothing lands left of them again
    // (SPEC §41). Each half gets an anchor of its own, so the byte count
    // is only a guide: where a half doesn't fit, other points are tried.
    let appends = pos == entries.len() - 1 && next_page == 0;
    let n = entries.len();
    let by_bytes = byte_midpoint(entries.iter().map(|(k, _)| k.len() + 10));
    let candidates = appends
        .then_some(pos)
        .into_iter()
        .chain([by_bytes, n / 2])
        .chain(1..n);
    let new_right_id = store.allocate_page()?;
    // Appending to a packed leaf, it stays as it is but for its link.
    let kept = (appends && leaf.anchor.is_some()).then(|| {
        let mut kept = page.clone();
        kept.set_next_page(new_right_id);
        kept
    });
    for mid in candidates.filter(|&mid| 0 < mid && mid < n) {
        let left = match &kept {
            Some(kept) if mid == pos => Some(kept.clone()),
            _ => build_leaf_page(&entries[..mid], new_right_id),
        };
        let Some(left) = left else {
            continue;
        };
        let Some(right) = build_leaf_page(&entries[mid..], next_page) else {
            continue;
        };
        // Smallest key of the right half, copied up (leaves keep it too).
        let separator = entries[mid].0.clone();
        store.write_page(page_id, left.into_page())?;
        store.write_page(new_right_id, right.into_page())?;
        return Ok(InsertOutcome::Split {
            separator,
            new_right: new_right_id,
        });
    }
    Err(std::io::Error::other(format!(
        "index leaf {page_id} can't be split so that each half fits"
    )))
}

fn insert_into_branch(
    store: &mut dyn PageStore,
    page_id: PageId,
    page: SlottedPage,
    key: &[u8],
    loc: RecordLocation,
    depth: usize,
) -> std::io::Result<InsertOutcome> {
    // The first separator above `key` routes it, as in `find_child`;
    // `None` is the rightmost child.
    let (slot, child_id) =
        match first_slot_where(&page, 0, |cell| decode_branch_entry(cell).0 > key) {
            Some(slot) => {
                let (_separator, child) =
                    decode_branch_entry(page.get_cell(slot).expect("a live slot"));
                (Some(slot), child)
            }
            None => (None, page.next_page()),
        };

    let outcome = insert_into(store, child_id, key, loc, depth + 1)?;
    let InsertOutcome::Split {
        separator,
        new_right,
    } = outcome
    else {
        return Ok(InsertOutcome::Done); // child absorbed the insert, this page is unchanged
    };

    // In place if it fits (SPEC §52): the separator goes in front of
    // the child that split, and what routed there now routes to its new
    // right half.
    let routing = encode_branch_entry(&separator, child_id);
    if page.fits(routing.len()) {
        match slot {
            Some(slot) => {
                let (next_separator, _child) =
                    decode_branch_entry(page.get_cell(slot).expect("a live slot"));
                let rerouted = encode_branch_entry(next_separator, new_right);
                store.change_page(page_id, page, &mut |page| {
                    assert!(page.insert_cell_at(slot, &routing));
                    // The same length, so in place.
                    assert!(page.update_cell(slot + 1, &rerouted));
                })?;
            }
            None => store.change_page(page_id, page, &mut |page| {
                assert!(page.insert_cell_at(page.slot_count(), &routing));
                page.set_next_page(new_right);
            })?,
        }
        return Ok(InsertOutcome::Done);
    }

    // Full: split it.
    let mut entries: Vec<(Vec<u8>, PageId)> = page
        .iter_cells()
        .map(|(_slot, c)| {
            let (k, child) = decode_branch_entry(c);
            (k.to_vec(), child)
        })
        .collect();
    let mut rightmost = page.next_page();
    let idx = entries.partition_point(|(k, _)| k.as_slice() <= key);

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
        store.write_page(page_id, page.into_page())?;
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
    store.write_page(page_id, left.into_page())?;
    store.write_page(new_right_id, right.into_page())?;

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
    // The root's page, shared, not copied (SPEC §67): the root itself
    // gets a new page next.
    let old_root = store.read_page(root)?;
    store.write_page(left_id, old_root)?;

    let mut new_root = SlottedPage::new(PageType::IndexBranch);
    new_root.set_next_page(new_right);
    new_root
        .insert_cell(&encode_branch_entry(separator, left_id))
        .expect("a single entry always fits in a fresh page");
    store.write_page(root, new_root.into_page())
}

/// A packed leaf of `entries`, its anchor the middle one's key; `None`
/// if they don't fit.
fn build_leaf_page(
    entries: &[(Vec<u8>, RecordLocation)],
    next_page: PageId,
) -> Option<SlottedPage> {
    let mut page = SlottedPage::new(PageType::IndexLeafPacked);
    page.set_next_page(next_page);
    let anchor = anchor_of(entries);
    page.insert_cell(&encode_anchor(anchor))?;
    for (key, loc) in entries {
        page.insert_cell(&encode_packed(anchor, key, *loc))?;
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
/// is: `insert_into_branch` puts each separator in at its place.
fn find_child(page: &SlottedPage, target: &[u8]) -> PageId {
    match first_slot_where(page, 0, |cell| decode_branch_entry(cell).0 > target) {
        Some(slot) => decode_branch_entry(page.get_cell(slot).expect("a live slot")).1,
        None => page.next_page(),
    }
}

/// The first live slot of a B-tree page whose cell meets `meets`, by a
/// binary search (SPEC §150): a page's live cells are in key order, so
/// `meets` on a key bound (`key >= x`, `key > x`) fails for every cell
/// before that slot and holds from it on. A probe that lands on a
/// tombstone, which a leaf written before §52 may hold, steps on to the
/// next live slot; a stretch of them is passed over one by one.
fn first_slot_where(page: &SlottedPage, from: u16, meets: impl Fn(&[u8]) -> bool) -> Option<u16> {
    let (mut lo, mut hi) = (from, page.slot_count());
    let mut found = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        // The first live slot from `mid` on: the slots between are
        // tombstones, so the answer is below `mid` or is this one.
        let Some((slot, cell)) = (mid..hi).find_map(|slot| page.get_cell(slot).map(|c| (slot, c)))
        else {
            hi = mid;
            continue;
        };
        if meets(cell) {
            found = Some(slot);
            hi = mid;
        } else {
            lo = slot + 1;
        }
    }
    found
}

/// For tests that need a damaged index: the entry with key `was`, in
/// the tree at `root`, given the key `now` where it is, out of order if
/// that's where `now` goes.
#[cfg(test)]
pub(crate) fn rewrite_key_in_place(
    store: &mut dyn PageStore,
    root: PageId,
    was: &[u8],
    now: &[u8],
) -> std::io::Result<()> {
    let (page_id, page) = find_leaf(store, root, was)?;
    let mut entries = Leaf::of(&page).entries();
    for (key, _) in &mut entries {
        if key == was {
            *key = now.to_vec();
        }
    }
    let rebuilt = build_leaf_page(&entries, page.next_page()).expect("it fit before");
    store.write_page(page_id, rebuilt.into_page())
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
            .write_page(
                root,
                SlottedPage::new(PageType::IndexLeafPacked).into_page(),
            )
            .unwrap();
        root
    }

    /// A leaf as files before format 14 hold them: every key whole.
    fn build_plain_leaf_page(
        entries: &[(Vec<u8>, RecordLocation)],
        next_page: PageId,
    ) -> Option<SlottedPage> {
        let mut page = SlottedPage::new(PageType::IndexLeaf);
        page.set_next_page(next_page);
        for (key, loc) in entries {
            page.insert_cell(&super::super::leaf::encode_index_entry(key, *loc))?;
        }
        Some(page)
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
        let mut sizes = vec![Leaf::of(&page).len()];
        while page.next_page() != 0 {
            page = SlottedPage::from_bytes(store.read_page(page.next_page()).unwrap()).unwrap();
            sizes.push(Leaf::of(&page).len());
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
        fn read_page(&self, id: PageId) -> std::io::Result<crate::storage::Page> {
            self.reads.set(self.reads.get() + 1);
            self.store.read_page(id)
        }
        fn try_read_page(&self, id: PageId) -> std::io::Result<Option<crate::storage::Page>> {
            self.reads.set(self.reads.get() + 1);
            self.store.try_read_page(id)
        }
        fn write_page(&mut self, _id: PageId, _data: crate::storage::Page) -> std::io::Result<()> {
            unreachable!("read only")
        }
        fn free_page(&mut self, _id: PageId) -> std::io::Result<()> {
            unreachable!("read only")
        }
    }

    /// SPEC §97: a walk copies a key only to hand its entry out — 5
    /// keys for 5 entries, from a range that starts or ends mid-leaf,
    /// either way, and across leaves as many as it hands out.
    #[test]
    fn a_walk_copies_only_the_keys_it_hands_out() {
        let (_dir, mut store, mut index) = fresh();
        let key = |i: u32| [&i.to_be_bytes()[..], &[7u8; 12]].concat();
        for i in 0..1000u32 {
            let loc = RecordLocation {
                page: i as u64,
                slot: 0,
            };
            index.insert(&mut store, &key(i), loc).unwrap();
        }
        let copied = |range: KeyRange, backward: bool, take: usize| {
            KEYS_COPIED.with(|n| n.set(0));
            let pages: Vec<u64> = (index.walk(&store, range, backward).take(take))
                .map(|entry| entry.unwrap().1.page)
                .collect();
            (pages, KEYS_COPIED.with(|n| n.get()))
        };
        let middle = KeyRange {
            start: key(301),
            end: Some(key(707)),
        };
        assert_eq!(
            copied(middle.clone(), false, 5),
            (vec![301, 302, 303, 304, 305], 5)
        );
        assert_eq!(
            copied(middle.clone(), true, 5),
            (vec![706, 705, 704, 703, 702], 5)
        );
        assert_eq!(copied(KeyRange::everything(), true, 1), (vec![999], 1));
        // Over several leaves (fewer than 400 entries each), and to the end.
        let (pages, keys) = copied(middle.clone(), false, 400);
        assert_eq!((pages.len(), keys), (400, 400));
        assert_eq!(copied(middle, true, usize::MAX).1, 406);
    }

    /// SPEC §97: stopped anywhere, a walk has handed out the start of the
    /// range in its direction and copied a key for each entry and no
    /// more — over leaves emptied by removals, and leaves with
    /// tombstoned slots as versions before §52 left them.
    #[test]
    fn a_walk_stopped_anywhere_copied_what_it_handed_out() {
        let (_dir, mut store, mut index) = fresh();
        let mut rng = XorShift(0x1319_8A2E_0370_7344);
        let mut expected = std::collections::BTreeMap::new();
        for i in 0..4000u32 {
            let len = 1 + rng.below(30);
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
        for key in keys[500..1200].iter().chain(keys.iter().step_by(5)) {
            index.remove(&mut store, key).unwrap();
            expected.remove(key);
        }
        // Every third leaf as a version before §52 left it: plain, with
        // every fourth slot tombstoned.
        let leaves: Vec<PageId> = (index.pages(&store).unwrap().into_iter())
            .filter(|id| {
                let page = SlottedPage::from_bytes(store.read_page(*id).unwrap()).unwrap();
                page.page_type() != PageType::IndexBranch
            })
            .collect();
        let mut tombstoned = 0;
        for id in leaves.iter().step_by(3) {
            let page = SlottedPage::from_bytes(store.read_page(*id).unwrap()).unwrap();
            let mut page =
                build_plain_leaf_page(&Leaf::of(&page).entries(), page.next_page()).unwrap();
            for slot in (0..page.slot_count()).step_by(4) {
                let (key, _) = decode_index_entry(page.get_cell(slot).unwrap());
                expected.remove(key);
                page.delete_cell(slot);
                tombstoned += 1;
            }
            store.write_page(*id, page.into_page()).unwrap();
        }
        assert!(tombstoned > 50, "{tombstoned}");
        let bound = |rng: &mut XorShift| -> Vec<u8> {
            match rng.below(3) {
                0 => keys[rng.below(keys.len())].clone(),
                _ => (0..rng.below(4)).map(|_| rng.below(256) as u8).collect(),
            }
        };
        for _ in 0..400 {
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
            let backward = rng.below(2) == 0;
            let mut want: Vec<(Vec<u8>, RecordLocation)> = expected
                .iter()
                .filter(|(k, _)| **k >= range.start && range.end.as_ref().is_none_or(|e| *k < e))
                .map(|(k, l)| (k.clone(), *l))
                .collect();
            if backward {
                want.reverse();
            }
            let stop = rng.below(want.len() + 2);
            KEYS_COPIED.with(|n| n.set(0));
            let got: Vec<_> = (index.walk(&store, range.clone(), backward).take(stop))
                .map(Result::unwrap)
                .collect();
            assert_eq!(got, want[..stop.min(want.len())], "{range:?} {backward}");
            assert_eq!(KEYS_COPIED.with(|n| n.get()), got.len(), "{range:?}");
            by_runs(&index, &store, &range, backward, &want, &mut rng);
        }
    }

    /// A walk that passes runs of at most a random number of entries
    /// (`Walk::run`), declines some to step instead, and steps between:
    /// each run the next `want` entries in the walk's direction, `want`
    /// all of them in all.
    fn by_runs(
        index: &BTreeIndex,
        store: &FileStore,
        range: &KeyRange,
        backward: bool,
        want: &[(Vec<u8>, RecordLocation)],
        rng: &mut XorShift,
    ) {
        let mut walk = index.walk(store, range.clone(), backward);
        let mut at = 0;
        loop {
            let most = [1, 2, 7, usize::MAX][rng.below(4)];
            let decline = rng.below(4) == 0;
            if let Some(run) = walk.run(most).unwrap() {
                let n = run.entries;
                assert!(n >= 1 && n <= most, "{n} of {most}");
                let (near, far) = (&want[at].0, &want[at + n - 1].0);
                let (lowest, highest) = if backward { (far, near) } else { (near, far) };
                assert_eq!((run.lowest, run.highest), (&lowest[..], &highest[..]));
                if !decline {
                    walk.pass_leaf();
                    at += n;
                    continue;
                }
            }
            match walk.step() {
                Some(entry) => {
                    let (key, loc) = entry.unwrap();
                    assert_eq!((key, loc), (&want[at].0[..], want[at].1), "{range:?}");
                    at += 1;
                }
                None => break,
            }
        }
        assert_eq!(at, want.len(), "{range:?} {backward}");
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
            assert_eq!(index.count(&store, range.clone()).unwrap(), want.len());
            for backward in [false, true] {
                let (mut stepped, whole) = by_leaves(&index, &store, &range, backward);
                if backward {
                    stepped.reverse();
                }
                let stepped_in_want = stepped.iter().all(|entry| want.contains(entry));
                assert!(stepped_in_want, "{range:?}");
                assert_eq!(stepped.len() + whole, want.len(), "{range:?}");
            }
        }
    }

    /// A walk that passes over every leaf it can whole (SPEC §101.3),
    /// checking that each lies inside `range`: the entries it stepped
    /// through, and how many it passed over.
    fn by_leaves(
        index: &BTreeIndex,
        store: &FileStore,
        range: &KeyRange,
        backward: bool,
    ) -> (Vec<(Vec<u8>, RecordLocation)>, usize) {
        let mut walk = index.walk(store, range.clone(), backward);
        let (mut stepped, mut whole) = (Vec::new(), 0);
        loop {
            if let Some(leaf) = walk.whole_leaf().unwrap() {
                assert!(leaf.lowest >= range.start.as_slice());
                assert!(range.end.as_deref().is_none_or(|end| leaf.highest < end));
                assert!(leaf.lowest <= leaf.highest);
                whole += leaf.entries;
                walk.pass_leaf();
                continue;
            }
            match walk.step() {
                Some(entry) => {
                    let (key, loc) = entry.unwrap();
                    stepped.push((key.to_vec(), loc));
                }
                None => return (stepped, whole),
            }
        }
    }

    #[test]
    fn a_count_looks_only_at_the_keys_of_the_leaves_at_its_ends() {
        let (_dir, mut store, mut index) = fresh();
        let loc = RecordLocation { page: 1, slot: 0 };
        let key = |i: u32| format!("{i:05}{}", "x".repeat(40)).into_bytes();
        for i in 0..5000 {
            index.insert(&mut store, &key(i), loc).unwrap();
        }
        let range = KeyRange {
            start: key(1000),
            end: Some(key(4000)),
        };
        let (stepped, whole) = by_leaves(&index, &store, &range, false);
        assert_eq!(stepped.len() + whole, 3000);
        // Some leaves are passed whole, and of the rest at most the two
        // at the range's ends are stepped through.
        assert!(whole > 2000, "{whole}");
        let per_leaf = crate::storage::USABLE_PAGE_SIZE / key(0).len();
        assert!(stepped.len() < 2 * per_leaf, "{}", stepped.len());
        assert_eq!(index.count(&store, range).unwrap(), 3000);
    }

    /// `count_at_most` counts through as many leaves as it may, and says
    /// whether that was all (SPEC §133.3).
    #[test]
    fn a_count_at_most_stops_after_its_leaves() {
        let (_dir, mut store, mut index) = fresh();
        let loc = RecordLocation { page: 1, slot: 0 };
        let key = |i: u32| format!("{i:05}{}", "x".repeat(40)).into_bytes();
        for i in 0..5000 {
            index.insert(&mut store, &key(i), loc).unwrap();
        }
        let range = KeyRange {
            start: key(1000),
            end: Some(key(4000)),
        };
        let count = |leaves| index.count_at_most(&store, range.clone(), leaves).unwrap();
        let all = count(usize::MAX);
        assert_eq!((all.entries, all.whole), (3000, true));
        assert!(all.leaves > 3, "{}", all.leaves);
        let enough = count(all.leaves);
        assert_eq!((enough.entries, enough.whole), (3000, true));
        let short = count(all.leaves - 1);
        assert!(!short.whole);
        assert!(
            short.entries < 3000 && short.entries > 2000,
            "{}",
            short.entries
        );
        let per_leaf = crate::storage::USABLE_PAGE_SIZE / key(0).len();
        let one = count(1);
        assert!(!one.whole);
        assert!(
            one.entries > 0 && one.entries <= per_leaf,
            "{}",
            one.entries
        );
        let none = count(0);
        assert_eq!((none.entries, none.whole), (0, false));
        // A range with no end, its last leaf passed whole: at a cap of
        // as many leaves, whole, by a look one entry ahead.
        let rest = KeyRange {
            start: key(1000),
            end: None,
        };
        let open = |leaves| index.count_at_most(&store, rest.clone(), leaves).unwrap();
        let all = open(usize::MAX);
        let enough = open(all.leaves);
        assert_eq!((enough.entries, enough.whole), (4000, true));
        // A range inside one leaf is whole in one.
        let small = KeyRange {
            start: key(10),
            end: Some(key(12)),
        };
        let in_one = index.count_at_most(&store, small, 1).unwrap();
        assert_eq!((in_one.entries, in_one.whole), (2, true));
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
        // Full, give or take what the keys share with each leaf's anchor.
        let full = *sizes.iter().max().unwrap();
        assert!(sizes.len() > 5, "{sizes:?}");
        assert!(
            sizes[..sizes.len() - 1].iter().all(|&n| n * 10 >= full * 9),
            "{sizes:?}"
        );
        assert_eq!(sizes.iter().sum::<usize>(), keys.len());

        let (_dir, mut store, mut index) = fresh();
        for key in keys.iter().rev() {
            index.insert(&mut store, key, loc).unwrap();
        }
        let sizes = leaf_sizes(&store, &index);
        // The first leaf, still filling, aside.
        assert!(sizes[1..].iter().all(|&n| n < full * 2 / 3), "{sizes:?}");
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
    /// Every page of the tree, as bytes.
    fn tree_bytes(store: &FileStore, index: &BTreeIndex) -> Vec<Vec<u8>> {
        index
            .pages(store)
            .unwrap()
            .into_iter()
            .map(|id| store.read_page(id).unwrap().to_vec())
            .collect()
    }

    /// Found by fuzzing (SPEC §55): a leaf or branch cell too short for
    /// its entry is an error from every path that reads the page, not a
    /// panic in the decoder.
    #[test]
    fn a_cell_too_short_for_its_entry_is_an_error() {
        let loc = RecordLocation { page: 1, slot: 0 };
        for leaf in [true, false] {
            let (_dir, mut store, mut index) = fresh();
            for i in 0..400u16 {
                index.insert(&mut store, &id_from(i), loc).unwrap();
            }
            let root = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
            assert_eq!(root.page_type(), PageType::IndexBranch);
            let target = match leaf {
                true => decode_branch_entry(root.get_cell(0).unwrap()).1,
                false => index.root,
            };
            let mut page = SlottedPage::from_bytes(store.read_page(target).unwrap()).unwrap();
            let message = match leaf {
                // A packed leaf's first entry, its varints never ending.
                true => {
                    assert_eq!(page.page_type(), PageType::IndexLeafPacked);
                    let mut bytes = page.into_page();
                    let at = 13 + 4;
                    let offset = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
                    let length = u16::from_le_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
                    bytes.make_mut()[offset..offset + length].fill(0x80);
                    store.write_page(target, bytes).unwrap();
                    "varint"
                }
                false => {
                    page.remove_cell_at(0);
                    assert!(page.insert_cell_at(0, &[1, 2, 3]));
                    store.write_page(target, page.into_page()).unwrap();
                    "shorter than any entry"
                }
            };

            let invalid = |e: std::io::Error| {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
                assert!(e.to_string().contains(message), "{e}");
            };
            invalid(index.lookup(&store, &id_from(0)).unwrap_err());
            invalid(index.scan(&store).unwrap_err());
            invalid(index.insert(&mut store, &id_from(0), loc).unwrap_err());
            invalid(index.pages(&store).unwrap_err());
            let walked: std::io::Result<Vec<_>> = index
                .walk(
                    &store,
                    KeyRange {
                        start: Vec::new(),
                        end: None,
                    },
                    true,
                )
                .collect();
            invalid(walked.unwrap_err());
        }
    }

    /// Found in review: a packed leaf whose slot 0, the anchor, is a
    /// tombstone is damage. Its next entry, sharing nothing, would pass
    /// for an anchor, while reads took an empty one and lost the bytes
    /// every other entry shares.
    #[test]
    fn a_packed_leaf_without_its_anchor_is_an_error() {
        let loc = RecordLocation { page: 1, slot: 0 };
        let (_dir, mut store, mut index) = fresh();
        for i in 0..400u16 {
            index.insert(&mut store, &id_from(i), loc).unwrap();
        }
        let root = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
        let target = decode_branch_entry(root.get_cell(0).unwrap()).1;
        let page = SlottedPage::from_bytes(store.read_page(target).unwrap()).unwrap();
        assert_eq!(page.page_type(), PageType::IndexLeafPacked);
        let mut bytes = page.into_page();
        let raw = bytes.make_mut();
        // Slot 0's length to 0, a tombstone; slot 1's entry to share
        // nothing, so its cell starts with 0 as an anchor's does.
        raw[13 + 2..13 + 4].fill(0);
        let offset = u16::from_le_bytes([raw[17], raw[18]]) as usize;
        raw[offset] = 0;
        store.write_page(target, bytes).unwrap();

        let invalid = |e: std::io::Error| {
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
            assert!(e.to_string().contains("anchor"), "{e}");
        };
        invalid(index.lookup(&store, &id_from(0)).unwrap_err());
        invalid(index.scan(&store).unwrap_err());
        invalid(index.insert(&mut store, &id_from(0), loc).unwrap_err());
        let walked: std::io::Result<Vec<_>> = index
            .walk(
                &store,
                KeyRange {
                    start: Vec::new(),
                    end: None,
                },
                true,
            )
            .collect();
        invalid(walked.unwrap_err());
    }

    /// Found by fuzzing (SPEC §55): a child pointer back to the root, or
    /// a leaf linked back to an earlier one, is an error from every path
    /// that follows it, not a loop that never ends.
    #[test]
    fn pointers_that_go_round_in_a_loop_are_errors() {
        let loc = RecordLocation { page: 1, slot: 0 };
        let everything = || KeyRange {
            start: Vec::new(),
            end: None,
        };
        let invalid = |e: std::io::Error, says: &str| {
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
            assert!(e.to_string().contains(says), "{e}");
        };

        // The root's last child is the root itself.
        let (_dir, mut store, mut index) = fresh();
        for i in 0..400u16 {
            index.insert(&mut store, &id_from(i), loc).unwrap();
        }
        let mut root = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
        assert_eq!(root.page_type(), PageType::IndexBranch);
        root.set_next_page(index.root);
        store.write_page(index.root, root.into_page()).unwrap();
        invalid(
            index.lookup(&store, &[0xFF; 16]).unwrap_err(),
            "levels down",
        );
        invalid(
            index.insert(&mut store, &[0xFF; 16], loc).unwrap_err(),
            "levels down",
        );
        // Backward starts down the last child; forward starts at the
        // first leaf and goes along the leaves, never through it.
        let walked: std::io::Result<Vec<_>> = index.walk(&store, everything(), true).collect();
        invalid(walked.unwrap_err(), "levels down");

        // The last leaf links back to the first.
        let (_dir, mut store, mut index) = fresh();
        for i in 0..400u16 {
            index.insert(&mut store, &id_from(i), loc).unwrap();
        }
        let (first, _) = find_leaf(&store, index.root, &[]).unwrap();
        let (last, mut page) = find_leaf(&store, index.root, &[0xFF; 16]).unwrap();
        assert_ne!(first, last);
        page.set_next_page(first);
        store.write_page(last, page.into_page()).unwrap();
        let walked: std::io::Result<Vec<_>> = index.walk(&store, everything(), false).collect();
        invalid(walked.unwrap_err(), "comes twice along the leaves");
    }

    /// Keys go in and out of a page in place (SPEC §52): a removed key's
    /// bytes are zeroed, not left for the next rebuild of its leaf.
    #[test]
    fn removed_keys_leave_no_bytes_behind() {
        let (_dir, mut store, mut index) = fresh();
        let loc = RecordLocation { page: 1, slot: 0 };
        let key = |i: u32| format!("key{i:05}-secret{}", i % 7).into_bytes();
        for i in 0..2000 {
            index
                .insert(&mut store, &key(i * 7919 % 2000), loc)
                .unwrap();
        }
        let has = |store: &FileStore, index: &BTreeIndex, needle: &[u8]| {
            tree_bytes(store, index)
                .iter()
                .any(|page| page.windows(needle.len()).any(|w| w == needle))
        };
        assert!(has(&store, &index, b"secret3"));
        for i in (3..2000).step_by(7) {
            index.remove(&mut store, &key(i)).unwrap();
        }
        assert!(!has(&store, &index, b"secret3"));
        let left: Vec<Vec<u8>> = index
            .scan(&store)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let expected: Vec<Vec<u8>> = (0..2000).filter(|i| i % 7 != 3).map(key).collect();
        assert_eq!(left, expected);
    }

    /// Versions before SPEC §52 left tombstones on a leaf when a key was
    /// removed; inserts around them keep the keys in order, and a full
    /// leaf is rebuilt without them before it splits.
    #[test]
    fn leaves_with_tombstones_from_older_versions_take_inserts() {
        let (_dir, mut store, mut index) = fresh();
        let loc = RecordLocation { page: 1, slot: 0 };
        let key = |i: u32| format!("{i:04}{}", "x".repeat(60)).into_bytes();
        for i in (0..100).map(|i| i * 2) {
            index.insert(&mut store, &key(i), loc).unwrap();
        }
        // As a remove used to leave it: every third slot tombstoned.
        // As an older version wrote it: plain, then with tombstones.
        let page = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
        let mut page = build_plain_leaf_page(&Leaf::of(&page).entries(), 0).unwrap();
        for slot in (0..page.slot_count()).step_by(3) {
            page.delete_cell(slot);
        }
        store.write_page(index.root, page.into_page()).unwrap();
        let mut expected: std::collections::BTreeSet<Vec<u8>> = (0..100)
            .filter(|i| i % 3 != 0)
            .map(|i| key(i * 2))
            .collect();
        // Counted whole, the tombstones are left out (SPEC §101.3).
        let everything = KeyRange::everything();
        assert_eq!(index.count(&store, everything).unwrap(), expected.len());

        for i in (0..400).map(|i| i * 2 + 1) {
            index.insert(&mut store, &key(i), loc).unwrap();
            expected.insert(key(i));
        }
        let keys: Vec<Vec<u8>> = index
            .scan(&store)
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, expected.into_iter().collect::<Vec<_>>());
    }

    /// The binary search finds the slot the linear scan before SPEC §150
    /// found, for every key bound, on leaves and branches whose
    /// tombstones lie anywhere: none, scattered, in a run at the start,
    /// the middle or the end, all of them, all but one.
    #[test]
    fn the_search_finds_what_a_scan_finds_around_tombstones() {
        let key = |i: usize| format!("{:04}", i * 2).into_bytes();
        let n = 60;
        let patterns: Vec<(&str, Vec<u16>)> = vec![
            ("none", vec![]),
            ("every third", (0..n).step_by(3).collect()),
            ("a run at the start", (0..20).collect()),
            ("a run in the middle", (20..45).collect()),
            ("a run at the end", (40..n).collect()),
            ("all", (0..n).collect()),
            ("all but one", (0..n).filter(|&s| s != 33).collect()),
        ];
        let leaf_entries: Vec<(Vec<u8>, RecordLocation)> = (0..n as usize)
            .map(|i| {
                (
                    key(i),
                    RecordLocation {
                        page: 7,
                        slot: i as u16,
                    },
                )
            })
            .collect();
        let branch_entries: Vec<(Vec<u8>, PageId)> =
            (0..n as usize).map(|i| (key(i), 100 + i as u64)).collect();
        for (name, deleted) in &patterns {
            let mut leaf = build_plain_leaf_page(&leaf_entries, 0).unwrap();
            let mut branch = build_branch_page(&branch_entries, 999).unwrap();
            for &slot in deleted {
                leaf.delete_cell(slot);
                branch.delete_cell(slot);
            }
            // Probes below, at, between and above every key.
            for probe in (0..2 * n as usize + 3).map(|p| format!("{p:04}").into_bytes()) {
                let probe = probe.as_slice();
                let scan = |page: &SlottedPage, meets: &dyn Fn(&[u8]) -> bool| {
                    page.iter_cells()
                        .find(|(_slot, cell)| meets(cell))
                        .map(|(slot, _)| slot)
                };
                let at_least = |cell: &[u8]| decode_index_entry(cell).0 >= probe;
                assert_eq!(
                    first_slot_where(&leaf, 0, at_least),
                    scan(&leaf, &at_least),
                    "{name}, leaf, >= {probe:?}"
                );
                let above = |cell: &[u8]| decode_branch_entry(cell).0 > probe;
                assert_eq!(
                    first_slot_where(&branch, 0, above),
                    scan(&branch, &above),
                    "{name}, branch, > {probe:?}"
                );
                let found = leaf
                    .iter_cells()
                    .find(|(_slot, cell)| decode_index_entry(cell).0 == probe)
                    .map(|(slot, _)| slot);
                assert_eq!(
                    Leaf::of(&leaf).slot_of(probe),
                    found,
                    "{name}, leaf, == {probe:?}"
                );
            }
        }
    }

    /// `lookup` and `remove` on a leaf an older version left with
    /// tombstones (SPEC §52): every live key found and removed, none of
    /// the tombstoned ones.
    #[test]
    fn leaves_with_tombstones_from_older_versions_take_lookups_and_removes() {
        let (_dir, mut store, mut index) = fresh();
        let key = |i: u32| format!("{i:04}{}", "x".repeat(60)).into_bytes();
        for i in 0..100 {
            index
                .insert(
                    &mut store,
                    &key(i),
                    RecordLocation {
                        page: 1,
                        slot: i as u16,
                    },
                )
                .unwrap();
        }
        // As an older version wrote it: plain, then with tombstones.
        let page = SlottedPage::from_bytes(store.read_page(index.root).unwrap()).unwrap();
        let mut page = build_plain_leaf_page(&Leaf::of(&page).entries(), 0).unwrap();
        for slot in (0..page.slot_count()).step_by(3) {
            page.delete_cell(slot);
        }
        store.write_page(index.root, page.into_page()).unwrap();
        for i in 0..100 {
            let want = (i % 3 != 0).then_some(RecordLocation {
                page: 1,
                slot: i as u16,
            });
            assert_eq!(index.lookup(&store, &key(i)).unwrap(), want, "{i}");
        }
        for i in (0..100).filter(|i| i % 3 != 0) {
            index.remove(&mut store, &key(i)).unwrap();
            assert_eq!(index.lookup(&store, &key(i)).unwrap(), None, "{i}");
        }
        assert_eq!(index.count(&store, KeyRange::everything()).unwrap(), 0);
    }

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
