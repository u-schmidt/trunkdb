use super::{Page, PageId, PageType, USABLE_PAGE_SIZE};

// Header layout (13 bytes), then the slot directory, then cells packed
// backward from the end of the page. Shared by Catalog, Data and IndexLeaf
// pages — they differ only in how their cell bytes are interpreted, which
// is a concern for whoever calls insert_cell/get_cell, not this module.
//
//   [0]      page_type: u8
//   [1..9)   next_page: u64  (0 = none/end — chaining, e.g. index-leaf pages)
//   [9..11)  slot_count: u16
//   [11..13) data_start: u16 (offset where packed cell bytes currently begin)
//   [13..)   slot directory: slot_count * {offset: u16, length: u16}
const HEADER_LEN: usize = 13;
const SLOT_LEN: usize = 4;

/// The shortest cell a page of type `page_type` (its byte) may hold: an
/// index page's cells are each long enough for an entry, which the entry
/// decoders trust (SPEC §55).
fn min_cell(page_type: u8) -> usize {
    match page_type {
        t if t == PageType::IndexLeaf as u8 => crate::index::MIN_INDEX_ENTRY_LEN,
        t if t == PageType::IndexBranch as u8 => crate::index::MIN_BRANCH_ENTRY_LEN,
        // The anchor: its marker byte; `check_packed` checks the rest.
        t if t == PageType::IndexLeafPacked as u8 => 1,
        _ => 0,
    }
}

impl PageType {
    pub fn is_slotted(&self) -> bool {
        matches!(
            self,
            PageType::Catalog
                | PageType::Data
                | PageType::IndexBranch
                | PageType::IndexLeaf
                | PageType::IndexLeafPacked
        )
    }
}

/// A page interpreted as a slot directory (growing forward from the
/// header) plus variable-length cells (packed backward from the end of the
/// page) — the standard layout PostgreSQL heap pages and SQLite B-tree
/// pages both use. A cell's identity is its slot index, not its byte
/// offset, so a cell can be relocated within the page later (compaction)
/// without invalidating anything that references it by slot.
///
/// Its bytes are a shared `Page` (SPEC §64): reading a page from the
/// cache and looking at its cells copies nothing, and the first change
/// copies the page (`bytes_mut`), so the cache's copy stays as it was.
#[derive(Clone)]
pub struct SlottedPage {
    buf: Page,
    /// Whether the bytes pass `check_layout` (SPEC §152): so from
    /// `from_bytes`, which checked them, and from `new` with a slotted
    /// type. Every change here keeps it, placing cells only where they
    /// fit, but for a cell too short for an index entry. `into_page`
    /// marks the page by it, so a page changed here isn't checked again.
    valid: bool,
}

impl SlottedPage {
    /// A fresh, empty page of the given type.
    pub fn new(page_type: PageType) -> Self {
        let mut buf = vec![0u8; USABLE_PAGE_SIZE];
        buf[0] = page_type as u8;
        buf[11..13].copy_from_slice(&(USABLE_PAGE_SIZE as u16).to_le_bytes());
        Self {
            buf: buf.into(),
            valid: page_type.is_slotted(),
        }
    }

    /// Interprets an existing page buffer (as read from a `PageStore`) as a
    /// slotted page. Fails if the type byte is unknown or not a slotted page
    /// type, if the slot directory runs into the cells, or if a cell lies
    /// outside the page. Overlapping cells are not detected here (SPEC §54.2);
    /// `Database::check` finds them (§141).
    ///
    /// A page the cache marked as checked isn't checked again (SPEC §66).
    pub fn from_bytes(buf: impl Into<Page>) -> std::io::Result<Self> {
        let buf = buf.into();
        if !buf.layout_checked() {
            Self::check_layout(&buf)?;
        }
        Ok(Self { buf, valid: true })
    }

