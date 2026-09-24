use crate::document::{DocId, Document, decode_document, encode_document};
use crate::storage::{PageId, PageStore, PageType, RecordLocation, SlottedPage, USABLE_PAGE_SIZE};

/// Flags byte value for a document stored entirely inside its cell.
const INLINE: u8 = 0;
/// Flags byte value for a document too large for one page (SPEC §26): the
/// cell holds only `[u32 length][u64 first Overflow page]`, the encoded
/// document lives in a chain of `Overflow` pages.
const OVERFLOW: u8 = 1;

/// `[u8 flags][16-byte DocId]`, common to both cell kinds.
const CELL_HEADER_LEN: usize = 1 + 16;
/// An overflow cell: the header plus `[u32 length][u64 first page]`.
const OVERFLOW_CELL_LEN: usize = CELL_HEADER_LEN + 4 + 8;

// Overflow page layout — deliberately not a `SlottedPage`: it holds one
// run of bytes, and a slot directory would only cost space.
//   [0]      page type tag (PageType::Overflow)
//   [1..9)   next: u64 (PageId, 0 = last page of the chain)
//   [9..)    the next OVERFLOW_CAPACITY bytes of the encoded document;
//            on the last page, only the remainder, the rest zeroed
const OVERFLOW_HEADER_LEN: usize = 9;
const OVERFLOW_CAPACITY: usize = USABLE_PAGE_SIZE - OVERFLOW_HEADER_LEN;

/// The `Data` page cell format: `[u8 flags][16-byte DocId][payload]`, the
/// payload being the encoded document (`INLINE`) or a pointer to it
/// (`OVERFLOW`). The id has to be stored here explicitly — nothing else
/// does, once a document is persisted. The index maps `id -> location`,
/// not the other way around, so given only a data page's bytes there's no
/// way to ask anything else "whose document is this" (needed for e.g. a
/// full scan, or rebuilding a corrupt index from the data pages
/// themselves). The flags byte comes first so the two kinds can be told
/// apart before anything else is decoded.
enum Cell<'a> {
    Inline(DocId, &'a [u8]),
    Overflow { id: DocId, len: u32, first: PageId },
}

impl<'a> Cell<'a> {
    fn parse(bytes: &'a [u8]) -> std::io::Result<Self> {
        let (&flags, rest) = bytes.split_first().expect("cells are never empty");
        let (id_bytes, rest) = rest.split_at(16);
        let id = DocId(id_bytes.try_into().unwrap());
        match flags {
            INLINE => Ok(Cell::Inline(id, rest)),
            OVERFLOW => Ok(Cell::Overflow {
                id,
                len: u32::from_le_bytes(rest[0..4].try_into().unwrap()),
                first: PageId::from_le_bytes(rest[4..12].try_into().unwrap()),
            }),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown record flags {other} — file may be corrupt"),
            )),
        }
    }

    /// The overflow chain this cell owns, if any — `(length, first page)`.
    fn chain(&self) -> Option<(u32, PageId)> {
        match *self {
            Cell::Inline(..) => None,
            Cell::Overflow { len, first, .. } => Some((len, first)),
        }
    }
}

/// Plain functions, not a struct — every `RecordLocation` is a
/// self-contained address. The one piece of state that does outlive a
/// call, a collection's *current data page* (where its next insert goes,
/// `0` = none yet), belongs to the catalog (`CollectionMeta`); these
/// functions take it as `current` and update it when they allocate a new
/// page, and the caller persists the change.
///
/// Documents are packed: an insert goes into the current page if it has
/// room (after compacting, if needed), and only allocates a new page —
/// which becomes current — when it doesn't. Older, partly emptied pages
/// aren't reused for inserts (no free-space map yet, see SPEC §20.4);
/// their dead bytes are reclaimed when an update there needs them, and
/// the whole page is freed once its last document is deleted.
///
/// A document too large for an empty data page goes to overflow pages
/// (`write_cell`); its cell is then small and packed like any other.
pub fn insert_record(
    store: &mut dyn PageStore,
    current: &mut PageId,
    id: DocId,
    doc: &Document,
) -> std::io::Result<RecordLocation> {
    let cell = write_cell(store, id, doc)?;
    place_cell(store, current, &cell)
}

