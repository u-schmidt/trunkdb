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
pub(crate) use file::{clear_binding, rewrite_format_version, rewrite_releases};
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

/// What a WAL's header says of the file it belongs to (SPEC §183): the
/// file's id, the session the file's header had when the WAL started
/// from empty, and the session the WAL's batches were logged in, which
/// their images of the header carry. Recovery replays a WAL only into
/// the file with that id, at either session: the one before, as a crash
/// left it, or the WAL's own, which a checkpoint that didn't get to cut
/// the WAL had already written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct WalBinding {
    pub(crate) file_id: [u8; 16],
    pub(crate) prev: u64,
    pub(crate) session: u64,
}

impl WalBinding {
    /// Whether this WAL may be replayed into a file whose header says
    /// `file_id` and `session` (`FileStore::disk_binding`). A file
    /// without an id, new or from before, takes only a WAL started on
    /// a file without one.
    pub(crate) fn belongs_to(&self, file_id: [u8; 16], session: u64) -> bool {
        if file_id == [0; 16] {
            return self.prev == 0;
        }
        file_id == self.file_id && (session == self.prev || session == self.session)
    }
}

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

/// Syncs the directory `path` is in, so that a file just created or
/// renamed there is still under its name after a power cut (SPEC §182):
/// a file's own sync doesn't promise its directory entry. Unix only;
/// Windows can't open a directory as a file, and NTFS journals the
/// entry. Not under `--cfg fuzzing`, as `sync`.
pub(crate) fn sync_dir(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(test)]
    DIR_SYNCS.with(|syncs| syncs.set(syncs.get() + 1));
    if cfg!(fuzzing) || cfg!(not(unix)) {
        return Ok(());
    }
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => std::path::Path::new("."),
    };
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
thread_local! {
    /// Test-only: how many times `sync_dir` ran on this thread.
    pub(crate) static DIR_SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Opens the file at `path` for reading and writing, creating it if it
/// isn't there, as `create(true)` would; a file it creates has its
/// directory synced before it is handed out (`sync_dir`), so that what
/// is written to it later can't be lost with its name.
pub(crate) fn open_or_create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    loop {
        match options.clone().create_new(true).open(path) {
            Ok(file) => {
                sync_dir(path)?;
                return Ok(file);
            }
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            Err(_) => {}
        }
        match options.open(path) {
            // Gone again between the two: try creating it once more.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            opened => return opened,
        }
    }
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
    /// An index leaf whose keys are stored after an anchor's (file
    /// format 14, SPEC §171, `index/leaf.rs`).
    IndexLeafPacked = 7,
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
            7 => Ok(PageType::IndexLeafPacked),
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
    /// `write_page` of `page` after `change`, for a page just read to
    /// change it (SPEC §151): a store holding a copy of its own, as a
    /// batch holds the pages it changed, lets go of it first, so the
    /// change is made in place instead of on a copy of the whole page.
    fn change_page(
        &mut self,
        id: PageId,
        mut page: SlottedPage,
        change: &mut dyn FnMut(&mut SlottedPage),
    ) -> std::io::Result<()> {
        change(&mut page);
        self.write_page(id, page.into_page())
    }
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
