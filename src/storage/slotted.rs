use super::{PageId, PageType, USABLE_PAGE_SIZE};

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

/// A page interpreted as a slot directory (growing forward from the
/// header) plus variable-length cells (packed backward from the end of the
/// page) — the standard layout PostgreSQL heap pages and SQLite B-tree
/// pages both use. A cell's identity is its slot index, not its byte
/// offset, so a cell can be relocated within the page later (compaction)
/// without invalidating anything that references it by slot.
#[derive(Clone)]
pub struct SlottedPage {
    buf: Vec<u8>,
}

impl SlottedPage {
    /// A fresh, empty page of the given type.
    pub fn new(page_type: PageType) -> Self {
        let mut buf = vec![0u8; USABLE_PAGE_SIZE];
        buf[0] = page_type as u8;
        buf[11..13].copy_from_slice(&(USABLE_PAGE_SIZE as u16).to_le_bytes());
        Self { buf }
    }

    /// Interprets an existing page buffer (as read from a `PageStore`) as a
    /// slotted page. Fails only if the type tag is unrecognized — anything
    /// else about a corrupt buffer (bad offsets, overlapping cells) is not
    /// currently detected.
    pub fn from_bytes(buf: Vec<u8>) -> std::io::Result<Self> {
        if buf.len() != USABLE_PAGE_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "page buffer is {} bytes, expected {USABLE_PAGE_SIZE}",
                    buf.len()
                ),
            ));
        }
        PageType::from_u8(buf[0])?;
        Ok(Self { buf })
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn page_type(&self) -> PageType {
        PageType::from_u8(self.buf[0]).expect("validated in new()/from_bytes()")
    }

    pub fn next_page(&self) -> PageId {
        PageId::from_le_bytes(self.buf[1..9].try_into().unwrap())
    }

    pub fn set_next_page(&mut self, id: PageId) {
        self.buf[1..9].copy_from_slice(&id.to_le_bytes());
    }

    pub fn slot_count(&self) -> u16 {
        u16::from_le_bytes(self.buf[9..11].try_into().unwrap())
    }

    fn set_slot_count(&mut self, n: u16) {
        self.buf[9..11].copy_from_slice(&n.to_le_bytes());
    }

    fn data_start(&self) -> u16 {
        u16::from_le_bytes(self.buf[11..13].try_into().unwrap())
    }

    fn set_data_start(&mut self, v: u16) {
        self.buf[11..13].copy_from_slice(&v.to_le_bytes());
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

    fn write_slot(&mut self, index: u16, offset: u16, length: u16) {
        let at = Self::slot_offset(index);
        self.buf[at..at + 2].copy_from_slice(&offset.to_le_bytes());
        self.buf[at + 2..at + 4].copy_from_slice(&length.to_le_bytes());
    }

    /// Bytes available for one more slot + its cell, right now.
    pub fn free_space(&self) -> usize {
        let slots_end = HEADER_LEN + self.slot_count() as usize * SLOT_LEN;
        (self.data_start() as usize).saturating_sub(slots_end)
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
        if !self.has_room_for(data.len()) {
            if self.reclaimable_space() < SLOT_LEN + data.len() {
                return false;
            }
            self.compact_keeping(self.slot_count());
        }
        let count = self.slot_count();
        let from = Self::slot_offset(slot);
        self.buf
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
        self.buf[offset as usize..(offset + length) as usize].fill(0);
        let from = Self::slot_offset(slot + 1);
        self.buf
            .copy_within(from..Self::slot_offset(count), from - SLOT_LEN);
        self.buf[Self::slot_offset(count - 1)..Self::slot_offset(count)].fill(0);
        self.set_slot_count(count - 1);
    }

    /// Tombstones a slot. Does not reclaim its space by itself — plain
    /// `insert_cell` may still reject a cell that would fit after
    /// `compact` (which `insert_cell_reusing_slot`/`update_cell` call
    /// when they need to).
    pub fn delete_cell(&mut self, slot: u16) {
        self.write_slot(slot, 0, 0);
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
    /// new bytes are no longer than the old ones, otherwise by compacting
    /// the page around them. Returns `false`, leaving the page untouched,
    /// if they don't fit even then (the caller moves the cell elsewhere).
    pub fn update_cell(&mut self, slot: u16, data: &[u8]) -> bool {
        let (offset, length) = self.read_slot(slot);
        assert!(
            slot < self.slot_count() && length != 0,
            "update_cell on a missing or tombstoned slot {slot}"
        );
        if data.len() <= length as usize {
            let offset = offset as usize;
            self.buf[offset..offset + data.len()].copy_from_slice(data);
            self.write_slot(slot, offset as u16, data.len() as u16);
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

    /// Writes `data` just below the packed cells and points `slot` (an
    /// existing directory entry) at it. Caller guarantees `free_space()`
    /// covers it.
    fn place_cell(&mut self, slot: u16, data: &[u8]) {
        let new_data_start = self.data_start() as usize - data.len();
        self.buf[new_data_start..new_data_start + data.len()].copy_from_slice(data);
        self.set_data_start(new_data_start as u16);
        self.write_slot(slot, new_data_start as u16, data.len() as u16);
    }

    /// Bytes that would be free after `compact` — `free_space` plus every
    /// dead byte left behind by deleted or shrunk cells.
    fn reclaimable_space(&self) -> usize {
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
    fn compact_keeping(&mut self, min_slots: u16) {
        let live: Vec<(u16, Vec<u8>)> = self
            .iter_cells()
            .map(|(slot, cell)| (slot, cell.to_vec()))
            .collect();
        let slot_count = live.last().map_or(0, |(slot, _)| slot + 1).max(min_slots);
        self.set_slot_count(slot_count);
        self.set_data_start(USABLE_PAGE_SIZE as u16);
        for (slot, cell) in &live {
            self.place_cell(*slot, cell);
        }
        // Zero the gap, so bytes of deleted documents don't linger on disk.
        let gap = HEADER_LEN + slot_count as usize * SLOT_LEN..self.data_start() as usize;
        self.buf[gap].fill(0);
    }

    /// Whether every slot is tombstoned (or there are none).
    pub fn is_empty(&self) -> bool {
        self.iter_cells().next().is_none()
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn survives_bytes_roundtrip() {
        let mut page = SlottedPage::new(PageType::Catalog);
        page.insert_cell(b"trunk-collection").unwrap();
        page.set_next_page(7);

        let bytes = page.into_bytes();
        let restored = SlottedPage::from_bytes(bytes).unwrap();

        assert_eq!(restored.page_type(), PageType::Catalog);
        assert_eq!(restored.next_page(), 7);
        assert_eq!(restored.get_cell(0), Some(&b"trunk-collection"[..]));
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
        let before = page.clone().into_bytes();
        assert!(!page.insert_cell_at(0, &[7; 2000]));
        assert_eq!(page.clone().into_bytes(), before);
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
        let bytes = page.clone().into_bytes();
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
}