#[cfg(test)]
thread_local! {
    /// How many documents `get_record` has read on this thread — so a
    /// test can check a query reads no more than it has to.
    pub(crate) static RECORDS_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn get_record(
    store: &dyn PageStore,
    loc: RecordLocation,
) -> std::io::Result<(DocId, Document)> {
    Records::new(store).get(loc)
}

/// Reads documents one after another, keeping the last data page it read:
/// a scan's next document is usually on the same page, and reading that
/// page again — from the file, checksum and all (SPEC §40) — is what a
/// scan spent most of its time on. Only for reads that change nothing in
/// between, which the `&dyn PageStore` it borrows makes sure of.
pub struct Records<'a> {
    store: &'a dyn PageStore,
    page: Option<(PageId, SlottedPage)>,
}

impl<'a> Records<'a> {
    pub fn new(store: &'a dyn PageStore) -> Self {
        Records { store, page: None }
    }

    pub fn get(&mut self, loc: RecordLocation) -> std::io::Result<(DocId, Document)> {
        #[cfg(test)]
        RECORDS_READ.with(|n| n.set(n.get() + 1));
        let page = match &self.page {
            Some((id, page)) if *id == loc.page => page,
            _ => {
                &self
                    .page
                    .insert((loc.page, read_data_page(self.store, loc.page)?))
                    .1
            }
        };
        match Cell::parse(live_cell(page, loc)?)? {
            Cell::Inline(id, encoded) => Ok((id, decode(encoded)?)),
            Cell::Overflow { id, len, first } => {
                let mut encoded = Vec::with_capacity(len as usize);
                walk_chain(self.store, first, len, |_page, bytes| {
                    encoded.extend_from_slice(bytes)
                })?;
                Ok((id, decode(&encoded)?))
            }
        }
    }
}

/// Replaces the document at `loc`, returning where it lives now. Usually
/// that's `loc` itself — the cell is rewritten in place, or the page is
/// compacted around it. If the grown cell doesn't fit on its page at all
/// anymore, it moves: out of `loc` (see `delete_record` for what happens
/// to the page it leaves), into wherever `insert_record` puts it. The
/// caller must then re-point the index at the returned location.
///
/// An old overflow chain is freed first and the new one written from
/// scratch, whatever the sizes — the freed pages are the first ones the
/// new chain gets back from the free list anyway.
pub fn update_record(
    store: &mut dyn PageStore,
    current: &mut PageId,
    loc: RecordLocation,
    id: DocId,
    doc: &Document,
) -> std::io::Result<RecordLocation> {
    let mut page = read_data_page(store, loc.page)?;
    if let Some((len, first)) = Cell::parse(live_cell(&page, loc)?)?.chain() {
        free_chain(store, first, len)?;
    }
    let cell = write_cell(store, id, doc)?;
    if page.update_cell(loc.slot, &cell) {
        store.write_page(loc.page, &page.into_bytes())?;
        return Ok(loc);
    }

    page.delete_cell(loc.slot);
    write_or_free(store, *current, loc.page, page)?;
    place_cell(store, current, &cell)
}

/// Tombstones the document's cell and frees its overflow chain, if any.
/// A page left with no documents is freed — back to `FileStore`'s free
/// list, for the next `allocate_page` of any page type — unless it's the
/// collection's current page, which the next insert is about to use
/// anyway.
pub fn delete_record(
    store: &mut dyn PageStore,
    current: PageId,
    loc: RecordLocation,
) -> std::io::Result<()> {
    let mut page = read_data_page(store, loc.page)?;
    if let Some((len, first)) = Cell::parse(live_cell(&page, loc)?)?.chain() {
        free_chain(store, first, len)?;
    }
    page.delete_cell(loc.slot);
    write_or_free(store, current, loc.page, page)
}

/// Frees every page a dropped collection's documents use (SPEC §37): the
/// data pages at `locs`, their documents' overflow chains, and the
/// collection's `current` data page, which may hold none of them (it
/// stays even when empty). A data page never holds two collections'
/// documents (SPEC §20), so every page here is the collection's own.
/// Everything is read before anything is freed — freeing overwrites.
pub fn free_collection_pages(
    store: &mut dyn PageStore,
    current: PageId,
    locs: impl IntoIterator<Item = RecordLocation>,
) -> std::io::Result<()> {
    let (data_pages, chain_pages) = collection_pages(store, current, locs)?;
    for page in chain_pages.into_iter().chain(data_pages) {
        store.free_page(page)?;
    }
    Ok(())
}

