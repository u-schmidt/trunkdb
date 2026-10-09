use super::file::{HEADER_PAGE, Header, free_list};
use super::pages::Pages;
use super::{Page, PageId, PageStore};
use std::io;

/// A commit, as a reader needs it (SPEC §78): its number, which says
/// which version of each page to read, and its header, which says how
/// many pages there were and where the free list started.
#[derive(Clone, Copy)]
pub(crate) struct Commit {
    seq: u64,
    header: Header,
}

impl Commit {
    pub(super) fn new(seq: u64, header: Header) -> Self {
        Commit { seq, header }
    }

    /// The commit's number, counted from 0 at open (SPEC §77.4).
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }
}

/// The pages as of one commit (SPEC §78): what §57 called "one more
/// `PageStore`". Every read goes through one, so a reader sees the
/// state of its commit and nothing a writer is staging. It can't
/// write: `allocate_page`, `write_page` and `free_page` are errors.
pub(crate) struct SnapshotStore<'a> {
    pages: &'a Pages,
    commit: Commit,
}

impl Pages {
    /// The pages as of `commit`, to read.
    pub(crate) fn at(&self, commit: Commit) -> SnapshotStore<'_> {
        SnapshotStore {
            pages: self,
            commit,
        }
    }
}

impl SnapshotStore<'_> {
    /// How many pages the file had, the header included.
    pub(crate) fn page_count(&self) -> u64 {
        self.commit.header.page_count
    }

    /// The format version the header said: this build's, or an older
    /// one it reads as it is, until the header is next written.
    pub(crate) fn format_version(&self) -> io::Result<u32> {
        let buf = self.pages.read_at(HEADER_PAGE, self.commit.seq)?;
        Ok(u32::from_le_bytes(buf[28..32].try_into().unwrap()))
    }

    /// The pages on the free list, in list order (`free_list`).
    pub(crate) fn free_pages(&self) -> io::Result<Vec<PageId>> {
        free_list(&self.commit.header, |id| {
            self.pages.read_at(id, self.commit.seq)
        })
    }

    /// The pages whose checksum doesn't match their bytes on disk
    /// (`Pages::damaged`): of the file as it is now, not as of the
    /// commit, since a page's older bytes aren't on disk any more.
    pub(crate) fn damaged_pages(&self) -> io::Result<Vec<PageId>> {
        self.pages.damaged(self.commit.header.page_count)
    }

    fn read_only<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a snapshot is read-only",
        ))
    }
}

impl PageStore for SnapshotStore<'_> {
    fn allocate_page(&mut self) -> io::Result<PageId> {
        Self::read_only()
    }

    fn read_page(&self, id: PageId) -> io::Result<Page> {
        if id >= self.commit.header.page_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page {id} out of bounds (page_count = {})",
                    self.commit.header.page_count
                ),
            ));
        }
        self.pages.read_at(id, self.commit.seq)
    }

    fn try_read_page(&self, id: PageId) -> io::Result<Option<Page>> {
        if id >= self.commit.header.page_count {
            return Ok(None);
        }
        self.pages.read_at(id, self.commit.seq).map(Some)
    }

    fn write_page(&mut self, _id: PageId, _data: Page) -> io::Result<()> {
        Self::read_only()
    }

    fn free_page(&mut self, _id: PageId) -> io::Result<()> {
        Self::read_only()
    }
}
