mod file;
mod slotted;

pub use file::{FileStore, PAGE_SIZE};
pub use slotted::SlottedPage;

pub type PageId = u64;

/// A page's id plus its full bytes (`PAGE_SIZE` long) — what the WAL
/// records and crash recovery writes back (`FileStore::restore_pages`).
pub type PageImage = (PageId, Vec<u8>);

/// Where a document's cell lives: a specific slot on a specific page, not a
/// raw byte offset (see `SlottedPage`). The slot's actual byte offset
/// inside the page can move later (e.g. during a future compaction pass)
/// without invalidating anything that holds this reference — only the
/// page's slot directory changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLocation {
    pub page: PageId,
    pub slot: u16,
}

/// What a page currently holds. Written as the first byte of every page
/// except the header (page 0, always identifiable positionally — see
/// `FileStore`) — `Header` exists here only for completeness, it's never
/// actually written via this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    Header = 0,
    Free = 1,
    Catalog = 2,
    Data = 3,
    IndexLeaf = 4,
    IndexBranch = 5,
    /// Part of a document too large for one page (SPEC §26). Not a
    /// `SlottedPage`: `data.rs` owns the layout.
    Overflow = 6,
}

impl PageType {
    fn from_u8(v: u8) -> std::io::Result<Self> {
        match v {
            0 => Ok(PageType::Header),
            1 => Ok(PageType::Free),
            2 => Ok(PageType::Catalog),
            3 => Ok(PageType::Data),
            4 => Ok(PageType::IndexLeaf),
            5 => Ok(PageType::IndexBranch),
            6 => Ok(PageType::Overflow),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown page type tag {other} — file may be corrupt"),
            )),
        }
    }
}

/// The one load-bearing seam in this crate: everything above `PageStore`
/// works in terms of pages, never raw file offsets. Unlike `Index`,
/// `TransactionManager` and `Durability` below, this is NOT meant to be
/// faked — get the contract right here, because changing it later ripples
/// through every layer that sits on top of it.
pub trait PageStore {
    fn allocate_page(&mut self) -> std::io::Result<PageId>;
    fn read_page(&self, id: PageId) -> std::io::Result<Vec<u8>>;
    /// Like `read_page`, but `Ok(None)` for a page that was never
    /// allocated — an expected, ordinary outcome (e.g. "this is a fresh
    /// database") rather than an error. Only distinguishes "never
    /// allocated" from "allocated"; it does not know whether an allocated
    /// page is currently on the free list — callers that care (like the
    /// catalog, which is never freed) verify that separately, e.g. via the
    /// page's own type tag.
    fn try_read_page(&self, id: PageId) -> std::io::Result<Option<Vec<u8>>>;
    fn write_page(&mut self, id: PageId, data: &[u8]) -> std::io::Result<()>;
    fn free_page(&mut self, id: PageId) -> std::io::Result<()>;
}