/// A collection's data pages — the ones holding the documents at `locs`,
/// and its `current` one — and its documents' overflow pages. What
/// `free_collection_pages` frees, and what `Database::check` expects the
/// collection to own (SPEC §39).
pub fn collection_pages(
    store: &dyn PageStore,
    current: PageId,
    locs: impl IntoIterator<Item = RecordLocation>,
) -> std::io::Result<(std::collections::BTreeSet<PageId>, Vec<PageId>)> {
    let mut data_pages = std::collections::BTreeSet::new();
    let mut chain_pages = Vec::new();
    for loc in locs {
        let page = read_data_page(store, loc.page)?;
        if let Some((len, first)) = Cell::parse(live_cell(&page, loc)?)?.chain() {
            walk_chain(store, first, len, |page, _bytes| chain_pages.push(page))?;
        }
        data_pages.insert(loc.page);
    }
    if current != 0 {
        read_data_page(store, current)?; // a data page, as the catalog says
        data_pages.insert(current);
    }
    Ok((data_pages, chain_pages))
}

/// Encodes `doc` into the cell that will represent it: inline if that
/// fits on an empty data page, otherwise an overflow cell pointing at a
/// newly written chain. The choice depends on the size alone, so a
/// document switches kinds when an update moves it across the line.
fn write_cell(store: &mut dyn PageStore, id: DocId, doc: &Document) -> std::io::Result<Vec<u8>> {
    let encoded = encode_document(doc);
    let mut cell = Vec::with_capacity(CELL_HEADER_LEN + encoded.len());
    if SlottedPage::new(PageType::Data).has_room_for(CELL_HEADER_LEN + encoded.len()) {
        cell.push(INLINE);
        cell.extend_from_slice(&id.0);
        cell.extend_from_slice(&encoded);
        return Ok(cell);
    }

    let len = u32::try_from(encoded.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "document is {} bytes encoded, more than the {} a document can have",
                encoded.len(),
                u32::MAX
            ),
        )
    })?;
    let first = write_chain(store, &encoded)?;
    cell.push(OVERFLOW);
    cell.extend_from_slice(&id.0);
    cell.extend_from_slice(&len.to_le_bytes());
    cell.extend_from_slice(&first.to_le_bytes());
    debug_assert_eq!(cell.len(), OVERFLOW_CELL_LEN);
    Ok(cell)
}

/// Puts a new cell into the current page if it fits there, otherwise into
/// a newly allocated page, which becomes current.
fn place_cell(
    store: &mut dyn PageStore,
    current: &mut PageId,
    cell: &[u8],
) -> std::io::Result<RecordLocation> {
    if *current != 0 {
        let mut page = read_data_page(store, *current)?;
        if let Some(slot) = page.insert_cell_reusing_slot(cell) {
            store.write_page(*current, &page.into_bytes())?;
            return Ok(RecordLocation {
                page: *current,
                slot,
            });
        }
    }

    let page_id = store.allocate_page()?;
    let mut page = SlottedPage::new(PageType::Data);
    let slot = page
        .insert_cell(cell)
        .expect("write_cell only makes cells that fit on an empty page");
    store.write_page(page_id, &page.into_bytes())?;
    *current = page_id;

    Ok(RecordLocation {
        page: page_id,
        slot,
    })
}