    /// `from_bytes`'s check, on its own: for the store, which marks a
    /// page that passes as it goes into the cache (SPEC §66).
    pub(crate) fn check_layout(buf: &[u8]) -> std::io::Result<()> {
        if buf.len() != USABLE_PAGE_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "page buffer is {} bytes, expected {USABLE_PAGE_SIZE}",
                    buf.len()
                ),
            ));
        }

        let page_type = PageType::from_u8(buf[0])?;
        if !page_type.is_slotted() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("page buffer contains invalid file type {page_type:?}"),
            ));
        }

        let field = |at: usize| u16::from_le_bytes([buf[at], buf[at + 1]]);
        let slot_count = field(9);
        let data_start = field(11) as usize;

        // The cells start inside the page. Found by fuzzing (SPEC §55):
        // with no live slot, the loops below have no cell to catch a
        // data_start past the end, and the next insert wrote there. And
        // before them (SPEC §129): with the directory below, it bounds
        // every slot they read, which a directory past the page's end
        // would otherwise take them to.
        if data_start > USABLE_PAGE_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("cells start at {data_start}, past the page's end at {USABLE_PAGE_SIZE}"),
            ));
        }

        // The directory: slot_count and data_start are u16, so convert to usize.
        let directory_end = HEADER_LEN + slot_count as usize * SLOT_LEN;
        if data_start < directory_end {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("slot directory ends at {directory_end}, past the cells at {data_start}"),
            ));
        }

        // Every slot:
        for slot in 0..slot_count {
            let at = Self::slot_offset(slot);
            let (offset, length) = (field(at) as usize, field(at + 2) as usize); // usize, no overflow
            if length == 0 {
                continue; // a tombstone has no cell to check
            }
            if offset < data_start || offset + length > USABLE_PAGE_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "slot {slot} points outside the page: offset {offset}, length {length}"
                    ),
                ));
            }
        }

        // An index page's cells are each long enough for an entry, which
        // the entry decoders trust (SPEC §55). Checked here, with the
        // rest of the layout, so a page the cache marked isn't checked
        // again on every step of a walk (SPEC §101).
        let min_cell = min_cell(buf[0]);
        for slot in 0..slot_count {
            let length = field(Self::slot_offset(slot) + 2) as usize;
            if length != 0 && length < min_cell {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("slot {slot} is {length} bytes, shorter than any entry"),
                ));
            }
        }
        // A packed leaf's entries, against its anchor. The anchor is read
        // from slot 0 (SPEC §171.4), so slot 0 must hold it: a tombstone
        // there would let the next live entry pass as the anchor.
        if buf[0] == PageType::IndexLeafPacked as u8 {
            if slot_count > 0 && field(Self::slot_offset(0) + 2) == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "a packed index leaf's slot 0, its anchor, is empty",
                ));
            }
            let cells = (0..slot_count).filter_map(|slot| {
                let at = Self::slot_offset(slot);
                let (offset, length) = (field(at) as usize, field(at + 2) as usize);
                (length != 0).then(|| &buf[offset..offset + length])
            });
            crate::index::check_packed(cells)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        }
        Ok(())
    }

    /// The page, to write back: `store.write_page(id, &page.into_page())`.
    /// Marked as checked if its layout is valid, as every change here
    /// keeps it (SPEC §152): a debug build checks that it is.
    pub fn into_page(self) -> Page {
        debug_assert!(
            !self.valid || Self::check_layout(&self.buf).is_ok(),
            "a change to a slotted page broke its layout"
        );
        match self.valid {
            true => self.buf.known_checked(),
            false => self.buf,
        }
    }

    /// The bytes, to change: copied first if they're shared, as a page
    /// just read from the cache is. The page then counts as unchecked
    /// (SPEC §66) until `into_page` marks it again (§152).
    fn bytes_mut(&mut self) -> &mut [u8] {
        self.buf.make_mut()
    }

    pub fn page_type(&self) -> PageType {
        PageType::from_u8(self.buf[0]).expect("validated in new()/from_bytes()")
    }

    pub fn next_page(&self) -> PageId {
        PageId::from_le_bytes(self.buf[1..9].try_into().unwrap())
    }

    pub fn set_next_page(&mut self, id: PageId) {
        self.bytes_mut()[1..9].copy_from_slice(&id.to_le_bytes());
    }

    pub fn slot_count(&self) -> u16 {
        u16::from_le_bytes(self.buf[9..11].try_into().unwrap())
    }

    fn set_slot_count(&mut self, n: u16) {
        self.bytes_mut()[9..11].copy_from_slice(&n.to_le_bytes());
    }

    fn data_start(&self) -> u16 {
        u16::from_le_bytes(self.buf[11..13].try_into().unwrap())
    }

    fn set_data_start(&mut self, v: u16) {
        self.bytes_mut()[11..13].copy_from_slice(&v.to_le_bytes());
    }

    fn slot_offset(index: u16) -> usize {
        HEADER_LEN + index as usize * SLOT_LEN
    }

    fn read_slot(&self, index: u16) -> (u16, u16) {
        let at = Self::slot_offset(index);
        let offset = u16::from_le_bytes(self.buf[at..at + 2].try_into().unwrap());
        let length = u16::from_le_bytes(self.buf[at + 2..at + 4].try_into().unwrap());
        (offset, length)
    }

    /// Every cell's slot is written here, so here a cell too short for
    /// an index entry makes the layout one `check_layout` refuses.
    fn write_slot(&mut self, index: u16, offset: u16, length: u16) {
        if length != 0 && (length as usize) < min_cell(self.buf[0]) {
            self.valid = false;
        }
        let at = Self::slot_offset(index);
        self.bytes_mut()[at..at + 2].copy_from_slice(&offset.to_le_bytes());
        self.bytes_mut()[at + 2..at + 4].copy_from_slice(&length.to_le_bytes());
    }

    /// Bytes available for one more slot + its cell, right now.
    pub fn free_space(&self) -> usize {
        let slots_end = HEADER_LEN + self.slot_count() as usize * SLOT_LEN;
        (self.data_start() as usize).saturating_sub(slots_end)
    }

    /// The room a new cell of `data_len` bytes takes: its bytes, and its
    /// entry in the slot directory.
    pub fn space_needed(data_len: usize) -> usize {
        SLOT_LEN + data_len
    }

    /// Whether a cell of `data_len` bytes would fit right now — accounts
    /// for the slot-directory entry it also needs, not just the cell
    /// payload, so this always agrees with what `insert_cell` will
    /// actually accept.
    pub fn has_room_for(&self, data_len: usize) -> bool {
        self.free_space() >= SLOT_LEN + data_len
    }

    /// Appends a new cell, returning its slot index — or `None` if it
    /// doesn't fit (the caller allocates a new page and, for chained page
    /// kinds, links it via `set_next_page`).
    pub fn insert_cell(&mut self, data: &[u8]) -> Option<u16> {
        if self.free_space() < SLOT_LEN + data.len() {
            return None;
        }
        let slot = self.slot_count();
        self.set_slot_count(slot + 1);
        self.place_cell(slot, data);
        Some(slot)
    }

    /// Whether `insert_cell_at` takes a cell of `data_len` bytes, after
    /// compacting the page if it has to.
    pub fn fits(&self, data_len: usize) -> bool {
        self.has_room_for(data_len) || self.reclaimable_space() >= SLOT_LEN + data_len
    }

    /// Inserts a cell as slot `slot`, moving the slots from there on up
    /// by one — for pages whose slot order is their key order, B-tree
    /// pages, so an insert touches one cell instead of rebuilding the
    /// page (SPEC §52). Compacts first if the bytes are there but
    /// scattered; `false`, leaving the page untouched, if they aren't.
    pub fn insert_cell_at(&mut self, slot: u16, data: &[u8]) -> bool {
        assert!(
            slot <= self.slot_count(),
            "insert at slot {slot} past the end"
        );
        if !self.fits(data.len()) {
            return false;
        }
        if !self.has_room_for(data.len()) {
            self.compact_keeping(self.slot_count());
        }
        let count = self.slot_count();
        let from = Self::slot_offset(slot);
        self.bytes_mut()
            .copy_within(from..Self::slot_offset(count), from + SLOT_LEN);
        self.set_slot_count(count + 1);
        self.place_cell(slot, data);
        true
    }

    /// Takes slot `slot` out, moving the slots after it down by one, and
    /// zeroes its bytes so a removed key doesn't linger on disk; the gap
    /// is reclaimed by the next compaction.
    pub fn remove_cell_at(&mut self, slot: u16) {
        let count = self.slot_count();
        assert!(slot < count, "remove of slot {slot} past the end");
        let (offset, length) = self.read_slot(slot);
        self.bytes_mut()[offset as usize..(offset + length) as usize].fill(0);
        let from = Self::slot_offset(slot + 1);
        self.bytes_mut()
            .copy_within(from..Self::slot_offset(count), from - SLOT_LEN);
        self.bytes_mut()[Self::slot_offset(count - 1)..Self::slot_offset(count)].fill(0);
        self.set_slot_count(count - 1);
    }

    /// Tombstones a slot. Does not reclaim its space by itself — plain
    /// `insert_cell` may still reject a cell that would fit after
    /// `compact` (which `insert_cell_reusing_slot`/`update_cell` call
    /// when they need to).
    pub fn delete_cell(&mut self, slot: u16) {
        self.write_slot(slot, 0, 0);
    }

    /// Whether `insert_cell_reusing_slot` takes a cell of `data_len`
    /// bytes, after compacting the page if it has to.
    pub fn fits_reusing_slot(&self, data_len: usize) -> bool {
        match (0..self.slot_count()).any(|s| self.read_slot(s).1 == 0) {
            true => self.reclaimable_space() >= data_len,
            false => self.fits(data_len),
        }
    }

    /// Like `insert_cell`, but reuses a tombstoned slot if there is one
    /// (so it needs no new directory entry), and compacts the page first
    /// if its free bytes are there but fragmented. Only for page kinds
    /// whose cells are addressed by slot and never read in slot order —
    /// `Data` pages; a B-tree page's slot order *is* its key order.
    pub fn insert_cell_reusing_slot(&mut self, data: &[u8]) -> Option<u16> {
        let Some(slot) = (0..self.slot_count()).find(|&s| self.read_slot(s).1 == 0) else {
            if !self.has_room_for(data.len()) && self.reclaimable_space() >= SLOT_LEN + data.len() {
                self.compact();
            }
            return self.insert_cell(data);
        };
        if self.reclaimable_space() < data.len() {
            return None;
        }
        if self.free_space() < data.len() {
            self.compact_keeping(slot + 1);
        }
        self.place_cell(slot, data);
        Some(slot)
    }

    /// Replaces a live cell's bytes, keeping its slot — in place if the
    /// new bytes are no longer than the old ones; if they are longer, in
    /// place still where the page has the bytes they grew by free, the
    /// cells below moved down to make room (SPEC §153); otherwise by
    /// compacting the page around them. Returns `false`, leaving the
    /// page untouched, if they don't fit even then (the caller moves the
    /// cell elsewhere).
    pub fn update_cell(&mut self, slot: u16, data: &[u8]) -> bool {
        let (offset, length) = self.read_slot(slot);
        assert!(
            slot < self.slot_count() && length != 0,
            "update_cell on a missing or tombstoned slot {slot}"
        );
        if data.len() <= length as usize {
            let offset = offset as usize;
            self.bytes_mut()[offset..offset + data.len()].copy_from_slice(data);
            self.write_slot(slot, offset as u16, data.len() as u16);
            return true;
        }
        let grown = data.len() - length as usize;
        if self.free_space() >= grown {
            self.grow_cell(slot, offset, data, grown);
            return true;
        }
        if self.reclaimable_space() + (length as usize) < data.len() {
            return false;
        }
        self.delete_cell(slot);
        if self.free_space() < data.len() {
            self.compact_keeping(slot + 1);
        }
        self.place_cell(slot, data);
        true
    }

    /// `update_cell` of a cell grown by `grown` bytes, which the free
    /// space has: the bytes from the cells' start to the cell's own move
    /// down by that much, the slots of the cells there with them, and
    /// the cell takes their place and its own (SPEC §153). Moves half the
    /// page's cells on average, where a compaction moves them all.
    fn grow_cell(&mut self, slot: u16, offset: u16, data: &[u8], grown: usize) {
        let (start, offset) = (self.data_start() as usize, offset as usize);
        self.bytes_mut().copy_within(start..offset, start - grown);
        for other in 0..self.slot_count() {
            let (at, length) = self.read_slot(other);
            if length != 0 && (start..offset).contains(&(at as usize)) {
                self.write_slot(other, at - grown as u16, length);
            }
        }
        let to = offset - grown;
        self.bytes_mut()[to..to + data.len()].copy_from_slice(data);
        self.set_data_start((start - grown) as u16);
        self.write_slot(slot, to as u16, data.len() as u16);
    }

    /// Writes `data` just below the packed cells and points `slot` (an
    /// existing directory entry) at it. Caller guarantees `free_space()`
    /// covers it.
    fn place_cell(&mut self, slot: u16, data: &[u8]) {
        let new_data_start = self.data_start() as usize - data.len();
        self.bytes_mut()[new_data_start..new_data_start + data.len()].copy_from_slice(data);
        self.set_data_start(new_data_start as u16);
        self.write_slot(slot, new_data_start as u16, data.len() as u16);
    }

    /// Bytes that would be free after `compact` — `free_space` plus every
    /// dead byte left behind by deleted or shrunk cells.
    pub fn reclaimable_space(&self) -> usize {
        let live: usize = self.iter_cells().map(|(_slot, cell)| cell.len()).sum();
        USABLE_PAGE_SIZE - HEADER_LEN - self.slot_count() as usize * SLOT_LEN - live
    }

    /// Repacks the live cells against the end of the page, turning every
    /// dead byte into free space. Slot indices don't change — that's the
    /// point of addressing cells by slot (see the type's doc comment) —
    /// except that tombstoned slots at the *end* of the directory are
    /// dropped: nothing references a tombstone.
    pub fn compact(&mut self) {
        self.compact_keeping(0);
    }

    /// `compact`, but never trims the directory below `min_slots` — for a
    /// caller about to refill a tombstoned slot, which may be trailing.
    ///
    /// In place (SPEC §153): the cells, highest first, each moved up
    /// against the one above, so none is written over before it moves.
    /// Cells that share bytes, which only a damaged page has (§54.2),
    /// are copied out first instead, each whole, as they always were.
    fn compact_keeping(&mut self, min_slots: u16) {
        let mut live: Vec<(u16, u16, u16)> = (0..self.slot_count())
            .map(|slot| (self.read_slot(slot), slot))
            .filter(|&((_, length), _)| length != 0)
            .map(|((offset, length), slot)| (offset, length, slot))
            .collect();
        let slot_count = live.iter().map(|&(_, _, slot)| slot + 1).max().unwrap_or(0);
        let slot_count = slot_count.max(min_slots);
        live.sort_unstable_by_key(|&(offset, _, _)| std::cmp::Reverse(offset));
        let apart = live
            .windows(2)
            .all(|pair| pair[1].0 as usize + pair[1].1 as usize <= pair[0].0 as usize);
        self.set_slot_count(slot_count);
        if apart {
            let mut end = USABLE_PAGE_SIZE;
            for &(offset, length, slot) in &live {
                let (offset, length) = (offset as usize, length as usize);
                let to = end - length;
                self.bytes_mut().copy_within(offset..offset + length, to);
                self.write_slot(slot, to as u16, length as u16);
                end = to;
            }
            self.set_data_start(end as u16);
        } else {
            let cells: Vec<(u16, Vec<u8>)> = live
                .iter()
                .map(|&(_, _, slot)| (slot, self.get_cell(slot).expect("live").to_vec()))
                .collect();
            self.set_data_start(USABLE_PAGE_SIZE as u16);
            for (slot, cell) in &cells {
                self.place_cell(*slot, cell);
            }
        }
        // Zero the gap, so bytes of deleted documents don't linger on disk.
        let gap = HEADER_LEN + slot_count as usize * SLOT_LEN..self.data_start() as usize;
        self.bytes_mut()[gap].fill(0);
    }

    /// Whether every slot is tombstoned (or there are none).
    pub fn is_empty(&self) -> bool {
        self.iter_cells().next().is_none()
    }

    /// How many slots hold a cell, tombstones left out: read from the
    /// slot directory alone (SPEC §101.3).
    pub fn live_cells(&self) -> usize {
        let directory = &self.buf[HEADER_LEN..Self::slot_offset(self.slot_count())];
        let (slots, _) = directory.as_chunks::<SLOT_LEN>();
        slots.iter().filter(|slot| slot[2..4] != [0, 0]).count()
    }

    /// How many of the slots in `slots` are live, by the directory alone.
    pub fn live_in(&self, slots: std::ops::Range<u16>) -> usize {
        let end = slots.end.min(self.slot_count());
        let start = slots.start.min(end);
        let directory = &self.buf[Self::slot_offset(start)..Self::slot_offset(end)];
        let (slots, _) = directory.as_chunks::<SLOT_LEN>();
        slots.iter().filter(|slot| slot[2..4] != [0, 0]).count()
    }

    pub fn get_cell(&self, slot: u16) -> Option<&[u8]> {
        if slot >= self.slot_count() {
            return None;
        }
        let (offset, length) = self.read_slot(slot);
        if length == 0 {
            return None; // tombstoned
        }
        Some(&self.buf[offset as usize..offset as usize + length as usize])
    }

    pub fn iter_cells(&self) -> impl Iterator<Item = (u16, &[u8])> {
        (0..self.slot_count()).filter_map(move |slot| self.get_cell(slot).map(|c| (slot, c)))
    }

    /// Two live slots whose cells share bytes, if any: one document's or
    /// entry's bytes read as part of another's (SPEC §54.2, §141). The
    /// cells are sorted by offset and each compared with the one before:
    /// until two overlap, that one ends furthest. An allocation and
    /// O(n log n), for `check` alone, never on a read.
    pub(crate) fn overlapping_cells(&self) -> Option<(u16, u16)> {
        let mut cells: Vec<(u16, u16, u16)> = (0..self.slot_count())
            .map(|slot| {
                let (offset, length) = self.read_slot(slot);
                (offset, length, slot)
            })
            .filter(|&(_, length, _)| length != 0)
            .collect();
        cells.sort_unstable();
        cells.windows(2).find_map(|pair| {
            let [(offset, length, slot), (next, _, other)] = *pair else {
                unreachable!("windows of two")
            };
            (offset as usize + length as usize > next as usize)
                .then(|| (slot.min(other), slot.max(other)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cells grown in place and pages compacted in place (SPEC §153)
    /// against a model, every slot's bytes: 300 pages of random inserts,
    /// updates that grow, shrink or keep a cell, deletes and compactions,
    /// each page filled to the brim. After every step the cells are the
    /// model's, the layout checks, no two cells share a byte, the bytes
    /// between the directory and the cells are zero after a compaction,
    /// and an update refused is one the page has no room for.
    #[test]
    fn cells_grown_and_compacted_in_place_keep_every_cell() {
        let mut seed = 11u64;
        let mut next = |below: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % below
        };
        for round in 0..300 {
            let mut page = SlottedPage::new(PageType::Data);
            let mut model: Vec<Option<Vec<u8>>> = Vec::new();
            for step in 0..400 {
                let fill = (round * 400 + step) as u8;
                let live: Vec<u16> = page.iter_cells().map(|(slot, _)| slot).collect();
                match next(10) {
                    0..=3 => {
                        let cell = vec![fill; 1 + next(300)];
                        if let Some(slot) = page.insert_cell_reusing_slot(&cell) {
                            if model.len() <= slot as usize {
                                model.resize(slot as usize + 1, None);
                            }
                            model[slot as usize] = Some(cell);
                        }
                    }
                    4..=7 if !live.is_empty() => {
                        let slot = live[next(live.len())];
                        let old = model[slot as usize].as_ref().unwrap().len();
                        let len = match next(3) {
                            0 => 1 + next(old),
                            1 => old + 1 + next(16),
                            _ => old + 1 + next(400),
                        };
                        let cell = vec![fill; len];
                        let room = page.reclaimable_space() + old;
                        let updated = page.update_cell(slot, &cell);
                        assert_eq!(updated, len <= room, "{round} {step}");
                        if updated {
                            model[slot as usize] = Some(cell);
                        }
                    }
                    8 if !live.is_empty() => {
                        let slot = live[next(live.len())];
                        page.delete_cell(slot);
                        model[slot as usize] = None;
                    }
                    _ => {
                        page.compact();
                        model.truncate(page.slot_count() as usize);
                        let directory = HEADER_LEN + page.slot_count() as usize * SLOT_LEN;
                        let gap = &page.buf[directory..page.data_start() as usize];
                        assert!(gap.iter().all(|&b| b == 0), "{round} {step}");
                    }
                }
                for (slot, cell) in model.iter().enumerate() {
                    assert_eq!(
                        page.get_cell(slot as u16),
                        cell.as_deref(),
                        "{round} {step}"
                    );
                }
                assert!(SlottedPage::check_layout(&page.buf).is_ok());
                assert_eq!(page.overlapping_cells(), None, "{round} {step}");
            }
        }
    }

    /// A damaged page whose cells share bytes (SPEC §54.2) compacts as it
    /// always did (§153): every cell copied out whole, then packed, so
    /// each keeps the bytes it was read with.
    #[test]
    fn a_page_whose_cells_overlap_compacts_each_cell_whole() {
        // A cell starting inside the one below it; and one stretched
        // over the one above it, with a third below: moved in place, it
        // would be written over the third before the third moves.
        type Damage = fn(&mut SlottedPage, [u16; 3]);
        let starts_inside: Damage = |page, [a, b, _]| {
            let (offset, _) = page.read_slot(b);
            page.write_slot(a, offset + 10, 40);
        };
        let stretched: Damage = |page, [_, b, _]| {
            let (offset, _) = page.read_slot(b);
            page.write_slot(b, offset, 80);
        };
        for damage in [starts_inside, stretched] {
            let mut page = SlottedPage::new(PageType::Data);
            let slots = [1, 2, 3].map(|fill| page.insert_cell(&[fill; 40]).unwrap());
            damage(&mut page, slots);
            let before = slots.map(|slot| page.get_cell(slot).unwrap().to_vec());
            assert!(page.overlapping_cells().is_some());
            page.compact();
            for (slot, cell) in slots.iter().zip(&before) {
                assert_eq!(page.get_cell(*slot).unwrap(), cell);
            }
            assert_eq!(page.overlapping_cells(), None);
        }
    }

    /// `fits` and `fits_reusing_slot` say beforehand what the inserts
    /// will do (SPEC §151): on pages filled to every level, with cells
    /// removed, tombstoned, or both, for every cell length up to a
    /// quarter page.
    #[test]
    fn fits_says_what_the_insert_will_do() {
        let mut seed = 7u64;
        let mut next = |below: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % below
        };
        for round in 0..200 {
            let mut page = SlottedPage::new(PageType::Data);
            let cells = next(80);
            for _ in 0..cells {
                if page.insert_cell(&vec![1; 1 + next(200)]).is_none() {
                    break;
                }
            }
            for slot in 0..page.slot_count() {
                if next(3) == 0 && round % 2 == 1 {
                    page.delete_cell(slot);
                }
            }
            if round % 4 >= 2 {
                for _ in 0..next(4) {
                    let live: Vec<u16> = page.iter_cells().map(|(slot, _)| slot).collect();
                    if !live.is_empty() {
                        page.remove_cell_at(live[next(live.len())]);
                    }
                }
            }
            // Every length up to a quarter page, and those around the
            // page's free and reclaimable bytes, where the answer turns.
            let edges = [page.free_space(), page.reclaimable_space()]
                .map(|edge| edge.saturating_sub(SLOT_LEN + 2)..edge + 2);
            let lens = (0..=USABLE_PAGE_SIZE / 4).chain(edges.into_iter().flatten());
            for len in lens {
                let cell = vec![2; len];
                let mut at = page.clone();
                let slot = next(page.slot_count() as usize + 1) as u16;
                assert_eq!(
                    page.fits(len),
                    at.insert_cell_at(slot, &cell),
                    "{round} {len}"
                );
                let mut reusing = page.clone();
                assert_eq!(
                    page.fits_reusing_slot(len),
                    reusing.insert_cell_reusing_slot(&cell).is_some(),
                    "{round} {len}"
                );
            }
        }
    }

    /// SPEC §66: a page marked checked isn't checked again, and a change
    /// to its bytes takes the mark away; a change through `SlottedPage`
    /// keeps it (§152). Marked bytes that don't hold a valid page show
    /// the check is skipped: only `Page::checked` and `into_page` mark a
    /// page, and only a valid one, so they can't happen outside a test.
    #[test]
    fn a_checked_page_is_not_checked_again_until_it_changes() {
        let mut page = SlottedPage::new(PageType::Data);
        page.insert_cell(b"cell").unwrap();
        let valid = page.into_page();
        assert!(valid.layout_checked(), "built here");
        let mut read = SlottedPage::from_bytes(valid.clone()).unwrap();
        read.insert_cell(b"more").unwrap();
        read.delete_cell(0);
        read.compact();
        let changed = read.into_page();
        assert!(changed.layout_checked(), "changed here");
        assert!(valid.layout_checked(), "the original kept its mark");
        let mut raw = changed.clone();
        raw.make_mut()[0] = PageType::Data as u8;
        assert!(!raw.layout_checked(), "its bytes changed");

        // A cell too short for an index entry: a layout the check
        // refuses, so not marked. One just long enough is marked.
        for (page_type, min) in [
            (PageType::IndexLeaf, crate::index::MIN_INDEX_ENTRY_LEN),
            (PageType::IndexBranch, crate::index::MIN_BRANCH_ENTRY_LEN),
        ] {
            let mut index = SlottedPage::new(page_type);
            index.insert_cell(&vec![1; min]).unwrap();
            index.delete_cell(0);
            assert!(index.clone().into_page().layout_checked(), "{page_type:?}");
            index.insert_cell(&vec![1; min - 1]).unwrap();
            let short = index.into_page();
            assert!(!short.layout_checked(), "{page_type:?}");
            assert!(SlottedPage::from_bytes(short).is_err());
        }
        assert!(
            !SlottedPage::new(PageType::Overflow)
                .into_page()
                .layout_checked(),
            "not a slotted type"
        );

        let mut damaged = vec![0u8; USABLE_PAGE_SIZE];
        damaged[0] = PageType::Data as u8;
        damaged[9..11].copy_from_slice(&5000u16.to_le_bytes()); // slots past the cells
        let damaged = Page::from(damaged);
        assert!(!damaged.clone().checked().layout_checked());
        assert!(SlottedPage::from_bytes(damaged.clone()).is_err());
        let marked = damaged.marked_unchecked();
        assert!(SlottedPage::from_bytes(marked).is_ok(), "not checked again");

        // Cells starting at the page's end: an empty page. One byte past
        // it: damage (SPEC §55).
        let mut empty = SlottedPage::new(PageType::Data).into_page().to_vec();
        assert!(SlottedPage::check_layout(&empty).is_ok());
        empty[11..13].copy_from_slice(&(USABLE_PAGE_SIZE as u16 + 1).to_le_bytes());
        assert!(SlottedPage::check_layout(&empty).is_err());

        let overflow = Page::from(vec![PageType::Overflow as u8; USABLE_PAGE_SIZE]);
        assert!(!overflow.checked().layout_checked(), "not a slotted page");
    }

    /// A slot directory that runs past the page's end, with cells said
    /// to start further on still, is refused before a slot is read: it
    /// panicked, reading past the buffer (SPEC §129). The most slots
    /// that fit, all empty, before cells at the page's end, still pass.
    #[test]
    fn a_directory_past_the_page_is_refused_before_it_is_read() {
        let page = |slot_count: u16, data_start: u16| {
            let mut buf = vec![0u8; USABLE_PAGE_SIZE];
            buf[0] = PageType::Data as u8;
            buf[9..11].copy_from_slice(&slot_count.to_le_bytes());
            buf[11..13].copy_from_slice(&data_start.to_le_bytes());
            buf
        };
        let most = ((USABLE_PAGE_SIZE - HEADER_LEN) / SLOT_LEN) as u16;
        let end = USABLE_PAGE_SIZE as u16;
        assert!(SlottedPage::check_layout(&page(most, end)).is_ok());
        for (slot_count, data_start) in [
            (most + 1, end),     // the directory one slot past the cells
            (most + 1, end + 1), // and both past the page's end
            (most + 1, end + 4), // the last slot's length past the end
            (2048, 9000),        // the robustness review's page
            (u16::MAX, u16::MAX),
        ] {
            let err = SlottedPage::check_layout(&page(slot_count, data_start)).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
        }
    }

    /// A page's bytes are shared until it changes (SPEC §64): changing a
    /// page made from another's bytes leaves those as they were.
    #[test]
    fn a_change_leaves_the_bytes_it_started_from_alone() {
        let mut first = SlottedPage::new(PageType::Data);
        first.insert_cell(b"kept").unwrap();
        let bytes = first.into_page();
        let mut second = SlottedPage::from_bytes(bytes.clone()).unwrap();
        second.insert_cell(b"added").unwrap();
        second.set_next_page(7);
        second.delete_cell(0);

        let first = SlottedPage::from_bytes(bytes).unwrap();
        assert_eq!((first.slot_count(), first.next_page()), (1, 0));
        assert_eq!(first.get_cell(0), Some(&b"kept"[..]));
        assert_eq!((second.slot_count(), second.get_cell(0)), (2, None));
    }

    #[test]
    fn insert_get_roundtrip() {
        let mut page = SlottedPage::new(PageType::Data);
        let a = page.insert_cell(b"hello").unwrap();
        let b = page.insert_cell(b"world!").unwrap();

        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(page.get_cell(a), Some(&b"hello"[..]));
        assert_eq!(page.get_cell(b), Some(&b"world!"[..]));
        assert_eq!(page.slot_count(), 2);
    }

    #[test]
    fn delete_tombstones_without_reclaiming() {
        let mut page = SlottedPage::new(PageType::Data);
        let slot = page.insert_cell(b"gone soon").unwrap();
        let before = page.free_space();

        page.delete_cell(slot);

        assert_eq!(page.get_cell(slot), None);
        assert_eq!(
            page.free_space(),
            before,
            "delete alone must not reclaim space"
        );
    }

    #[test]
    fn insert_fails_once_full() {
        let mut page = SlottedPage::new(PageType::IndexLeaf);
        let big = vec![0u8; 1000];
        let mut inserted = 0;
        while page.insert_cell(&big).is_some() {
            inserted += 1;
        }
        assert!(inserted > 0, "should fit at least one large cell");
        assert!(page.free_space() < SLOT_LEN + big.len());
    }

    #[test]
    fn next_page_roundtrip() {
        let mut page = SlottedPage::new(PageType::IndexLeaf);
        assert_eq!(page.next_page(), 0);
        page.set_next_page(42);
        assert_eq!(page.next_page(), 42);
    }

    /// SPEC §141: cells that share a byte are found, whichever slots
    /// they are in; cells that only touch, and tombstones, are not.
    #[test]
    fn overlapping_cells_are_found_and_touching_ones_are_not() {
        let mut page = SlottedPage::new(PageType::Data);
        for cell in [&b"aaaa"[..], b"bbbb", b"cccc", b"dddd"] {
            page.insert_cell(cell).unwrap();
        }
        page.delete_cell(1);
        assert_eq!(page.overlapping_cells(), None, "packed cells touch");

        // Cells are placed from the page's end down: `d` lowest, then
        // `c`, the bytes slot 1 held, and `a`.
        let (c, d) = (page.read_slot(2), page.read_slot(3));
        let with = |slot: u16, (offset, length): (u16, u16)| {
            let mut page = page.clone();
            page.write_slot(slot, offset, length);
            page.overlapping_cells()
        };
        assert_eq!(with(3, c), Some((2, 3)), "the same bytes");
        assert_eq!(with(3, (d.0, 5)), Some((2, 3)), "one byte into the next");
        assert_eq!(with(0, (c.0, 12)), Some((0, 2)), "one cell over two");
        assert_eq!(with(1, (d.0 + 1, 2)), Some((1, 3)), "inside another cell");
        assert_eq!(with(3, (c.0 + 5, 2)), None, "where a deleted cell was");
        assert_eq!(with(1, (c.0 + 1, 0)), None, "a tombstone inside a cell");
    }

    #[test]
    fn survives_bytes_roundtrip() {
        let mut page = SlottedPage::new(PageType::Catalog);
        page.insert_cell(b"trunk-collection").unwrap();
        page.set_next_page(7);

        let bytes = page.into_page();
        let restored = SlottedPage::from_bytes(bytes).unwrap();

        assert_eq!(restored.page_type(), PageType::Catalog);
        assert_eq!(restored.next_page(), 7);
        assert_eq!(restored.get_cell(0), Some(&b"trunk-collection"[..]));
    }

    /// `space_needed` is what `has_room_for` checks: a cell fits exactly
    /// when that much is free (SPEC §75).
    #[test]
    fn space_needed_counts_the_slot_too() {
        let page = SlottedPage::new(PageType::Data);
        let largest = page.free_space() - SlottedPage::space_needed(0);
        assert!(page.has_room_for(largest));
        assert!(!page.has_room_for(largest + 1));
        assert_eq!(SlottedPage::space_needed(largest), page.free_space());
    }

    #[test]
    fn compact_reclaims_dead_bytes_and_keeps_slots() {
        let mut page = SlottedPage::new(PageType::Data);
        let a = page.insert_cell(&[1u8; 100]).unwrap();
        let b = page.insert_cell(&[2u8; 100]).unwrap();
        let c = page.insert_cell(&[3u8; 100]).unwrap();
        page.delete_cell(b);
        let before = page.free_space();

        page.compact();

        assert_eq!(page.free_space(), before + 100);
        assert_eq!(page.get_cell(a), Some(&[1u8; 100][..]));
        assert_eq!(page.get_cell(b), None);
        assert_eq!(page.get_cell(c), Some(&[3u8; 100][..]));
    }

    #[test]
    fn compact_drops_trailing_tombstones() {
        let mut page = SlottedPage::new(PageType::Data);
        page.insert_cell(b"keep").unwrap();
        let gone = page.insert_cell(b"gone").unwrap();
        page.delete_cell(gone);

        page.compact();

        assert_eq!(page.slot_count(), 1);
        assert_eq!(page.get_cell(0), Some(&b"keep"[..]));
    }

    #[test]
    fn insert_reusing_slot_fills_a_tombstone() {
        let mut page = SlottedPage::new(PageType::Data);
        page.insert_cell(b"a").unwrap();
        let hole = page.insert_cell(b"b").unwrap();
        page.insert_cell(b"c").unwrap();
        page.delete_cell(hole);

        assert_eq!(page.insert_cell_reusing_slot(b"new"), Some(hole));
        assert_eq!(page.get_cell(hole), Some(&b"new"[..]));
        assert_eq!(page.slot_count(), 3);
    }

    #[test]
    fn insert_reusing_slot_compacts_a_fragmented_page() {
        let mut page = SlottedPage::new(PageType::Data);
        let big = vec![7u8; 3000];
        let a = page.insert_cell(&big).unwrap();
        page.insert_cell(&big).unwrap();
        assert!(page.insert_cell(&big).is_none(), "page should be full");

        page.delete_cell(a);
        assert!(
            !page.has_room_for(big.len()),
            "space is dead, not free, until compaction"
        );
        assert_eq!(page.insert_cell_reusing_slot(&big), Some(a));
        assert_eq!(page.get_cell(a), Some(&big[..]));
    }

    #[test]
    fn insert_reusing_slot_refills_a_trailing_tombstone_after_compacting() {
        let mut page = SlottedPage::new(PageType::Data);
        let big = vec![7u8; 4000];
        page.insert_cell(&big).unwrap();
        let last = page.insert_cell(&big).unwrap();
        page.delete_cell(last);

        assert_eq!(page.insert_cell_reusing_slot(&big), Some(last));
        assert_eq!(page.get_cell(last), Some(&big[..]));
    }

    #[test]
    fn update_cell_in_place_growing_and_too_big() {
        let mut page = SlottedPage::new(PageType::Data);
        let a = page.insert_cell(b"hello").unwrap();
        let b = page.insert_cell(b"world").unwrap();

        assert!(page.update_cell(a, b"hi"));
        assert_eq!(page.get_cell(a), Some(&b"hi"[..]));

        let grown = vec![9u8; 4000];
        assert!(page.update_cell(a, &grown));
        assert_eq!(page.get_cell(a), Some(&grown[..]));
        assert_eq!(page.get_cell(b), Some(&b"world"[..]));

        let too_big = vec![1u8; USABLE_PAGE_SIZE];
        assert!(!page.update_cell(a, &too_big));
        assert_eq!(
            page.get_cell(a),
            Some(&grown[..]),
            "a failed update changes nothing"
        );
    }

    #[test]
    fn update_cell_uses_space_only_compaction_frees() {
        let mut page = SlottedPage::new(PageType::Data);
        let a = page.insert_cell(&[1u8; 3000]).unwrap();
        let b = page.insert_cell(&[2u8; 3000]).unwrap();
        page.delete_cell(b);

        // Fits only once b's dead bytes and a's own old bytes are reclaimed.
        let grown = vec![3u8; 6000];
        assert!(page.update_cell(a, &grown));
        assert_eq!(page.get_cell(a), Some(&grown[..]));
    }

    fn cells(page: &SlottedPage) -> Vec<Vec<u8>> {
        page.iter_cells()
            .map(|(_slot, cell)| cell.to_vec())
            .collect()
    }

    /// Slot order is key order on a B-tree page: a cell goes in at the
    /// front, between two others or at the end, and the rest move up.
    #[test]
    fn insert_cell_at_keeps_slot_order() {
        let mut page = SlottedPage::new(PageType::IndexLeaf);
        assert!(page.insert_cell_at(0, b"c"));
        assert!(page.insert_cell_at(0, b"a"));
        assert!(page.insert_cell_at(2, b"d"));
        assert!(page.insert_cell_at(1, b"bb"));
        assert_eq!(cells(&page), [&b"a"[..], b"bb", b"c", b"d"]);
        assert_eq!(page.get_cell(3), Some(&b"d"[..]));
    }

    /// Room made by removals is used once the page compacts; with no room
    /// at all, the page is left as it was.
    #[test]
    fn insert_cell_at_compacts_or_leaves_the_page_alone() {
        let mut page = SlottedPage::new(PageType::IndexLeaf);
        for fill in 1..=4u8 {
            assert!(page.insert_cell_at(fill as u16 - 1, &[fill; 2000]));
        }
        assert!(!page.has_room_for(2000));
        page.remove_cell_at(1);
        page.remove_cell_at(1);
        assert!(!page.has_room_for(2000), "dead, not free, until compaction");
        assert!(page.insert_cell_at(1, &[9; 2000]));
        assert!(page.insert_cell_at(1, &[8; 1000]));
        let before = page.clone().into_page();
        assert!(!page.insert_cell_at(0, &[7; 2000]));
        assert_eq!(page.clone().into_page(), before);
        assert_eq!(
            cells(&page),
            [vec![1; 2000], vec![8; 1000], vec![9; 2000], vec![4; 2000]]
        );
    }

    /// A trailing tombstone (left by versions before SPEC §52) keeps its
    /// slot through the compaction, so the slot the caller counted to —
    /// the end — is still the end.
    #[test]
    fn insert_cell_at_the_end_past_a_trailing_tombstone() {
        let mut page = SlottedPage::new(PageType::IndexLeaf);
        for fill in 1..=4u8 {
            assert!(page.insert_cell_at(fill as u16 - 1, &[fill; 2000]));
        }
        page.delete_cell(3);
        assert!(page.insert_cell_at(4, &[5; 2000]));
        assert_eq!(page.slot_count(), 5);
        assert_eq!(page.get_cell(3), None);
        assert_eq!(page.get_cell(4), Some(&[5; 2000][..]));
    }

    /// A removed cell's bytes are gone, not just unreachable, and the
    /// slots after it move down.
    #[test]
    fn remove_cell_at_zeroes_the_cell_and_closes_the_gap() {
        let mut page = SlottedPage::new(PageType::IndexLeaf);
        for (slot, cell) in [&b"first"[..], b"secret", b"last"].iter().enumerate() {
            assert!(page.insert_cell_at(slot as u16, cell));
        }
        page.remove_cell_at(1);
        assert_eq!(cells(&page), [&b"first"[..], b"last"]);
        assert_eq!(page.slot_count(), 2);
        let bytes = page.clone().into_page();
        assert!(!bytes.windows(6).any(|w| w == b"secret"));
        // The old last slot entry is cleared too.
        assert!(
            bytes[HEADER_LEN + 2 * SLOT_LEN..HEADER_LEN + 3 * SLOT_LEN]
                .iter()
                .all(|&b| b == 0)
        );
        page.remove_cell_at(1);
        page.remove_cell_at(0);
        assert!(page.is_empty());
        assert_eq!(page.slot_count(), 0);
    }

    #[test]
    fn from_bytes_rejects_bad_type_tag() {
        let mut buf = vec![0u8; USABLE_PAGE_SIZE];
        buf[0] = 200; // not a valid PageType
        assert!(SlottedPage::from_bytes(buf).is_err());
    }

    /// A page with one slot, `(offset, length)`, and otherwise empty.
    fn page_bytes_with_slot(offset: u16, length: u16) -> Vec<u8> {
        let mut buf = vec![0u8; USABLE_PAGE_SIZE];
        buf[0] = PageType::Data as u8;
        buf[9..11].copy_from_slice(&1u16.to_le_bytes());
        buf[11..13].copy_from_slice(&offset.to_le_bytes());
        buf[13..15].copy_from_slice(&offset.to_le_bytes());
        buf[15..17].copy_from_slice(&length.to_le_bytes());
        buf
    }

    #[test]
    fn from_bytes_rejects_offsets_past_page_end() {
        let mut buf = page_bytes_with_slot(60000, 500);
        // The cells inside the page, so the slot is what's refused: a
        // start past the page is refused before any slot (SPEC §129).
        buf[11..13].copy_from_slice(&(USABLE_PAGE_SIZE as u16).to_le_bytes());
        let error = SlottedPage::from_bytes(buf)
            .err()
            .expect("a slot past the page end must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("slot 0"), "wrong error: {error}");
    }

    #[test]
    fn from_bytes_rejects_offsets_slightly_above_end() {
        let buf = page_bytes_with_slot(8000, 200);
        let error = SlottedPage::from_bytes(buf)
            .err()
            .expect("a slot past the page end must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("slot 0"), "wrong error: {error}");
    }

    #[test]
    fn from_bytes_accepts_offsets_barely_within_bounds() {
        let buf = page_bytes_with_slot(8000, 100);
        let page = SlottedPage::from_bytes(buf).expect("a slot within bounds has to fit the page");

        let slots = page.slot_count();
        assert_eq!(slots, 1, "page should contain 1 slot but contains {slots}");
    }

    #[test]
    fn from_bytes_rejects_data_reaching_into_directory() {
        let buf = page_bytes_with_slot(12, 200);
        let error = SlottedPage::from_bytes(buf)
            .err()
            .expect("a slot reaching into the directory must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn from_bytes_rejects_wrong_page_format() {
        let mut buf = page_bytes_with_slot(12, 200);
        buf[0] = PageType::Header as u8;
        let error = SlottedPage::from_bytes(buf)
            .err()
            .expect("a page with an not slotted page type must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("Header"));
    }

    /// A page with a tombstone in it loads: the dead slot stays in the
    /// directory and is skipped, and the live one is still readable.
    #[test]
    fn from_bytes_accepts_a_page_with_a_tombstone() {
        let mut page = SlottedPage::new(PageType::Data);
        page.insert_cell(b"first").unwrap();
        page.insert_cell(b"second").unwrap();
        page.delete_cell(0);

        let page =
            SlottedPage::from_bytes(page.into_page()).expect("a page with a tombstone must load");

        assert_eq!(page.slot_count(), 2, "a delete keeps the slot");
        assert_eq!(page.get_cell(0), None);
        assert_eq!(page.get_cell(1), Some(&b"second"[..]));
    }

    /// `data_start` is legal here and the cell ends inside the page, so
    /// only the check that a cell starts at or after `data_start` can
    /// reject it.
    #[test]
    fn from_bytes_rejects_a_cell_starting_before_data_start() {
        let mut buf = page_bytes_with_slot(7000, 100); // the cell: bytes 7000..7100
        buf[11..13].copy_from_slice(&8000u16.to_le_bytes()); // but the cells "begin" at 8000

        let error = SlottedPage::from_bytes(buf)
            .err()
            .expect("a cell before data_start must be rejected");
        assert!(error.to_string().contains("slot 0"), "wrong error: {error}");
    }

    /// Found by fuzzing (SPEC §55): with no live slot, nothing else
    /// catches cells that "start" past the page's end.
    #[test]
    fn from_bytes_rejects_cells_starting_past_the_page_end() {
        let mut buf = page_bytes_with_slot(8000, 100);
        buf[9..11].copy_from_slice(&0u16.to_le_bytes()); // no slots
        buf[11..13].copy_from_slice(&16539u16.to_le_bytes());

        let error = SlottedPage::from_bytes(buf)
            .err()
            .expect("cells past the page's end must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("past the page's end"),
            "wrong error: {error}"
        );
    }
}
