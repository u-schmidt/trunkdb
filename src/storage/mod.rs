mod cache;
mod file;
mod page;
mod pages;
mod scratch;
mod slotted;
mod snapshot;

pub(crate) use file::HEADER_PAGE;
pub use file::{DEFAULT_CACHE_SIZE, FileStore};
#[cfg(test)]
pub(crate) use file::{rewrite_format_version, rewrite_releases};
pub use page::Page;
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) use pages::checksum;
pub(crate) use pages::{DEFAULT_VERSION_LIMIT, Pages, TooOld};
pub use pages::{PAGE_SIZE, USABLE_PAGE_SIZE};
#[cfg(test)]
pub(crate) use scratch::FAIL_CREATE;
pub(crate) use scratch::{Scratch, ScratchStore, scratch_path};
pub use slotted::SlottedPage;
pub(crate) use snapshot::{Commit, SnapshotStore};

pub type PageId = u64;

/// `file.sync_all()`, the flush every durability promise rests on —
/// except under `--cfg fuzzing` (SPEC §55.1), which `cargo fuzz` sets and
/// which tests decoding, not durability. A cfg and not the `fuzzing`
/// feature: a feature can be switched on by `--all-features` or by any
/// crate that depends on this one, and would turn the promise off
/// without a word.
pub(crate) fn sync(file: &std::fs::File) -> std::io::Result<()> {
    if cfg!(fuzzing) {
        return Ok(());
    }
    file.sync_all()
}

/// A page's id plus its full bytes (`USABLE_PAGE_SIZE` long) — what the WAL
/// records and crash recovery writes back (`FileStore::restore_pages`).
/// For tests: recovery itself takes `PendingPages`, and holds no list.
#[cfg(test)]
pub type PageImage = (PageId, Vec<u8>);

/// What `PendingPages::outline` shows each image to: its page, and the
/// image itself if it is the header's.
pub type SeePage<'a> = dyn FnMut(PageId, Option<&[u8]>) -> std::io::Result<()> + 'a;

/// What `PendingPages::pages` hands each image to.
pub type TakePage<'a> = dyn FnMut(PageId, &[u8]) -> std::io::Result<()> + 'a;

/// Page images to restore, in the order they were logged (SPEC §91):
/// what `FileStore::restore_pages` takes, without asking for them all in
/// memory. The WAL's `Pending` reads them from its file as they are
/// wanted; for tests, a list of `PageImage`s is one too.
pub trait PendingPages {
    /// How many images there are.
    fn count(&self) -> u64;

    /// Every image's page, in order, and the image itself where it is
    /// the header's: what is checked before anything is restored.
    fn outline(&self, see: &mut SeePage<'_>) -> std::io::Result<()>;

    /// Every image, in order.
    fn pages(&self, take: &mut TakePage<'_>) -> std::io::Result<()>;
}

#[cfg(test)]
impl PendingPages for [PageImage] {
    fn count(&self) -> u64 {
        self.len() as u64
    }

    fn outline(&self, see: &mut SeePage<'_>) -> std::io::Result<()> {
        self.iter()
            .try_for_each(|(id, page)| see(*id, (*id == HEADER_PAGE).then_some(&page[..])))
    }

    fn pages(&self, take: &mut TakePage<'_>) -> std::io::Result<()> {
        self.iter().try_for_each(|(id, page)| take(*id, page))
    }
}

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
    /// The page's current bytes, shared with the cache, not copied
    /// (SPEC §64): `Page::make_mut` copies them before a change.
    fn read_page(&self, id: PageId) -> std::io::Result<Page>;
    /// Like `read_page`, but `Ok(None)` for a page that was never
    /// allocated — an expected, ordinary outcome (e.g. "this is a fresh
    /// database") rather than an error. Only distinguishes "never
    /// allocated" from "allocated"; it does not know whether an allocated
    /// page is currently on the free list — callers that care (like the
    /// catalog, which is never freed) verify that separately, e.g. via the
    /// page's own type tag.
    fn try_read_page(&self, id: PageId) -> std::io::Result<Option<Page>>;
    /// Takes the page itself (SPEC §67): a page changed through
    /// `Page::make_mut` was copied once, on its first change, and is
    /// stored as it is, not copied again.
    fn write_page(&mut self, id: PageId, data: Page) -> std::io::Result<()>;
    fn free_page(&mut self, id: PageId) -> std::io::Result<()>;
}

#[cfg(test)]
impl<const N: usize> PendingPages for [PageImage; N] {
    fn count(&self) -> u64 {
        self[..].count()
    }

    fn outline(&self, see: &mut SeePage<'_>) -> std::io::Result<()> {
        self[..].outline(see)
    }

    fn pages(&self, take: &mut TakePage<'_>) -> std::io::Result<()> {
        self[..].pages(take)
    }
}

#[cfg(test)]
impl PendingPages for Vec<PageImage> {
    fn count(&self) -> u64 {
        self[..].count()
    }

    fn outline(&self, see: &mut SeePage<'_>) -> std::io::Result<()> {
        self[..].outline(see)
    }

    fn pages(&self, take: &mut TakePage<'_>) -> std::io::Result<()> {
        self[..].pages(take)
    }
}