/// Writes `bytes` (never empty — an overflowing document is thousands of
/// bytes) to a chain of newly allocated `Overflow` pages, returning the
/// first one. All pages are allocated before any is written, since each
/// page stores its successor's id.
fn write_chain(store: &mut dyn PageStore, bytes: &[u8]) -> std::io::Result<PageId> {
    let chunks: Vec<&[u8]> = bytes.chunks(OVERFLOW_CAPACITY).collect();
    let pages = chunks
        .iter()
        .map(|_| store.allocate_page())
        .collect::<std::io::Result<Vec<PageId>>>()?;
    for (i, chunk) in chunks.iter().enumerate() {
        let next = pages.get(i + 1).copied().unwrap_or(0);
        let mut buf = vec![0u8; USABLE_PAGE_SIZE];
        buf[0] = PageType::Overflow as u8;
        buf[1..OVERFLOW_HEADER_LEN].copy_from_slice(&next.to_le_bytes());
        buf[OVERFLOW_HEADER_LEN..OVERFLOW_HEADER_LEN + chunk.len()].copy_from_slice(chunk);
        store.write_page(pages[i], &buf)?;
    }
    Ok(pages[0])
}

/// Calls `visit` with each page of a `len`-byte chain and that page's
/// share of the bytes, in order. The length, not the `next` pointers,
/// decides where the chain ends — so a corrupt pointer can't send this
/// around a cycle forever: it's reported, as is a chain that ends early,
/// or a page in it that isn't tagged `Overflow`.
fn walk_chain(
    store: &dyn PageStore,
    first: PageId,
    len: u32,
    mut visit: impl FnMut(PageId, &[u8]),
) -> std::io::Result<()> {
    let corrupt = |what: String| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("overflow chain starting at page {first}: {what} — file may be corrupt"),
        )
    };
    let mut page_id = first;
    let mut remaining = len as usize;
    loop {
        let buf = store.read_page(page_id)?;
        if buf[0] != PageType::Overflow as u8 {
            return Err(corrupt(format!("page {page_id} isn't an overflow page")));
        }
        let next = PageId::from_le_bytes(buf[1..OVERFLOW_HEADER_LEN].try_into().unwrap());
        let take = remaining.min(OVERFLOW_CAPACITY);
        visit(
            page_id,
            &buf[OVERFLOW_HEADER_LEN..OVERFLOW_HEADER_LEN + take],
        );
        remaining -= take;
        match (remaining, next) {
            (0, 0) => return Ok(()),
            (0, _) => return Err(corrupt(format!("page {page_id} links past its end"))),
            (_, 0) => return Err(corrupt(format!("ends {remaining} bytes early"))),
            _ => page_id = next,
        }
    }
}

/// Returns a chain's pages to the free list. Walks the whole chain first:
/// freeing a page overwrites its `next` pointer.
fn free_chain(store: &mut dyn PageStore, first: PageId, len: u32) -> std::io::Result<()> {
    let mut pages = Vec::new();
    walk_chain(store, first, len, |page, _bytes| pages.push(page))?;
    for page in pages {
        store.free_page(page)?;
    }
    Ok(())
}

fn decode(encoded: &[u8]) -> std::io::Result<Document> {
    let (doc, _rest) = decode_document(encoded)?;
    Ok(doc)
}

fn write_or_free(
    store: &mut dyn PageStore,
    current: PageId,
    page_id: PageId,
    page: SlottedPage,
) -> std::io::Result<()> {
    if page.is_empty() && page_id != current {
        store.free_page(page_id)
    } else {
        store.write_page(page_id, &page.into_bytes())
    }
}

fn read_data_page(store: &dyn PageStore, page_id: PageId) -> std::io::Result<SlottedPage> {
    let page = SlottedPage::from_bytes(store.read_page(page_id)?)?;
    if page.page_type() != PageType::Data {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("page {page_id} was expected to be a data page — file may be corrupt"),
        ));
    }
    Ok(page)
}

fn live_cell(page: &SlottedPage, loc: RecordLocation) -> std::io::Result<&[u8]> {
    page.get_cell(loc.slot).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "no live cell at slot {} on page {} — file may be corrupt",
                loc.slot, loc.page
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FileStore;

    fn store() -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::open(dir.path().join("test.trunkdb")).unwrap();
        (dir, store)
    }

    fn id(n: u8) -> DocId {
        DocId([n; 16])
    }

    fn blob(len: usize) -> Document {
        Document::Binary(vec![0xAB; len])
    }

    /// The flags byte of the cell at `loc`.
    fn flags(store: &FileStore, loc: RecordLocation) -> u8 {
        read_data_page(store, loc.page)
            .unwrap()
            .get_cell(loc.slot)
            .unwrap()[0]
    }

    /// Size of a `blob` whose inline cell exactly fills an empty data page.
    fn largest_inline_blob() -> usize {
        let empty = SlottedPage::new(PageType::Data);
        let largest_cell = (0..USABLE_PAGE_SIZE)
            .rev()
            .find(|&n| empty.has_room_for(n))
            .unwrap();
        // flags + id, then the Binary tag and its u32 length.
        largest_cell - CELL_HEADER_LEN - 1 - 4
    }

    /// A `PageStore` that counts `read_page` calls.
    struct Counting<'a>(&'a FileStore, std::cell::Cell<usize>);

    impl PageStore for Counting<'_> {
        fn allocate_page(&mut self) -> std::io::Result<PageId> {
            unreachable!()
        }
        fn read_page(&self, id: PageId) -> std::io::Result<Vec<u8>> {
            self.1.set(self.1.get() + 1);
            self.0.read_page(id)
        }
        fn try_read_page(&self, _id: PageId) -> std::io::Result<Option<Vec<u8>>> {
            unreachable!()
        }
        fn write_page(&mut self, _id: PageId, _data: &[u8]) -> std::io::Result<()> {
            unreachable!()
        }
        fn free_page(&mut self, _id: PageId) -> std::io::Result<()> {
            unreachable!()
        }
    }

    /// Documents on the same page, one after another, cost one page read;
    /// a document on another page is read from that page, and one on the
    /// first page again from the first page — the right one each time.
    #[test]
    fn records_reads_a_page_once_for_the_documents_on_it_in_a_row() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let locs: Vec<RecordLocation> = (0..30u8)
            .map(|n| insert_record(&mut store, &mut current, id(n), &blob(1000)).unwrap())
            .collect();
        let pages: Vec<PageId> = locs.iter().map(|loc| loc.page).collect();
        assert!(pages[0] == pages[1] && pages[0] != pages[29], "{pages:?}");

        let counting = Counting(&store, std::cell::Cell::new(0));
        let mut records = Records::new(&counting);
        for (n, loc) in locs.iter().enumerate() {
            assert_eq!(records.get(*loc).unwrap(), (id(n as u8), blob(1000)));
        }
        let mut distinct = pages.clone();
        distinct.dedup();
        assert_eq!(counting.1.get(), distinct.len());

        for n in [0, 29, 1] {
            assert_eq!(records.get(locs[n]).unwrap().0, id(n as u8));
        }
        assert_eq!(counting.1.get(), distinct.len() + 3);
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let loc = insert_record(&mut store, &mut current, id(1), &Document::Null).unwrap();
        let mut page = read_data_page(&store, loc.page).unwrap();
        let mut cell = page.get_cell(loc.slot).unwrap().to_vec();
        cell[0] = 0x7F;
        assert!(page.update_cell(loc.slot, &cell));
        store.write_page(loc.page, &page.into_bytes()).unwrap();

        assert_eq!(
            get_record(&store, loc).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn insert_then_get_roundtrips() {
        let (_dir, mut store) = store();
        let mut current = 0;

        let doc = Document::String("Ada".to_string());
        let loc = insert_record(&mut store, &mut current, id(1), &doc).unwrap();
        assert_eq!(
            current, loc.page,
            "the first insert allocates the current page"
        );

        let (found_id, found_doc) = get_record(&store, loc).unwrap();
        assert_eq!(found_id, id(1));
        assert_eq!(found_doc, doc);
    }

    #[test]
    fn small_documents_share_a_page_until_it_fills() {
        let (_dir, mut store) = store();
        let mut current = 0;

        let first = insert_record(&mut store, &mut current, id(0), &blob(1000)).unwrap();
        let mut locs = vec![first];
        while current == first.page {
            let n = locs.len() as u8;
            locs.push(insert_record(&mut store, &mut current, id(n), &blob(1000)).unwrap());
        }

        // Seven ~1 KB documents fit in 8 KB; the eighth opened a new page.
        assert_eq!(locs.len(), 8);
        for (slot, loc) in locs[..7].iter().enumerate() {
            assert_eq!(
                *loc,
                RecordLocation {
                    page: first.page,
                    slot: slot as u16
                }
            );
        }
        assert_eq!(locs[7].slot, 0);
        for (n, loc) in locs.iter().enumerate() {
            assert_eq!(get_record(&store, *loc).unwrap().0, id(n as u8));
        }
    }

    #[test]
    fn update_in_place_keeps_the_location() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let loc = insert_record(&mut store, &mut current, id(2), &Document::Int(1)).unwrap();
        let neighbor = insert_record(&mut store, &mut current, id(3), &blob(500)).unwrap();

        let new_loc = update_record(&mut store, &mut current, loc, id(2), &blob(3000)).unwrap();

        assert_eq!(new_loc, loc);
        assert_eq!(get_record(&store, loc).unwrap().1, blob(3000));
        assert_eq!(get_record(&store, neighbor).unwrap().1, blob(500));
    }

    #[test]
    fn update_that_outgrows_its_page_moves_the_document() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let a = insert_record(&mut store, &mut current, id(1), &blob(3000)).unwrap();
        let b = insert_record(&mut store, &mut current, id(2), &blob(3000)).unwrap();

        let moved = update_record(&mut store, &mut current, a, id(1), &blob(6000)).unwrap();

        assert_ne!(moved.page, a.page);
        assert_eq!(current, moved.page, "the move allocated a new current page");
        assert_eq!(get_record(&store, moved).unwrap(), (id(1), blob(6000)));
        assert_eq!(get_record(&store, b).unwrap(), (id(2), blob(3000)));
        assert_eq!(
            get_record(&store, a).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "the old slot is gone"
        );
    }

    #[test]
    fn delete_frees_a_page_once_it_is_empty_but_not_the_current_one() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let a = insert_record(&mut store, &mut current, id(1), &blob(5000)).unwrap();
        let b = insert_record(&mut store, &mut current, id(2), &blob(5000)).unwrap();
        assert_ne!(a.page, b.page);
        assert_eq!(current, b.page);

        // b's page is current: emptied, but kept for the next insert.
        delete_record(&mut store, current, b).unwrap();
        let c = insert_record(&mut store, &mut current, id(3), &Document::Null).unwrap();
        assert_eq!(c.page, b.page);

        // a's page isn't: emptying it frees it for reuse.
        delete_record(&mut store, current, a).unwrap();
        assert_eq!(store.allocate_page().unwrap(), a.page);
    }

    #[test]
    fn deleted_space_on_the_current_page_is_reused() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let a = insert_record(&mut store, &mut current, id(1), &blob(4000)).unwrap();
        insert_record(&mut store, &mut current, id(2), &blob(4000)).unwrap();

        delete_record(&mut store, current, a).unwrap();
        let c = insert_record(&mut store, &mut current, id(3), &blob(4000)).unwrap();

        assert_eq!(c, a, "same page, same (reused) slot");
    }

    #[test]
    fn a_document_goes_to_overflow_pages_exactly_when_it_cannot_be_inline() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let largest = largest_inline_blob();

        let inline = insert_record(&mut store, &mut current, id(1), &blob(largest)).unwrap();
        let overflow = insert_record(&mut store, &mut current, id(2), &blob(largest + 1)).unwrap();

        assert_eq!(flags(&store, inline), INLINE);
        assert_eq!(flags(&store, overflow), OVERFLOW);
        assert_eq!(get_record(&store, inline).unwrap(), (id(1), blob(largest)));
        assert_eq!(
            get_record(&store, overflow).unwrap(),
            (id(2), blob(largest + 1))
        );
    }

    #[test]
    fn a_large_document_spans_a_chain_and_its_cell_is_packed() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let small = insert_record(&mut store, &mut current, id(1), &blob(100)).unwrap();

        // 3 × capacity, minus the tag and length: exactly three full pages.
        let big = blob(3 * OVERFLOW_CAPACITY - 5);
        let loc = insert_record(&mut store, &mut current, id(2), &big).unwrap();

        assert_eq!(
            loc.page, small.page,
            "the pointer cell shares the data page"
        );
        // Data page + 3 overflow pages (+ header, catalog-free store).
        assert_eq!(store.allocate_page().unwrap(), small.page + 4);
        assert_eq!(get_record(&store, loc).unwrap(), (id(2), big));
    }

    #[test]
    fn documents_past_64_kb_roundtrip() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let prose = Document::String("It was a dark and stormy night. ".repeat(10_000));

        let loc = insert_record(&mut store, &mut current, id(1), &prose).unwrap();

        assert_eq!(get_record(&store, loc).unwrap(), (id(1), prose));
    }

    #[test]
    fn updates_switch_between_inline_and_overflow_and_free_old_chains() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let loc = insert_record(&mut store, &mut current, id(1), &blob(10)).unwrap();
        let data_page = loc.page;

        // Inline → overflow: the cell shrinks to a pointer and stays put.
        let loc = update_record(&mut store, &mut current, loc, id(1), &blob(20_000)).unwrap();
        assert_eq!(loc.page, data_page);
        assert_eq!(flags(&store, loc), OVERFLOW);
        assert_eq!(get_record(&store, loc).unwrap().1, blob(20_000));

        // Overflow → larger overflow: the old three pages are reused.
        let loc = update_record(&mut store, &mut current, loc, id(1), &blob(30_000)).unwrap();
        assert_eq!(get_record(&store, loc).unwrap().1, blob(30_000));
        let high_water = store.allocate_page().unwrap();
        assert_eq!(high_water, data_page + 5, "data page + 4 overflow pages");
        store.free_page(high_water).unwrap();

        // Overflow → inline: the chain is freed.
        let loc = update_record(&mut store, &mut current, loc, id(1), &blob(10)).unwrap();
        assert_eq!(flags(&store, loc), INLINE);
        assert_eq!(get_record(&store, loc).unwrap().1, blob(10));
        let mut freed: Vec<PageId> = (0..5).map(|_| store.allocate_page().unwrap()).collect();
        freed.sort();
        assert_eq!(freed, (data_page + 1..=data_page + 5).collect::<Vec<_>>());
    }

    #[test]
    fn delete_frees_the_chain() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let keep = insert_record(&mut store, &mut current, id(1), &blob(10)).unwrap();
        let big = insert_record(&mut store, &mut current, id(2), &blob(20_000)).unwrap();

        delete_record(&mut store, current, big).unwrap();

        let mut freed: Vec<PageId> = (0..3).map(|_| store.allocate_page().unwrap()).collect();
        freed.sort();
        assert_eq!(freed, vec![keep.page + 1, keep.page + 2, keep.page + 3]);
        assert_eq!(
            store.allocate_page().unwrap(),
            keep.page + 4,
            "nothing else was free"
        );
    }

    #[test]
    fn a_broken_chain_is_a_corruption_error() {
        let (_dir, mut store) = store();
        let mut current = 0;
        let loc = insert_record(&mut store, &mut current, id(1), &blob(20_000)).unwrap();
        let page = read_data_page(&store, loc.page).unwrap();
        let (_len, first) = Cell::parse(page.get_cell(loc.slot).unwrap())
            .unwrap()
            .chain()
            .unwrap();
        let first_page = store.read_page(first).unwrap();
        let second = PageId::from_le_bytes(first_page[1..9].try_into().unwrap());

        // Cut the chain after its second page: it ends early.
        let mut page = store.read_page(second).unwrap();
        page[1..9].copy_from_slice(&0u64.to_le_bytes());
        store.write_page(second, &page).unwrap();
        let err = get_record(&store, loc).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("early"), "{err}");

        // Point it at a page that isn't an overflow page.
        page[1..9].copy_from_slice(&loc.page.to_le_bytes());
        store.write_page(second, &page).unwrap();
        let err = get_record(&store, loc).unwrap_err();
        assert!(err.to_string().contains("isn't an overflow page"), "{err}");
        assert_eq!(
            delete_record(&mut store, current, loc).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "a delete must not free pages of a chain it can't follow"
        );
    }

    #[test]
    fn get_rejects_wrong_page_type() {
        let (_dir, mut store) = store();

        // Page 1 is always the catalog's page — asking for it as a data
        // record should be a corruption error, not a misdecoded document.
        let _catalog = crate::catalog::Catalog::load(&mut store).unwrap();
        let bogus_loc = RecordLocation { page: 1, slot: 0 };

        let err = get_record(&store, bogus_loc).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
