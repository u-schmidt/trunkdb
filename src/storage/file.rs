use super::pages::{
    PAGE_SIZE, Pages, USABLE_PAGE_SIZE, checksum_matches, damaged, read_disk_page, read_exact_at,
};
use super::snapshot::Commit;
#[cfg(test)]
use super::snapshot::SnapshotStore;
use super::{Page, PageId, PageImage, PageStore, PageType};
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// How much of the file `FileStore` keeps in memory unless told
/// otherwise (`OpenOptions::cache_size`, SPEC §50): 256 MiB, 32,768
/// pages. It fills only as pages are read, so a small file costs its own
/// size; redb and sled default to 1 GiB.
pub const DEFAULT_CACHE_SIZE: usize = 256 << 20;

const MAGIC: &[u8; 8] = b"TRUNKDB1";
/// The on-disk format this build reads and writes — bumped whenever a
/// page or cell layout changes. `0` (the field's bytes in every header
/// written before it existed) marks a pre-versioning file: trunkdb 0.1.0,
/// or a development build between 0.1.0 and this field (SPEC §21.2).
/// History: 1 = SPEC §21; 2 = `u32` lengths in documents and overflow
/// pages (SPEC §26); 3 = catalog cells with a kind byte, and index
/// entries (SPEC §28); 4 = indexes hold null and missing fields (SPEC §32);
/// 5 = unique indexes, a new catalog cell kind (SPEC §33); 6 = a
/// checksum at the end of every page (SPEC §40); 7 = multikey indexes,
/// on paths with `[*]` (SPEC §42), which a build before them would fill
/// as if the path named one value; 8 = compound indexes, new catalog
/// cell kinds (SPEC §43); 9 = documents stored without their `_id`,
/// which reads take from the cell (SPEC §59): a build before it would
/// read them without ids; 10 = ids in index keys (SPEC §62): a build
/// before it would leave their entries behind when it deletes; 11 =
/// date-times in documents and index keys (SPEC §69), a type tag a build
/// before it can't decode.
const FORMAT_VERSION: u32 = 11;
/// Older formats this build opens as they are, because such a file *is*
/// a valid `FORMAT_VERSION` file — one that uses none of what came since
/// (for 4: unique indexes; for 8: a document's `_id` stored in it too,
/// which reads replace with the cell's; for 9: no index holding an id,
/// which `Database::open` makes sure of first). Every header write stamps
/// `FORMAT_VERSION`, and the first page written in a batch writes the
/// header too if it's older (SPEC §59) — so a file that uses something
/// newer always says so, and an older build refuses it instead of
/// misreading it (SPEC §33.4). 4 and 5 aren't: every page of theirs lacks
/// the checksum (6), and uses the bytes where it now goes.
const COMPATIBLE_OLDER_FORMATS: [u32; 5] = [6, 7, 8, 9, 10];
pub(super) const HEADER_PAGE: PageId = 0;
// Page 0 is reserved for the header and is never itself a free/data page,
// so 0 doubles safely as "no free page" within the free list.
const NO_FREE_PAGE: PageId = 0;

// Header page layout (rest of the page beyond this is reserved/zeroed,
// up to the checksum every page ends with):
//   [0..8)   magic
//   [8..12)  page_size: u32
//   [12..20) page_count: u64
//   [20..28) free_list_head: u64 (PageId, 0 = none)
//   [28..32) format_version: u32 (FORMAT_VERSION)
#[derive(Clone, Copy)]
pub(super) struct Header {
    page_size: u32,
    pub(super) page_count: u64,
    pub(super) free_list_head: PageId,
    /// What the file on disk says: `FORMAT_VERSION`, or an older one this
    /// build reads as it is, until the next header write stamps the
    /// current one. `encode` always writes `FORMAT_VERSION`.
    format_version: u32,
}

impl Header {
    fn encode(&self) -> [u8; USABLE_PAGE_SIZE] {
        let mut buf = [0u8; USABLE_PAGE_SIZE];
        buf[0..8].copy_from_slice(MAGIC);
        buf[8..12].copy_from_slice(&self.page_size.to_le_bytes());
        buf[12..20].copy_from_slice(&self.page_count.to_le_bytes());
        buf[20..28].copy_from_slice(&self.free_list_head.to_le_bytes());
        buf[28..32].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> io::Result<Self> {
        if &buf[0..8] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a trunkdb file (bad magic)",
            ));
        }
        // Checked before anything else in the header: another format
        // version may not even lay the rest of the header out this way.
        let format_version = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        if format_version != FORMAT_VERSION && !COMPATIBLE_OLDER_FORMATS.contains(&format_version) {
            let message = match format_version {
                0 => "file was written before format versioning (trunkdb 0.1.0 or an early \
                      development build), which this build can't read"
                    .to_string(),
                v if v > FORMAT_VERSION => format!(
                    "file has format {v}, newer than this build's {FORMAT_VERSION} — \
                     open it with a newer trunkdb"
                ),
                v => format!(
                    "file has format {v}, older than this build's {FORMAT_VERSION}: \
                     export it with the trunkdb version that wrote it (`trunkdb export`), \
                     and import the export into a new file with this one"
                ),
            };
            return Err(io::Error::new(io::ErrorKind::InvalidData, message));
        }
        let page_size = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if page_size as usize != PAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("page size mismatch: file has {page_size}, this build expects {PAGE_SIZE}"),
            ));
        }
        let page_count = u64::from_le_bytes(buf[12..20].try_into().unwrap());
        let free_list_head = u64::from_le_bytes(buf[20..28].try_into().unwrap());
        // The header counts itself, every page's offset fits in a `u64`,
        // and the free list starts inside the file (SPEC §55): a count of
        // 0 would hand out page 0, the header, as the next new page.
        if page_count == 0 || page_count > u64::MAX / PAGE_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("header counts {page_count} pages — file may be corrupt"),
            ));
        }
        if free_list_head >= page_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "free list starts at page {free_list_head}, past the {page_count} pages — file may be corrupt"
                ),
            ));
        }
        Ok(Header {
            page_size,
            page_count,
            free_list_head,
            format_version,
        })
    }
}

/// The real, load-bearing page store: a single file, fixed-size pages, a
/// header page (0) holding page count + free-list head, and free pages
/// linked into an on-disk free list (each free page's own bytes hold the
/// next free page id — the same technique SQLite's freelist trunk pages
/// use, no separate bitmap page needed).
///
/// Invariant: the content of a freshly allocated page (whether newly
/// grown or popped off the free list) is unspecified until the caller
/// writes to it — allocation does not zero pages reused from the free
/// list, matching how most page allocators work.
///
/// Staging (`begin` … `write_back` or `rollback`): while active, every
/// page change — `write_page`, `free_page`'s free-list link, and the
/// header updates `allocate_page`/`free_page` make — lands in an
/// in-memory dirty set instead of the file, and every read checks that
/// set first, so a batch sees its own writes. Nothing reaches the file
/// until `write_back`; `rollback` drops the lot. This is the foundation
/// for the page-image WAL (SPEC §19.2): the dirty set is exactly what
/// gets logged. Staging lives here rather than in a wrapper `PageStore`
/// because the header is managed internally, bypassing `write_page` — a
/// wrapper would have to duplicate the allocation logic to see it.
pub struct FileStore {
    /// The committed pages: the file, the cache, and the pages waiting
    /// for a checkpoint (SPEC §77). What a reader reads, and shared with
    /// the readers (SPEC §79): `Database` holds it too.
    pub(crate) pages: Arc<Pages>,
    /// The header as the batch in the making has it; as of the last
    /// commit when none is staged. The writer's, like `staging`.
    header: Header,
    /// Whether the header's checksum has been checked — not yet between
    /// `open_before_recovery` and `check_header` (SPEC §40.4).
    header_checked: bool,
    /// The batch in the making: the writer's alone (SPEC §77). Nothing
    /// of it is in `pages` before `commit`.
    staging: Option<Staging>,
}

struct Staging {
    /// The header as of `begin`, restored by `rollback`.
    header_before: Header,
    /// Every page changed since `begin`, keyed by id — so a page written
    /// many times in one batch is held (and later logged) once. Includes
    /// the header, as page 0, once anything has changed it.
    dirty: BTreeMap<PageId, Page>,
    /// Each of those pages as the batch found it, the committed one:
    /// what a snapshot open at the commit goes on reading (SPEC §80).
    /// Not for a page past the end of the file as it was, which no such
    /// snapshot reads, nor for one that couldn't be read.
    before: BTreeMap<PageId, Page>,
    /// Whether the batch replaces the whole file (`replace_all`): then
    /// nothing is kept in `before`, and the batch may only commit with
    /// no snapshot open (`FileStore::replaces_all`).
    replaces_all: bool,
}

impl FileStore {
    /// Opens (or creates) the file and takes an exclusive lock on it,
    /// held until the `FileStore` is dropped. A second open while it's
    /// held — from another process, or another handle in this one — fails
    /// with `ErrorKind::WouldBlock` instead of waiting: two writers, each
    /// with its own cached header and catalog, would corrupt the file.
    /// The lock is the OS's advisory file lock (`flock`/`LockFileEx`), so
    /// it only stops other trunkdb opens, not arbitrary programs, and it
    /// dies with the process — no stale lock file after a crash.
    #[cfg(test)]
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut store = Self::open_before_recovery(path)?;
        store.check_header()?;
        Ok(store)
    }

    /// `open`, but without checking the header's checksum yet — for
    /// `Database::open`, which recovers from the WAL first. A crash can
    /// tear the header's write, leaving its fields (in its first bytes)
    /// new and its checksum (in its last) old; the WAL holds the whole
    /// page, and `restore_pages` writes it back. `check_header` then
    /// finds only real damage.
    pub(crate) fn open_before_recovery(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Self::from_file(file, true)
    }

    /// `open` on an already-open file; `lock: false` only for tests, which
    /// look at the file through a second handle (`on_disk`).
    fn from_file(file: File, lock: bool) -> io::Result<Self> {
        if lock {
            // Before reading anything: what we'd read could be mid-change
            // by the holder.
            file.try_lock().map_err(|e| match e {
                std::fs::TryLockError::WouldBlock => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "database file is already open (by another process, or another handle in this one)",
                ),
                std::fs::TryLockError::Error(e) => e,
            })?;
        }

        // Fresh unless a complete header page is on disk. The header isn't
        // written here: it reaches the file with the first write that
        // changes it (the catalog bootstrap's allocation, via staging and
        // the WAL when `Database` opens the file), so a crash before then
        // leaves an empty file — still fresh next time — rather than a
        // file with a header and nothing else. A non-empty file shorter
        // than one page is a first write-back that a crash cut short —
        // its batch is in the WAL, and `restore_pages` writes it out in
        // full — but only if it starts like one: the header page goes out
        // first, so its bytes begin with the magic. Anything else is some
        // other program's file, which a fresh bootstrap would overwrite
        // (SPEC §22.1).
        let len = file.metadata()?.len();
        let is_fresh = len < PAGE_SIZE as u64;
        if is_fresh && len > 0 {
            let mut start = vec![0u8; (len as usize).min(MAGIC.len())];
            read_exact_at(&file, &mut start, 0)?;
            if !MAGIC.starts_with(&start) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "not a trunkdb file (bad magic)",
                ));
            }
        }
        let header = if is_fresh {
            Header {
                page_size: PAGE_SIZE as u32,
                page_count: 1, // just the header page so far
                free_list_head: NO_FREE_PAGE,
                format_version: FORMAT_VERSION,
            }
        } else {
            Header::decode(&read_disk_page(&file, HEADER_PAGE)?[..USABLE_PAGE_SIZE])?
        };

        Ok(Self {
            pages: Arc::new(Pages::new(file, DEFAULT_CACHE_SIZE / PAGE_SIZE)),
            header,
            // A fresh file's header isn't on disk yet: nothing to check.
            header_checked: is_fresh,
            staging: None,
        })
    }

    /// Checks the header's checksum, if `open_before_recovery` left it
    /// unchecked.
    pub(crate) fn check_header(&mut self) -> io::Result<()> {
        if !self.header_checked {
            if !checksum_matches(
                HEADER_PAGE,
                &read_disk_page(self.pages.file(), HEADER_PAGE)?,
            ) {
                return Err(damaged(HEADER_PAGE));
            }
            self.header_checked = true;
        }
        Ok(())
    }

    /// How many pages the file has, the header included.
    pub(crate) fn page_count(&self) -> u64 {
        self.header.page_count
    }

    /// The format version the header says — `FORMAT_VERSION`, or an older
    /// one this build reads as it is (`COMPATIBLE_OLDER_FORMATS`) until
    /// the header is next written.
    pub(crate) fn format_version(&self) -> io::Result<u32> {
        let buf = self.read_current(HEADER_PAGE)?;
        Ok(u32::from_le_bytes(buf[28..32].try_into().unwrap()))
    }

    /// Whether the file is from before ids had index keys (format 10,
    /// SPEC §62), so an index may lack entries for them.
    pub(crate) fn predates_id_keys(&self) -> io::Result<bool> {
        Ok(self.format_version()? < 10)
    }

    /// Stamps the header with `FORMAT_VERSION`, in the current batch, if
    /// it says an older one: for a change no page write shows (SPEC §62).
    pub(crate) fn stamp_format_version(&mut self) -> io::Result<()> {
        if self.header.format_version != FORMAT_VERSION {
            self.write_header()?;
        }
        Ok(())
    }

    /// The pages on the free list, in list order, as the batch in the
    /// making has it (`free_list`).
    #[cfg(test)]
    pub(crate) fn free_pages(&self) -> io::Result<Vec<PageId>> {
        free_list(&self.header, |id| self.read_current(id))
    }

    /// The pages whose checksum doesn't match their bytes on disk
    /// (`Pages::damaged`).
    #[cfg(test)]
    pub(crate) fn damaged_pages(&self) -> io::Result<Vec<PageId>> {
        self.pages.damaged(self.header.page_count)
    }

    /// The last commit: its number and its header. Not while staging,
    /// when `header` is the batch's.
    pub(crate) fn last_commit(&self) -> Commit {
        assert!(
            self.staging.is_none(),
            "FileStore::last_commit while staging"
        );
        Commit::new(self.pages.seq(), self.header)
    }

    /// The pages as of `commit`, to read (SPEC §78).
    #[cfg(test)]
    pub(crate) fn at(&self, commit: Commit) -> SnapshotStore<'_> {
        self.pages.at(commit)
    }

    fn write_header(&mut self) -> io::Result<()> {
        let header = self.header.encode();
        self.write_raw(HEADER_PAGE, Page::from(&header[..]))?;
        self.header.format_version = FORMAT_VERSION;
        Ok(())
    }

    /// A page's current bytes: from the dirty set if it's there, the
    /// writer's own view; otherwise the committed page (`Pages::read`).
    /// Shared wherever it comes from, never copied (SPEC §64). No bounds
    /// check — callers do that.
    fn read_current(&self, id: PageId) -> io::Result<Page> {
        if let Some(staging) = &self.staging
            && let Some(page) = staging.dirty.get(&id)
        {
            return Ok(page.clone());
        }
        self.pages.read(id)
    }

    /// Writes a page: into the dirty set while staging, otherwise
    /// straight to the file — and the cache. No bounds check — callers do
    /// that.
    fn write_raw(&mut self, id: PageId, data: Page) -> io::Result<()> {
        match &mut self.staging {
            Some(staging) => {
                if !staging.replaces_all
                    && id < staging.header_before.page_count
                    && !staging.dirty.contains_key(&id)
                    // A page that can't be read is one a snapshot couldn't
                    // read either: the write goes ahead without it.
                    && let Ok(before) = self.pages.read(id)
                {
                    staging.before.insert(id, before);
                }
                staging.dirty.insert(id, data);
                Ok(())
            }
            None => self.pages.write_through(id, data),
        }
    }

    /// How many bytes of pages the cache holds at most (SPEC §50); 0
    /// turns it off. Pages over the new size are forgotten.
    pub fn set_cache_size(&self, bytes: usize) {
        self.pages.set_cache_pages(bytes / PAGE_SIZE);
    }

    /// How many bytes of older versions are kept for open snapshots at
    /// most (SPEC §83).
    pub fn set_snapshot_memory(&self, bytes: usize) {
        self.pages.set_version_limit(bytes / PAGE_SIZE);
    }

    #[cfg(test)]
    pub(crate) fn cache_size(&self) -> usize {
        self.pages.memory().cache.capacity()
    }

    /// The whole file as it is on disk, read through the store's own
    /// handle: on Windows the file lock (§21.1) is mandatory, and a second
    /// handle can't read the file while this one holds it.
    #[cfg(test)]
    pub(crate) fn file_bytes(&self) -> Vec<u8> {
        let file = self.pages.file();
        let mut bytes = vec![0u8; file.metadata().unwrap().len() as usize];
        read_exact_at(file, &mut bytes, 0).unwrap();
        bytes
    }

    /// Hits and misses so far.
    #[cfg(test)]
    pub(crate) fn cache_stats(&self) -> (usize, usize) {
        let cache = &self.pages.memory().cache;
        let count =
            |n: &std::sync::atomic::AtomicUsize| n.load(std::sync::atomic::Ordering::Relaxed);
        (count(&cache.hits), count(&cache.misses))
    }

    #[cfg(test)]
    fn cached(&self, id: PageId) -> Option<Page> {
        self.pages.memory().cache.get(id)
    }

    fn check_bounds(&self, id: PageId) -> io::Result<()> {
        if id >= self.header.page_count {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page {id} out of bounds (page_count = {})",
                    self.header.page_count
                ),
            ));
        }
        Ok(())
    }

    /// Starts staging: from here until `write_back` or `rollback`, no page
    /// change reaches the file. Staging doesn't nest — calling this while
    /// already staging is a bug in the caller, so it panics rather than
    /// returning an error nobody could handle meaningfully.
    pub fn begin(&mut self) {
        assert!(
            self.staging.is_none(),
            "FileStore::begin while already staging"
        );
        self.staging = Some(Staging {
            header_before: self.header,
            dirty: BTreeMap::new(),
            before: BTreeMap::new(),
            replaces_all: false,
        });
    }

    /// Discards every change since `begin` — dirty pages and header alike.
    /// The file was never touched, so there's nothing to undo on disk.
    pub fn rollback(&mut self) {
        let staging = self
            .staging
            .take()
            .expect("FileStore::rollback without begin");
        self.header = staging.header_before;
    }

    /// Every page changed since `begin`, in ascending id order, header
    /// (page 0) included if it changed. Empty when not staging. This is
    /// what the WAL logs before `write_back` touches the file.
    pub fn dirty_pages(&self) -> impl Iterator<Item = (PageId, &[u8])> {
        self.staging
            .iter()
            .flat_map(|staging| staging.dirty.iter())
            .map(|(&id, page)| (id, &**page))
    }

    /// Makes `pages` — ids 1 and up, as a `MemoryStore` built them — the
    /// file's whole content, staged like any other change: the page count
    /// becomes `page_count`, the free list empty, and `write_back` cuts
    /// the file to that length (SPEC §41).
    pub(crate) fn replace_all(
        &mut self,
        pages: impl IntoIterator<Item = (PageId, impl Into<Page>)>,
        page_count: u64,
    ) -> io::Result<()> {
        let staging = self
            .staging
            .as_mut()
            .expect("FileStore::replace_all without begin");
        // Every page changes, and the file may get shorter: keeping each
        // as it was would be keeping the whole file in memory. So no
        // snapshot may be open when this commits (rule 6 of SPEC §57.3,
        // §80.5), and nothing needs keeping.
        staging.replaces_all = true;
        staging.before.clear();
        for (id, page) in pages {
            let page = page.into();
            assert!(
                id != HEADER_PAGE && id < page_count,
                "page {id} out of range"
            );
            if page.len() != USABLE_PAGE_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("page {id} is {} bytes, not {USABLE_PAGE_SIZE}", page.len()),
                ));
            }
            self.write_raw(id, page)?;
        }
        self.header.page_count = page_count;
        self.header.free_list_head = NO_FREE_PAGE;
        self.write_header()
    }

    /// Checks the staged pages' layout (SPEC §66), ahead of `commit`,
    /// which would check them where readers wait for it (SPEC §79).
    pub(crate) fn check_staged(&mut self) {
        if let Some(staging) = &mut self.staging {
            for page in staging.dirty.values_mut() {
                *page = page.clone().checked();
            }
        }
    }

    /// Ends staging for a batch the WAL now holds (SPEC §51): its pages
    /// become the newest committed ones (`Pages::commit`), read from
    /// memory until `checkpoint` writes them back. Nothing is written to
    /// the file. With no snapshot open: `commit_beside` otherwise.
    pub fn commit(&mut self) {
        self.commit_beside(&[]);
    }

    /// `commit`, with snapshots open at the commits `live`, ascending
    /// (SPEC §80): the pages as they were stay in memory for them.
    pub(crate) fn commit_beside(&mut self, live: &[u64]) {
        let staging = self
            .staging
            .take()
            .expect("FileStore::commit without begin");
        assert!(
            live.is_empty() || !staging.replaces_all,
            "a batch that replaces the whole file committed beside a snapshot"
        );
        self.pages.commit(staging.dirty, staging.before, live);
    }

    /// Whether the staged batch replaces the whole file (`replace_all`),
    /// and so may only commit with no snapshot open.
    pub(crate) fn replaces_all(&self) -> bool {
        self.staging
            .as_ref()
            .is_some_and(|staging| staging.replaces_all)
    }

    /// How many committed pages wait for `checkpoint`.
    pub fn unwritten_pages(&self) -> usize {
        self.pages.unwritten()
    }

    /// Writes every committed page not written back yet to the file,
    /// cuts it to the page count, and `fsync`s (`Pages::checkpoint`). On
    /// error they stay where they were, and a later call writes them all
    /// again. With no snapshot open: `checkpoint_beside` otherwise.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.checkpoint_beside(&[])
    }

    /// `checkpoint`, with snapshots open at the commits `live`, ascending
    /// (SPEC §80): the versions they read stay in memory.
    pub(crate) fn checkpoint_beside(&mut self, live: &[u64]) -> io::Result<()> {
        self.pages.checkpoint(self.header.page_count, live)
    }

    /// `commit`, then `checkpoint`: the batch in the file at once — for
    /// the fresh file's bootstrap, and tests.
    pub fn write_back(&mut self) -> io::Result<()> {
        self.commit();
        self.checkpoint()
    }

    /// Crash recovery: writes page images recovered from the WAL straight
    /// to the file, in the order given (so a page logged more than once
    /// ends at its latest image), then re-reads the header — its own image
    /// may have been among them — and `fsync`s. No bounds check: the batch
    /// that logged these pages may have grown the file past what the
    /// on-disk header says, and its header image is what makes them valid.
    pub fn restore_pages(&mut self, pages: &[PageImage]) -> io::Result<()> {
        assert!(
            self.staging.is_none(),
            "FileStore::restore_pages while staging"
        );
        // A page past the end of the file, as the header before it has
        // it, is damage (SPEC §55): a batch that grows the file logs its
        // header too, and first, as page 0. Checked before anything is
        // written, so a damaged WAL leaves the file as it was.
        let mut page_count = read_header(self.pages.file()).ok().map(|h| h.page_count);
        for (id, page) in pages {
            if *id == HEADER_PAGE {
                page_count = Some(Header::decode(page)?.page_count);
            } else if page_count.is_none_or(|count| *id >= count) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "the WAL holds page {id}, past the end of the file — it may be corrupt"
                    ),
                ));
            }
        }
        self.pages.restore(pages)?;
        self.header = read_header(self.pages.file())?;
        self.header_checked = true;
        // A crash can come between a shrinking batch's write-back and
        // its cut.
        self.pages.truncate(self.header.page_count)?;
        self.pages.sync()
    }
}

impl PageStore for FileStore {
    /// Takes the free list's first page, or grows the file. A page freed
    /// by a commit may come back from here at once — harmless while no
    /// reader overlaps a commit. Snapshot reads (SPEC §57, deferred)
    /// would need a freed page kept until every reader that might still
    /// read its old contents is done: rule 1 of §57.3, which a free-space
    /// map has to keep too.
    fn allocate_page(&mut self) -> io::Result<PageId> {
        let id = if self.header.free_list_head != NO_FREE_PAGE {
            let id = self.header.free_list_head;
            let buf = self.read_current(id)?;
            if buf[0] != PageType::Free as u8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "page {id} was on the free list but isn't tagged Free — file may be corrupt"
                    ),
                ));
            }
            let next = PageId::from_le_bytes(buf[1..9].try_into().unwrap());
            // The link is from the file: the header's own was checked at
            // open, each one after it is checked here (SPEC §55).
            if next >= self.header.page_count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "free page {id} links to page {next}, past the end — file may be corrupt"
                    ),
                ));
            }
            self.header.free_list_head = next;
            id
        } else {
            let id = self.header.page_count;
            self.header.page_count += 1;
            // Materialize the page so the file's length matches page_count
            // (or, while staging, so reads of it don't run past the file's
            // end); content is unspecified (see struct doc) until the
            // caller writes it.
            self.write_raw(id, Page::from(vec![0u8; USABLE_PAGE_SIZE]))?;
            id
        };
        self.write_header()?;
        Ok(id)
    }

    fn read_page(&self, id: PageId) -> io::Result<Page> {
        self.check_bounds(id)?;
        self.read_current(id)
    }

    fn try_read_page(&self, id: PageId) -> io::Result<Option<Page>> {
        if id >= self.header.page_count {
            return Ok(None);
        }
        self.read_current(id).map(Some)
    }

    fn write_page(&mut self, id: PageId, data: Page) -> io::Result<()> {
        if id == HEADER_PAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the header page is managed internally, not through write_page",
            ));
        }
        self.check_bounds(id)?;
        if data.len() != USABLE_PAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page data must be exactly {USABLE_PAGE_SIZE} bytes, got {}",
                    data.len()
                ),
            ));
        }
        // A page written by this build may use what an older format lacks
        // (a document without its `_id`, SPEC §59): the header says so in
        // the same batch, and a rollback takes both back.
        if self.header.format_version != FORMAT_VERSION {
            self.write_header()?;
        }
        self.write_raw(id, data)
    }

    /// Puts `id` on the free list, where the next allocation takes it
    /// (see `allocate_page`, and SPEC §57.3 before changing when).
    fn free_page(&mut self, id: PageId) -> io::Result<()> {
        if id == HEADER_PAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot free the header page",
            ));
        }
        self.check_bounds(id)?;

        let mut buf = [0u8; USABLE_PAGE_SIZE];
        buf[0] = PageType::Free as u8;
        buf[1..9].copy_from_slice(&self.header.free_list_head.to_le_bytes());
        self.write_raw(id, Page::from(&buf[..]))?;

        self.header.free_list_head = id;
        self.write_header()
    }
}

/// The pages on the free list that starts at `header`'s head, in list
/// order, each read through `read`. An error if the list runs past the
/// file's end, visits a page that isn't tagged free, or loops — each
/// would make allocation hand out a page twice.
pub(super) fn free_list(
    header: &Header,
    read: impl Fn(PageId) -> io::Result<Page>,
) -> io::Result<Vec<PageId>> {
    let corrupt =
        |what: String| io::Error::new(io::ErrorKind::InvalidData, format!("free list: {what}"));
    let mut pages = Vec::new();
    let mut next = header.free_list_head;
    while next != NO_FREE_PAGE {
        if next >= header.page_count {
            return Err(corrupt(format!("page {next} is past the end")));
        }
        if pages.len() as u64 >= header.page_count {
            return Err(corrupt("it loops".to_string()));
        }
        let buf = read(next)?;
        if buf[0] != PageType::Free as u8 {
            return Err(corrupt(format!("page {next} isn't tagged free")));
        }
        pages.push(next);
        next = PageId::from_le_bytes(buf[1..9].try_into().unwrap());
    }
    Ok(pages)
}

/// The header, from the file. Magic and format version come before the
/// checksum: a file of another format may not have one where this
/// format's goes, and saying "format 5, export it" beats "page 0 is
/// damaged".
fn read_header(file: &File) -> io::Result<Header> {
    let disk = read_disk_page(file, HEADER_PAGE)?;
    let header = Header::decode(&disk[..USABLE_PAGE_SIZE])?;
    if !checksum_matches(HEADER_PAGE, &disk) {
        return Err(damaged(HEADER_PAGE));
    }
    Ok(header)
}

/// Rewrites a closed file's format version, with the header's checksum
/// to match: a file from an older build, for tests above this module.
#[cfg(test)]
pub(crate) fn rewrite_format_version(path: &Path, version: u32) {
    use super::pages::{read_page_at, write_page_at};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut header = read_page_at(&file, HEADER_PAGE).unwrap();
    header.make_mut()[28..32].copy_from_slice(&version.to_le_bytes());
    write_page_at(&file, HEADER_PAGE, &header).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc32::Crc32;
    use crate::storage::pages::{write_all_at, write_page_at};

    fn open_temp() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.trunkdb");
        (dir, path)
    }

    #[test]
    fn fresh_file_has_only_the_header_page() {
        let (_dir, path) = open_temp();
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.header.page_count, 1);
    }

    #[test]
    fn allocate_grows_sequentially_when_nothing_is_free() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        assert_eq!(store.allocate_page().unwrap(), 1);
        assert_eq!(store.allocate_page().unwrap(), 2);
        assert_eq!(store.allocate_page().unwrap(), 3);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let id = store.allocate_page().unwrap();

        let mut data = vec![0u8; USABLE_PAGE_SIZE];
        data[0..5].copy_from_slice(b"hello");
        store.write_page(id, data.clone().into()).unwrap();

        let read_back = store.read_page(id).unwrap();
        assert_eq!(&read_back[0..5], b"hello");
    }

    #[test]
    fn free_then_allocate_reuses_the_page() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let a = store.allocate_page().unwrap();
        let _b = store.allocate_page().unwrap();

        store.free_page(a).unwrap();
        let reused = store.allocate_page().unwrap();

        assert_eq!(
            reused, a,
            "freeing then allocating should reuse the freed page"
        );
    }

    #[test]
    fn state_survives_close_and_reopen() {
        let (_dir, path) = open_temp();
        {
            let mut store = FileStore::open(&path).unwrap();
            let id = store.allocate_page().unwrap();
            let mut data = vec![0u8; USABLE_PAGE_SIZE];
            data[0] = 42;
            store.write_page(id, data.clone().into()).unwrap();
            let extra = store.allocate_page().unwrap();
            store.free_page(extra).unwrap();
        } // store dropped, file closed

        let mut store = FileStore::open(&path).unwrap();
        assert_eq!(store.header.page_count, 3, "header + 2 allocated pages");
        assert_eq!(store.read_page(1).unwrap()[0], 42);

        // The freed page should still be reusable after reopening.
        assert_eq!(store.allocate_page().unwrap(), 2);
    }

    #[test]
    fn read_or_write_out_of_bounds_fails() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        assert!(store.read_page(99).is_err());
        assert!(
            store
                .write_page(99, vec![0u8; USABLE_PAGE_SIZE].into())
                .is_err()
        );
    }

    #[test]
    fn try_read_page_returns_none_for_never_allocated_page() {
        let (_dir, path) = open_temp();
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.try_read_page(99).unwrap(), None);
    }

    #[test]
    fn try_read_page_returns_some_for_allocated_page() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let id = store.allocate_page().unwrap();
        store
            .write_page(id, vec![7u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        assert_eq!(
            store.try_read_page(id).unwrap().unwrap(),
            vec![7u8; USABLE_PAGE_SIZE]
        );
    }

    #[test]
    fn write_page_rejects_wrong_size() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let id = store.allocate_page().unwrap();
        assert!(store.write_page(id, vec![0u8; 10].into()).is_err());
    }

    #[test]
    fn header_page_is_protected() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        assert!(
            store
                .write_page(HEADER_PAGE, vec![0u8; USABLE_PAGE_SIZE].into())
                .is_err()
        );
        assert!(store.free_page(HEADER_PAGE).is_err());
    }

    // ---------- lock and format version ----------

    #[test]
    fn a_second_open_fails_while_the_first_holds_the_lock() {
        let (_dir, path) = open_temp();
        let first = FileStore::open(&path).unwrap();

        let err = FileStore::open(&path).err().expect("must not open twice");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        drop(first);
        FileStore::open(&path).unwrap();
    }

    /// Writes a file of these pages, as `FileStore` would: each with its
    /// checksum.
    fn write_file(path: &Path, pages: &[(PageId, &[u8])]) {
        let file = File::create(path).unwrap();
        for (id, page) in pages {
            write_page_at(&file, *id, page).unwrap();
        }
    }

    /// Writes a one-page file whose header is valid except for its
    /// format version.
    fn file_with_format_version(path: &Path, version: u32) {
        let mut header = Header {
            page_size: PAGE_SIZE as u32,
            page_count: 1,
            free_list_head: NO_FREE_PAGE,
            format_version: FORMAT_VERSION,
        }
        .encode();
        header[28..32].copy_from_slice(&version.to_le_bytes());
        write_file(path, &[(HEADER_PAGE, &header)]);
    }

    #[test]
    fn the_format_version_survives_reopen() {
        let (_dir, path) = open_temp();
        {
            let mut store = FileStore::open(&path).unwrap();
            store.allocate_page().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[28..32], FORMAT_VERSION.to_le_bytes());
        FileStore::open(&path).unwrap();
    }

    #[test]
    fn a_compatible_older_format_opens_and_is_stamped_on_the_next_allocation() {
        let (_dir, path) = open_temp();
        let version = |path: &Path| std::fs::read(path).unwrap()[28..32].to_vec();
        for older in COMPATIBLE_OLDER_FORMATS {
            file_with_format_version(&path, older);
            {
                let store = FileStore::open(&path).unwrap();
                drop(store);
            }
            assert_eq!(
                version(&path),
                older.to_le_bytes(),
                "opening alone keeps it"
            );

            let mut store = FileStore::open(&path).unwrap();
            store.allocate_page().unwrap();
            drop(store);
            assert_eq!(version(&path), FORMAT_VERSION.to_le_bytes());
        }
    }

    #[test]
    fn other_format_versions_are_rejected_with_a_clear_error() {
        let (_dir, path) = open_temp();
        for (version, expected) in [
            (0, "before format versioning"),
            (3, "older than this build"),
            (4, "older than this build"),
            (5, "older than this build"),
            (FORMAT_VERSION + 1, "newer than this build"),
        ] {
            file_with_format_version(&path, version);
            let err = FileStore::open(&path).err().expect("must be rejected");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(
                err.to_string().contains(expected),
                "format {version}: {err}"
            );
        }
    }

    #[test]
    fn a_short_file_that_is_not_trunkdb_is_rejected_untouched() {
        let (_dir, path) = open_temp();
        let content = b"someone's notes, not a database";
        std::fs::write(&path, content).unwrap();

        let err = FileStore::open(&path).err().expect("must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }

    /// Down to a cut inside the magic itself: a first write-back cut short
    /// is still recognized as fresh (its batch is in the WAL).
    #[test]
    fn a_short_file_starting_with_the_magic_is_fresh() {
        let (_dir, path) = open_temp();
        for len in [3, MAGIC.len(), 100] {
            let mut header = Header {
                page_size: PAGE_SIZE as u32,
                page_count: 2,
                free_list_head: NO_FREE_PAGE,
                format_version: FORMAT_VERSION,
            }
            .encode()
            .to_vec();
            header.truncate(len);
            std::fs::write(&path, header).unwrap();
            let store = FileStore::open(&path).unwrap();
            assert_eq!(store.header.page_count, 1, "cut at {len} bytes: fresh");
        }
    }

    // ---------- checksums ----------

    /// A closed file with pages 1 and 2 written, `[1; …]` and `[2; …]`.
    /// SPEC §59: an older format's file is stamped with the current one
    /// by the first page a batch writes, allocation or not, and a batch
    /// rolled back takes the stamp back with it.
    #[test]
    fn an_older_format_is_stamped_by_the_first_page_written() {
        let (_dir, path) = open_temp();
        let mut header = Header {
            page_size: PAGE_SIZE as u32,
            page_count: 2,
            free_list_head: NO_FREE_PAGE,
            format_version: FORMAT_VERSION,
        }
        .encode();
        header[28..32].copy_from_slice(&8u32.to_le_bytes());
        write_file(
            &path,
            &[(HEADER_PAGE, &header), (1, &[1; USABLE_PAGE_SIZE])],
        );

        let mut store = FileStore::open(&path).unwrap();
        assert_eq!(store.format_version().unwrap(), 8);
        store.begin();
        store
            .write_page(1, vec![2; USABLE_PAGE_SIZE].into())
            .unwrap();
        assert_eq!(store.format_version().unwrap(), FORMAT_VERSION);
        // Remembered, so the batch's next page doesn't write it again.
        assert_eq!(store.header.format_version, FORMAT_VERSION);
        store.rollback();
        assert_eq!(store.format_version().unwrap(), 8, "rolled back with it");

        store.begin();
        store
            .write_page(1, vec![3; USABLE_PAGE_SIZE].into())
            .unwrap();
        store.write_back().unwrap();
        drop(store);
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.format_version().unwrap(), FORMAT_VERSION);
        assert_eq!(store.read_page(1).unwrap(), vec![3; USABLE_PAGE_SIZE]);
    }

    fn two_page_file(path: &Path) {
        let mut store = FileStore::open(path).unwrap();
        for fill in [1u8, 2] {
            let id = store.allocate_page().unwrap();
            store
                .write_page(id, vec![fill; USABLE_PAGE_SIZE].into())
                .unwrap();
        }
    }

    fn change_file(path: &Path, change: impl FnOnce(&mut Vec<u8>)) {
        let mut bytes = std::fs::read(path).unwrap();
        change(&mut bytes);
        std::fs::write(path, bytes).unwrap();
    }

    fn assert_damaged(result: io::Result<Page>, id: PageId) {
        let err = result.expect_err("a damaged page must not be read");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains(&format!("page {id} is damaged")),
            "{err}"
        );
    }

    #[test]
    fn every_page_ends_with_the_checksum_of_its_id_and_bytes() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 3 * PAGE_SIZE);
        for (id, page) in bytes.chunks(PAGE_SIZE).enumerate() {
            let mut crc = Crc32::new();
            crc.update(&(id as u64).to_le_bytes());
            crc.update(&page[..USABLE_PAGE_SIZE]);
            assert_eq!(
                page[USABLE_PAGE_SIZE..],
                crc.finish().to_le_bytes(),
                "page {id}"
            );
        }
    }

    /// A changed bit anywhere in the page — its bytes or its checksum —
    /// makes that page, and only that page, unreadable.
    #[test]
    fn a_changed_bit_makes_the_page_unreadable() {
        let (_dir, path) = open_temp();
        for at in [0, 1, USABLE_PAGE_SIZE - 1, USABLE_PAGE_SIZE, PAGE_SIZE - 1] {
            two_page_file(&path);
            change_file(&path, |bytes| bytes[PAGE_SIZE + at] ^= 0x10);
            let store = FileStore::open(&path).unwrap();
            assert_damaged(store.read_page(1), 1);
            assert_damaged(store.try_read_page(1).map(Option::unwrap), 1);
            assert_eq!(store.read_page(2).unwrap(), vec![2u8; USABLE_PAGE_SIZE]);
            assert_eq!(store.damaged_pages().unwrap(), vec![1], "byte {at}");
        }
    }

    /// The id is summed too: a page that is intact but in the wrong place
    /// — a misdirected write — doesn't pass for the page it replaced.
    #[test]
    fn a_page_copied_onto_another_is_damaged() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        change_file(&path, |bytes| {
            let page_1 = bytes[PAGE_SIZE..2 * PAGE_SIZE].to_vec();
            bytes[2 * PAGE_SIZE..].copy_from_slice(&page_1);
        });
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.read_page(1).unwrap(), vec![1u8; USABLE_PAGE_SIZE]);
        assert_damaged(store.read_page(2), 2);
    }

    /// What an OS can leave in a file a crash cut short: zeros. Their
    /// checksum isn't zero.
    #[test]
    fn a_zeroed_page_is_damaged() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        change_file(&path, |bytes| bytes[2 * PAGE_SIZE..].fill(0));
        assert_damaged(FileStore::open(&path).unwrap().read_page(2), 2);
    }

    #[test]
    fn a_damaged_header_fails_the_open() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        change_file(&path, |bytes| bytes[100] = 1);
        let err = FileStore::open(&path).err().expect("must not open");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("page 0 is damaged"), "{err}");
    }

    /// The header's own checks come before its checksum: an older file's
    /// header has none, and "format 5, export it" is what helps.
    #[test]
    fn an_older_format_is_named_before_the_missing_checksum() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        change_file(&path, |bytes| {
            bytes[28..32].copy_from_slice(&5u32.to_le_bytes())
        });
        let err = FileStore::open(&path).err().expect("must not open");
        assert!(err.to_string().contains("has format 5"), "{err}");
    }

    /// Staged pages are checked when they reach the file, not before:
    /// `damaged_pages` reads the file, so a batch's pages don't count.
    #[test]
    fn restored_pages_get_their_checksum() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        let mut store = FileStore::open(&path).unwrap();
        store.begin();
        store
            .write_page(2, vec![9u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        let pages: Vec<PageImage> = store
            .dirty_pages()
            .map(|(id, page)| (id, page.to_vec()))
            .collect();
        store.rollback();
        store.restore_pages(&pages).unwrap();
        assert_eq!(store.damaged_pages().unwrap(), Vec::<PageId>::new());
        assert_eq!(store.read_page(2).unwrap(), vec![9u8; USABLE_PAGE_SIZE]);
    }

    /// Found by fuzzing (SPEC §55): a header whose page count is 0 or
    /// overflows an offset, or whose free list starts past the end, is
    /// refused at open.
    #[test]
    fn a_header_that_cannot_be_right_is_refused() {
        for (page_count, free_list_head, says) in [
            (0, NO_FREE_PAGE, "counts 0 pages"),
            (u64::MAX / 2, NO_FREE_PAGE, "pages — file may be corrupt"),
            (3, 3, "free list starts at page 3"),
        ] {
            let (_dir, path) = open_temp();
            let header = Header {
                page_size: PAGE_SIZE as u32,
                page_count,
                free_list_head,
                format_version: FORMAT_VERSION,
            }
            .encode();
            write_file(&path, &[(HEADER_PAGE, &header)]);
            let Err(err) = FileStore::open(&path) else {
                panic!("{page_count} pages, free list at {free_list_head}: opened");
            };
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains(says), "{err}");
        }
    }

    /// Found by fuzzing (SPEC §55): a free page linking past the end is
    /// an error when allocation follows the link, not a read there.
    #[test]
    fn a_free_list_link_past_the_end_is_an_error() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        let mut store = FileStore::open(&path).unwrap();
        store.free_page(2).unwrap();
        let mut free = store.read_page(2).unwrap().to_vec();
        free[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        store.write_raw(2, free.into()).unwrap();

        let err = store.allocate_page().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("links to page"), "{err}");
    }

    /// A WAL page past the end of the file, as the header before it in
    /// the WAL (or the file's own) has it, is refused before anything is
    /// written; one the WAL's header makes room for is restored.
    #[test]
    fn restored_pages_must_lie_inside_the_file() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        let mut store = FileStore::open(&path).unwrap();
        let before = store.file_bytes();
        let page = vec![7u8; USABLE_PAGE_SIZE];
        let header = |page_count| {
            let header = Header {
                page_size: PAGE_SIZE as u32,
                page_count,
                free_list_head: NO_FREE_PAGE,
                format_version: FORMAT_VERSION,
            };
            header.encode().to_vec()
        };

        let past = store.restore_pages(&[(3, page.clone())]).unwrap_err();
        assert!(past.to_string().contains("page 3, past the end"), "{past}");
        let shrunk = [(HEADER_PAGE, header(2)), (2, page.clone())];
        assert!(
            store.restore_pages(&shrunk).is_err(),
            "the header shrank the file first"
        );
        assert!(store.restore_pages(&[(u64::MAX, page.clone())]).is_err());
        assert_eq!(store.file_bytes(), before, "nothing written");

        store
            .restore_pages(&[(HEADER_PAGE, header(5)), (4, page.clone())])
            .unwrap();
        assert_eq!(store.read_page(4).unwrap(), page);
    }

    // ---------- replacing the whole content ----------

    /// Pages 1–5 written, 4 freed: the file before a compaction.
    fn five_page_file(path: &Path) -> FileStore {
        let mut store = FileStore::open(path).unwrap();
        for fill in 1..=5u8 {
            let id = store.allocate_page().unwrap();
            store
                .write_page(id, vec![fill; USABLE_PAGE_SIZE].into())
                .unwrap();
        }
        store.free_page(4).unwrap();
        store
    }

    #[test]
    fn replace_all_makes_the_pages_the_whole_file() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.begin();
        let pages = [
            (1, vec![8u8; USABLE_PAGE_SIZE]),
            (2, vec![9u8; USABLE_PAGE_SIZE]),
        ];
        store.replace_all(pages, 3).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            6 * PAGE_SIZE as u64
        );
        store.write_back().unwrap();
        drop(store);

        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            3 * PAGE_SIZE as u64
        );
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.page_count(), 3);
        assert_eq!(store.free_pages().unwrap(), Vec::<PageId>::new());
        assert_eq!(store.read_page(2).unwrap(), vec![9u8; USABLE_PAGE_SIZE]);
        assert_eq!(store.damaged_pages().unwrap(), Vec::<PageId>::new());
    }

    #[test]
    fn a_rolled_back_replace_all_changes_nothing() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.begin();
        store
            .replace_all([(1, vec![8u8; USABLE_PAGE_SIZE])], 2)
            .unwrap();
        store.rollback();
        assert_eq!(store.page_count(), 6);
        assert_eq!(store.free_pages().unwrap(), vec![4]);
        assert_eq!(store.read_page(1).unwrap(), vec![1u8; USABLE_PAGE_SIZE]);
    }

    /// A crash between a shrinking batch's write-back and its cut leaves
    /// the file too long; restoring the batch from the WAL cuts it — here
    /// by exactly one page.
    #[test]
    fn restoring_pages_cuts_the_file_to_its_page_count() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.begin();
        let pages = (1..5).map(|id| (id, vec![8u8; USABLE_PAGE_SIZE]));
        store.replace_all(pages, 5).unwrap();
        let pages: Vec<PageImage> = store
            .dirty_pages()
            .map(|(id, page)| (id, page.to_vec()))
            .collect();
        store.rollback();
        store.restore_pages(&pages).unwrap();
        drop(store);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, 5 * PAGE_SIZE as u64);
        assert_eq!(FileStore::open(&path).unwrap().page_count(), 5);
    }

    // ---------- staging ----------

    /// What a second, independent reader of the file sees — i.e. what's
    /// actually on disk, bypassing `store`'s dirty set. Reads through a
    /// clone of `store`'s own handle, which shares its lock: on Windows
    /// the lock is mandatory, so a separately opened handle couldn't read
    /// the locked file at all.
    fn on_disk(store: &FileStore) -> FileStore {
        FileStore::from_file(store.pages.file().try_clone().unwrap(), false).unwrap()
    }

    #[test]
    fn staged_writes_are_visible_to_reads_but_not_on_disk() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();

        store.begin();
        let id = store.allocate_page().unwrap();
        store
            .write_page(id, vec![7u8; USABLE_PAGE_SIZE].into())
            .unwrap();

        assert_eq!(store.read_page(id).unwrap(), vec![7u8; USABLE_PAGE_SIZE]);
        let disk = on_disk(&store);
        assert_eq!(
            disk.header.page_count, 1,
            "allocation must not reach the header on disk"
        );
        assert_eq!(disk.try_read_page(id).unwrap(), None);
    }

    #[test]
    fn write_back_persists_staged_pages_and_header() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();

        store.begin();
        let id = store.allocate_page().unwrap();
        store
            .write_page(id, vec![9u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        store.write_back().unwrap();

        assert_eq!(store.dirty_pages().count(), 0, "write_back ends staging");
        let disk = on_disk(&store);
        assert_eq!(disk.header.page_count, 2);
        assert_eq!(disk.read_page(id).unwrap(), vec![9u8; USABLE_PAGE_SIZE]);
    }

    #[test]
    fn rollback_restores_page_count_free_list_and_page_contents() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let a = store.allocate_page().unwrap();
        store
            .write_page(a, vec![1u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        let b = store.allocate_page().unwrap();
        store.free_page(b).unwrap(); // free list: b

        store.begin();
        assert_eq!(
            store.allocate_page().unwrap(),
            b,
            "pops b off the free list"
        );
        let c = store.allocate_page().unwrap(); // grows the file
        store
            .write_page(a, vec![2u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        store.free_page(a).unwrap();
        store.rollback();

        assert_eq!(store.header.page_count, 3, "c's growth undone");
        assert_eq!(store.read_page(a).unwrap(), vec![1u8; USABLE_PAGE_SIZE]);
        assert!(store.try_read_page(c).unwrap().is_none());
        assert_eq!(
            store.allocate_page().unwrap(),
            b,
            "free list is back to just b"
        );
        assert_eq!(
            store.allocate_page().unwrap(),
            c,
            "and c is handed out fresh again"
        );
    }

    /// `allocate_page` reads the popped page's next-free link — which, for
    /// a page freed earlier in the same batch, exists only in the dirty set.
    #[test]
    fn a_page_freed_while_staging_can_be_reallocated_in_the_same_batch() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let a = store.allocate_page().unwrap();
        let b = store.allocate_page().unwrap();
        store
            .write_page(a, vec![1u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        store
            .write_page(b, vec![1u8; USABLE_PAGE_SIZE].into())
            .unwrap();

        store.begin();
        store.free_page(a).unwrap();
        store.free_page(b).unwrap(); // free list: b -> a
        assert_eq!(store.allocate_page().unwrap(), b);
        assert_eq!(store.allocate_page().unwrap(), a);
        store.write_back().unwrap();

        assert_eq!(on_disk(&store).header.free_list_head, NO_FREE_PAGE);
    }

    #[test]
    fn dirty_pages_holds_each_page_once_and_includes_the_header() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let a = store.allocate_page().unwrap();

        store.begin();
        assert_eq!(store.dirty_pages().count(), 0);
        store
            .write_page(a, vec![1u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        store
            .write_page(a, vec![2u8; USABLE_PAGE_SIZE].into())
            .unwrap();
        let b = store.allocate_page().unwrap();

        let dirty: Vec<(PageId, Vec<u8>)> = store
            .dirty_pages()
            .map(|(id, page)| (id, page.to_vec()))
            .collect();
        let ids: Vec<PageId> = dirty.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![HEADER_PAGE, a, b],
            "ascending, one entry per page"
        );
        assert_eq!(
            dirty[1].1,
            vec![2u8; USABLE_PAGE_SIZE],
            "the last write wins"
        );
        assert_eq!(Header::decode(&dirty[0].1).unwrap().page_count, 3);
    }

    #[test]
    fn the_header_page_stays_protected_while_staging() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        store.begin();
        assert!(
            store
                .write_page(HEADER_PAGE, vec![0u8; USABLE_PAGE_SIZE].into())
                .is_err()
        );
        assert!(store.free_page(HEADER_PAGE).is_err());
    }

    #[test]
    #[should_panic(expected = "already staging")]
    fn begin_does_not_nest() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        store.begin();
        store.begin();
    }

    fn filled(fill: u8) -> Vec<u8> {
        vec![fill; USABLE_PAGE_SIZE]
    }

    /// Hits and misses since `before`.
    fn since(store: &FileStore, before: (usize, usize)) -> (usize, usize) {
        let now = store.cache_stats();
        (now.0 - before.0, now.1 - before.1)
    }

    /// A page is read from the file once, then from the cache (SPEC §50).
    #[test]
    fn a_page_read_twice_comes_from_the_file_once() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        let store = FileStore::open(&path).unwrap();
        let before = store.cache_stats();
        for id in [1, 1, 2, 1] {
            assert_eq!(store.read_page(id).unwrap(), filled(id as u8));
        }
        assert_eq!(since(&store, before), (2, 2));
    }

    #[test]
    fn a_cache_of_size_zero_reads_the_file_every_time() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        let store = FileStore::open(&path).unwrap();
        store.set_cache_size(0);
        let before = store.cache_stats();
        store.read_page(1).unwrap();
        store.read_page(1).unwrap();
        assert_eq!(since(&store, before), (0, 2));
    }

    /// Pages are shared, not copied (SPEC §64): two reads of a cached
    /// page get the same bytes in memory, and so do the reads before and
    /// after a checkpoint moves a committed page into the cache. Changing
    /// a page read changes no one else's, and a reader holding a page
    /// keeps it as it was when a write replaces it.
    #[test]
    fn pages_are_shared_until_one_is_changed() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let held = store.read_page(1).unwrap();
        assert!(store.read_page(1).unwrap().shares(&held));

        let mut changed = store.read_page(1).unwrap();
        changed.make_mut()[0] = 42;
        assert_eq!(store.read_page(1).unwrap(), filled(1));

        store.begin();
        store.write_page(1, filled(9).into()).unwrap();
        let staged = store.read_page(1).unwrap();
        store.commit();
        assert!(store.read_page(1).unwrap().shares(&staged));
        store.checkpoint().unwrap();
        let cached = store.read_page(1).unwrap();
        assert!(cached.shares(&staged), "moved into the cache, not copied");
        assert_eq!(cached, filled(9));
        assert_eq!(held, filled(1));
    }

    /// A slotted page is checked when its batch commits or it comes in
    /// from the file, and every read of it after carries the mark (SPEC
    /// §66); a staged page and one that isn't slotted don't.
    #[test]
    fn cached_pages_come_with_their_layout_checked() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let slotted = super::super::SlottedPage::new(PageType::Data).into_page();
        store.begin();
        store.write_page(2, slotted.clone()).unwrap();
        assert!(!store.read_page(2).unwrap().layout_checked(), "staged");
        store.commit();
        assert!(store.read_page(2).unwrap().layout_checked(), "committed");
        store.checkpoint().unwrap();
        assert!(store.read_page(2).unwrap().layout_checked(), "cached");
        assert!(!store.read_page(1).unwrap().layout_checked(), "not slotted");
        store.write_page(3, slotted.clone()).unwrap();
        assert!(store.read_page(3).unwrap().layout_checked(), "unstaged");
        drop(store);
        let store = FileStore::open(&path).unwrap();
        assert!(
            store.read_page(2).unwrap().layout_checked(),
            "from the file"
        );
    }

    /// `write_page` keeps the page it's given (SPEC §67), in a batch or
    /// outside one, and so does the checkpoint that caches it: a page
    /// changed once isn't copied again.
    #[test]
    fn a_written_page_is_kept_not_copied() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let staged: Page = filled(9).into();
        store.begin();
        store.write_page(1, staged.clone()).unwrap();
        assert!(store.read_page(1).unwrap().shares(&staged));
        store.write_back().unwrap();
        assert!(store.read_page(1).unwrap().shares(&staged));
        let unstaged: Page = filled(8).into();
        store.write_page(2, unstaged.clone()).unwrap();
        assert!(store.read_page(2).unwrap().shares(&unstaged));
    }

    /// Readers share the cache (SPEC §65): while one holds it to look a
    /// page up, another's read of a cached page goes through, where a
    /// mutex would make it wait.
    #[test]
    fn readers_look_pages_up_together() {
        let (_dir, path) = open_temp();
        let store = five_page_file(&path);
        store.read_page(1).unwrap();
        let held = store.pages.memory();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| sent.send(store.read_page(1).unwrap()).unwrap());
            let page = received.recv_timeout(std::time::Duration::from_secs(10));
            drop(held);
            assert_eq!(page.expect("the second reader waited"), filled(1));
        });
    }

    /// Staged pages never reach the cache: a rolled-back write isn't read
    /// back, a written-back one is, from the cache.
    #[test]
    fn the_cache_holds_the_file_never_a_staged_page() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        assert_eq!(store.read_page(1).unwrap(), filled(1));
        store.begin();
        store.write_page(1, filled(9).into()).unwrap();
        assert_eq!(store.read_page(1).unwrap(), filled(9));
        store.rollback();
        assert_eq!(store.read_page(1).unwrap(), filled(1));

        store.begin();
        store.write_page(1, filled(8).into()).unwrap();
        store.write_back().unwrap();
        let before = store.cache_stats();
        assert_eq!(store.read_page(1).unwrap(), filled(8));
        assert_eq!(since(&store, before), (1, 0));
        assert_eq!(on_disk(&store).read_page(1).unwrap(), filled(8));
    }

    /// A checkpoint that failed halfway left some pages new in the file
    /// and some old: reads still find every committed page, from memory,
    /// and a checkpoint that works puts all of them in the file (SPEC
    /// §51).
    #[test]
    fn a_failed_checkpoint_keeps_its_pages_until_one_works() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        for id in 1..=3 {
            store.read_page(id).unwrap();
        }
        store.begin();
        for id in 1..=3 {
            store.write_page(id, filled(7).into()).unwrap();
        }
        store.commit();
        store.pages.fail_write_backs(1, 1);
        assert!(store.checkpoint().is_err());
        let disk = on_disk(&store);
        assert_eq!(disk.read_page(1).unwrap(), filled(7));
        assert_eq!(disk.read_page(2).unwrap(), filled(2), "not written yet");
        for id in 1..=3 {
            assert_eq!(store.read_page(id).unwrap(), filled(7), "{id}");
        }
        assert_eq!(store.unwritten_pages(), 3);
        store.checkpoint().unwrap();
        assert_eq!(store.unwritten_pages(), 0);
        let disk = on_disk(&store);
        for id in 1..=3 {
            assert_eq!(disk.read_page(id).unwrap(), filled(7), "{id}");
            assert_eq!(store.cached(id).unwrap(), filled(7), "{id}");
        }
    }

    /// Committed pages are read from memory, not the file, until the
    /// checkpoint — including pages past the file's end.
    #[test]
    fn committed_pages_are_read_before_the_checkpoint_writes_them() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.begin();
        store.write_page(2, filled(9).into()).unwrap();
        let new = store.allocate_page().unwrap();
        store.write_page(new, filled(6).into()).unwrap();
        store.commit();
        // A second batch, with a page past the file's end.
        store.begin();
        let beyond = store.allocate_page().unwrap();
        store.write_page(beyond, filled(5).into()).unwrap();
        store.commit();
        let disk = on_disk(&store);
        assert_eq!(disk.read_page(2).unwrap(), filled(2));
        assert_ne!(
            disk.read_page(new).unwrap(),
            filled(6),
            "the freed page, as it was"
        );
        assert_eq!(disk.try_read_page(beyond).unwrap(), None);
        assert_eq!(store.read_page(beyond).unwrap(), filled(5));
        assert_eq!(store.read_page(2).unwrap(), filled(9));
        assert_eq!(store.read_page(new).unwrap(), filled(6));
        assert_eq!(store.damaged_pages().unwrap(), Vec::<PageId>::new());
        store.checkpoint().unwrap();
        assert_eq!(on_disk(&store).read_page(new).unwrap(), filled(6));
        assert_eq!(on_disk(&store).read_page(beyond).unwrap(), filled(5));
    }

    #[test]
    fn restored_pages_replace_what_the_cache_held() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        assert_eq!(store.read_page(2).unwrap(), filled(2));
        store.restore_pages(&[(2, filled(6))]).unwrap();
        assert_eq!(store.read_page(2).unwrap(), filled(6));
    }

    /// Pages cut off the end of the file leave the cache too.
    #[test]
    fn pages_cut_off_leave_the_cache() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.read_page(5).unwrap();
        assert!(store.cached(5).is_some());
        store.begin();
        store.replace_all([(1, filled(8))], 2).unwrap();
        store.write_back().unwrap();
        assert!(store.cached(5).is_none());
        assert_eq!(store.cached(1).unwrap(), filled(8));

        // Committed by one batch, cut off by the next, before either is
        // written back: the checkpoint doesn't cache it either.
        store.begin();
        store
            .replace_all([(1, filled(1)), (4, filled(4))], 5)
            .unwrap();
        store.commit();
        store.begin();
        store.replace_all([(1, filled(7))], 2).unwrap();
        store.commit();
        store.checkpoint().unwrap();
        assert!(store.cached(4).is_none());
        assert_eq!(store.cached(1).unwrap(), filled(7));
    }

    /// The cache answers for a page that was damaged on disk after it was
    /// read — the scan `check` runs still finds it, since it reads the
    /// file itself (SPEC §50.3).
    #[test]
    fn a_page_damaged_after_it_was_cached_is_still_found_by_the_scan() {
        let (_dir, path) = open_temp();
        two_page_file(&path);
        let store = FileStore::open(&path).unwrap();
        assert_eq!(store.read_page(1).unwrap(), filled(1));
        // Through the store's own handle: Windows' file lock is
        // mandatory, so another handle couldn't write while it's held.
        let mut disk = read_disk_page(store.pages.file(), 1).unwrap();
        disk[100] ^= 1;
        write_all_at(store.pages.file(), &disk, PAGE_SIZE as u64).unwrap();
        assert_eq!(store.read_page(1).unwrap(), filled(1));
        assert_eq!(store.damaged_pages().unwrap(), [1]);
        drop(store);
        assert_damaged(FileStore::open(&path).unwrap().read_page(1), 1);
    }

    // --- The committed pages apart from the writer's (SPEC §77) ---

    /// A staged batch is the writer's alone: the store reads its own
    /// changes, the committed pages show none of them, the header's page
    /// included, until `commit`.
    #[test]
    fn a_staged_batch_is_not_among_the_committed_pages() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let header_before = store.pages.read(HEADER_PAGE).unwrap();

        store.begin();
        store.write_page(2, filled(9).into()).unwrap();
        let new = store.allocate_page().unwrap();
        assert_eq!(new, 4, "the free page");
        store.write_page(new, filled(8).into()).unwrap();

        assert_eq!(store.read_page(2).unwrap(), filled(9));
        assert_eq!(store.read_page(new).unwrap(), filled(8));
        assert_eq!(store.pages.read(2).unwrap(), filled(2));
        assert_eq!(store.pages.read(new).unwrap()[0], PageType::Free as u8);
        assert_eq!(store.pages.read(HEADER_PAGE).unwrap(), header_before);
        assert_eq!(store.pages.unwritten(), 0);

        store.commit();
        assert_eq!(store.pages.read(2).unwrap(), filled(9));
        assert_eq!(store.pages.read(new).unwrap(), filled(8));
        assert_ne!(store.pages.read(HEADER_PAGE).unwrap(), header_before);
    }

    /// A batch rolled back leaves the committed pages as they were, and
    /// takes no commit number.
    #[test]
    fn a_rollback_leaves_the_committed_pages_alone() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.begin();
        store.write_page(1, filled(7).into()).unwrap();
        store.commit();
        let seq = store.pages.seq();

        store.begin();
        store.write_page(1, filled(9).into()).unwrap();
        store.free_page(2).unwrap();
        store.rollback();

        assert_eq!(store.pages.seq(), seq);
        assert_eq!(store.pages.unwritten(), 1);
        assert_eq!(store.pages.version_seq(1), Some(seq));
        assert_eq!(store.pages.read(1).unwrap(), filled(7));
        assert_eq!(store.pages.read(2).unwrap(), filled(2));
    }

    /// Commits are numbered from 1, in memory; a page waiting for a
    /// checkpoint carries the number of the last commit that changed it,
    /// and a checkpoint doesn't start the count again.
    #[test]
    fn a_committed_page_carries_its_commits_number() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        assert_eq!(store.pages.seq(), 0);

        store.begin();
        store.write_page(1, filled(7).into()).unwrap();
        store.write_page(2, filled(7).into()).unwrap();
        store.commit();
        assert_eq!(store.pages.seq(), 1);

        store.begin();
        store.write_page(2, filled(8).into()).unwrap();
        store.commit();
        assert_eq!(store.pages.seq(), 2);
        assert_eq!(store.pages.version_seq(1), Some(1));
        assert_eq!(store.pages.version_seq(2), Some(2));
        assert_eq!(store.pages.version_seq(3), None, "never changed");
        assert_eq!(store.pages.read(2).unwrap(), filled(8));

        store.checkpoint().unwrap();
        assert_eq!(store.pages.version_seq(2), None, "in the file now");
        assert_eq!(store.pages.read(2).unwrap(), filled(8));
        store.begin();
        store.write_page(3, filled(9).into()).unwrap();
        store.commit();
        assert_eq!(store.pages.seq(), 3);
        assert_eq!(store.pages.version_seq(3), Some(3));

        // In memory only: a new open counts from 0 again.
        drop(store);
        assert_eq!(FileStore::open(&path).unwrap().pages.seq(), 0);
    }

    // --- Reading as of a commit (SPEC §78) ---

    /// A snapshot store reads its commit's pages, page count and free
    /// list, and none of a batch being staged.
    #[test]
    fn a_snapshot_reads_its_commit_and_no_staged_batch() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let commit = store.last_commit();

        store.begin();
        store.write_page(2, filled(9).into()).unwrap();
        assert_eq!(store.allocate_page().unwrap(), 4, "the free page");
        let grown = store.allocate_page().unwrap();
        store.write_page(grown, filled(8).into()).unwrap();

        let at = store.at(commit);
        assert_eq!(at.read_page(2).unwrap(), filled(2));
        assert_eq!(at.page_count(), 6);
        assert_eq!(at.free_pages().unwrap(), [4]);
        assert_eq!(at.try_read_page(grown).unwrap(), None);
        assert_eq!(
            at.read_page(grown).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(at.format_version().unwrap(), FORMAT_VERSION);
        // The writer sees its own batch.
        assert_eq!(store.read_page(2).unwrap(), filled(9));
        assert_eq!(store.page_count(), 7);
        assert_eq!(store.free_pages().unwrap(), Vec::<PageId>::new());

        store.commit();
        let at = store.at(store.last_commit());
        assert_eq!(at.read_page(2).unwrap(), filled(9));
        assert_eq!(at.read_page(grown).unwrap(), filled(8));
        assert_eq!(at.page_count(), 7);
        assert_eq!(at.free_pages().unwrap(), Vec::<PageId>::new());
    }

    /// A page waiting for a checkpoint is read by the snapshot of the
    /// commit that wrote it and of every later one. For an earlier one
    /// its older version is gone, and the read says so instead of
    /// handing out the newer page: until §80 keeps it.
    #[test]
    fn a_snapshot_reads_no_page_from_a_later_commit() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let commit_page = |store: &mut FileStore, id: PageId, fill: u8| {
            store.begin();
            store.write_page(id, filled(fill).into()).unwrap();
            store.commit();
            store.last_commit()
        };
        let first = commit_page(&mut store, 1, 7);
        let second = commit_page(&mut store, 2, 8);
        assert_eq!((first.seq(), second.seq()), (1, 2));

        // Page 1: written by the first commit, read by both.
        assert_eq!(store.at(first).read_page(1).unwrap(), filled(7));
        assert_eq!(store.at(second).read_page(1).unwrap(), filled(7));
        // Page 3: written by neither, the file's.
        assert_eq!(store.at(first).read_page(3).unwrap(), filled(3));
        // Page 2: written by the second.
        assert_eq!(store.at(second).read_page(2).unwrap(), filled(8));
        let err = store.at(first).read_page(2).unwrap_err();
        assert!(err.to_string().contains("snapshot too old"), "{err}");
        let err = store.at(first).try_read_page(2).unwrap_err();
        assert!(err.to_string().contains("snapshot too old"), "{err}");
    }

    #[test]
    fn a_snapshot_cannot_write() {
        let (_dir, path) = open_temp();
        let store = five_page_file(&path);
        let mut at = store.at(store.last_commit());
        let denied = |result: io::Result<()>| {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        };
        denied(at.allocate_page().map(|_| ()));
        denied(at.write_page(1, filled(9).into()));
        denied(at.free_page(1));
        assert_eq!(store.read_page(1).unwrap(), filled(1));
        assert_eq!(store.free_pages().unwrap(), [4]);
    }

    // --- Older versions, kept for the snapshots open (SPEC §80) ---

    /// Stages a write of `fill` to each of `ids` and commits it beside
    /// the snapshots open at `live`.
    fn commit_beside(store: &mut FileStore, ids: &[PageId], fill: u8, live: &[u64]) -> Commit {
        store.begin();
        for &id in ids {
            store.write_page(id, filled(fill).into()).unwrap();
        }
        store.commit_beside(live);
        store.last_commit()
    }

    /// A snapshot open at a commit goes on reading that commit: through
    /// later commits of the same page, through the checkpoint that puts
    /// the newest into the file, and from the cache's page being
    /// replaced. A page that had no version in memory keeps the one the
    /// batch found.
    #[test]
    fn an_open_snapshot_reads_its_commit_through_commits_and_checkpoints() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let open = store.last_commit();
        assert_eq!(open.seq(), 0);

        let first = commit_beside(&mut store, &[1, 2], 7, &[0]);
        assert_eq!(store.at(open).read_page(1).unwrap(), filled(1));
        assert_eq!(store.at(open).read_page(2).unwrap(), filled(2));
        assert_eq!(store.at(first).read_page(1).unwrap(), filled(7));
        assert_eq!(store.pages.version_seqs(1), [0, 1]);

        let second = commit_beside(&mut store, &[2, 3], 8, &[0]);
        assert_eq!(store.at(open).read_page(2).unwrap(), filled(2));
        assert_eq!(store.at(open).read_page(3).unwrap(), filled(3));
        assert_eq!(store.at(second).read_page(2).unwrap(), filled(8));
        // Nobody is open at the first commit: its version of page 2 went.
        assert_eq!(store.pages.version_seqs(2), [0, 2]);

        store.checkpoint_beside(&[0]).unwrap();
        assert_eq!(store.unwritten_pages(), 0);
        let disk = on_disk(&store);
        assert_eq!(
            disk.read_page(1).unwrap(),
            filled(7),
            "the newest, in the file"
        );
        assert_eq!(disk.read_page(2).unwrap(), filled(8));
        for (id, fill) in [(1, 1), (2, 2), (3, 3), (5, 5)] {
            assert_eq!(store.at(open).read_page(id).unwrap(), filled(fill), "{id}");
        }
        assert_eq!(store.at(second).read_page(2).unwrap(), filled(8));
        assert_eq!(store.pages.version_seqs(2), [0, 2], "kept, though written");

        // And a commit after the checkpoint, of a page read from the
        // file again.
        let third = commit_beside(&mut store, &[5, 1], 9, &[0]);
        assert_eq!(store.at(open).read_page(5).unwrap(), filled(5));
        assert_eq!(store.at(open).read_page(1).unwrap(), filled(1));
        assert_eq!(store.at(third).read_page(1).unwrap(), filled(9));
        assert_eq!(store.pages.version_seqs(1), [0, 3]);
        assert_eq!(store.unwritten_pages(), 2, "pages 1 and 5");
    }

    /// Once no snapshot is open, the next checkpoint or commit leaves
    /// nothing older in memory: only what waits for a checkpoint, as
    /// before §80.
    #[test]
    fn versions_go_when_no_snapshot_reads_them() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        commit_beside(&mut store, &[1, 2, 3], 7, &[0]);
        commit_beside(&mut store, &[1, 2], 8, &[0, 1]);
        store.checkpoint_beside(&[0, 1]).unwrap();
        assert_eq!(store.pages.version_seqs(1), [0, 1, 2]);
        assert_eq!(store.pages.version_seqs(3), [0, 1]);
        let held = store.pages.versions_held();

        // The one at 0 closed: its versions go, the others stay.
        store.checkpoint_beside(&[1]).unwrap();
        assert_eq!(store.pages.version_seqs(1), [1, 2]);
        assert_eq!(store.pages.version_seqs(3), Vec::<u64>::new(), "the file's");
        assert!(store.pages.versions_held() < held);

        // All closed: by a commit, the pages it changes; by a
        // checkpoint, every page.
        commit_beside(&mut store, &[1], 9, &[]);
        assert_eq!(store.pages.version_seqs(1), [3]);
        assert_eq!(store.pages.version_seqs(2), [1, 2], "not this commit's");
        store.checkpoint().unwrap();
        assert_eq!(store.pages.versions_held(), 0);
        for (id, fill) in [(1, 9), (2, 8), (3, 7)] {
            assert_eq!(store.read_page(id).unwrap(), filled(fill));
        }
    }

    /// One snapshot beside many commits of the same pages costs one
    /// older version of each, not one per commit.
    #[test]
    fn a_hot_page_keeps_two_versions_beside_one_snapshot() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        for round in 1..=50u64 {
            commit_beside(&mut store, &[1, 2], round as u8, &[0]);
            if round % 7 == 0 {
                store.checkpoint_beside(&[0]).unwrap();
            }
        }
        assert_eq!(store.pages.version_seqs(1), [0, 50]);
        assert_eq!(store.pages.versions_held(), 4);
        assert_eq!(
            store.at(Commit::new(0, store.header)).read_page(1).unwrap(),
            filled(1)
        );
    }

    /// Without a snapshot open, a commit keeps nothing older: as many
    /// versions in memory as pages waiting.
    #[test]
    fn a_commit_keeps_nothing_older_with_no_snapshot_open() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        for round in 1..=5 {
            commit_beside(&mut store, &[1, 2, 3], round, &[]);
            assert_eq!(store.pages.versions_held(), 3);
            assert_eq!(store.unwritten_pages(), 3);
        }
        assert_eq!(store.pages.version_seqs(1), [5]);
    }

    /// Rule 1 of §57.3, with no tag on the freed page: a page freed and
    /// taken again for something else is, to an open snapshot, the page
    /// it was, and the free list as it was.
    #[test]
    fn a_freed_page_taken_again_is_still_the_old_one_to_an_open_snapshot() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        let open = store.last_commit();

        store.begin();
        store.free_page(2).unwrap();
        store.commit_beside(&[0]);
        store.begin();
        assert_eq!(store.allocate_page().unwrap(), 2, "taken again at once");
        store.write_page(2, filled(9).into()).unwrap();
        store.commit_beside(&[0]);
        store.checkpoint_beside(&[0]).unwrap();

        let at = store.at(open);
        assert_eq!(at.read_page(2).unwrap(), filled(2));
        assert_eq!(at.free_pages().unwrap(), [4]);
        assert_eq!(store.read_page(2).unwrap(), filled(9));
        assert_eq!(store.free_pages().unwrap(), [4]);
    }

    /// A reader that found a page in neither the versions nor the cache
    /// goes to the file. If the page is committed and written back just
    /// then, what the file holds is too new for it, or half written: it
    /// sees that a checkpoint began, looks again, and finds its version.
    /// And it doesn't leave its page in the cache for others.
    #[test]
    fn a_page_written_back_under_a_readers_file_read_is_read_again() {
        let (_dir, path) = open_temp();
        drop(five_page_file(&path));
        let store = Arc::new(std::sync::Mutex::new(FileStore::open(&path).unwrap()));
        let (pages, open) = {
            let store = store.lock().unwrap();
            (store.pages.clone(), store.last_commit())
        };

        let writer = store.clone();
        pages.before_next_file_read(move || {
            // Another thread, as the writer is: the reader holds no lock
            // here.
            let written = std::thread::spawn(move || {
                let mut store = writer.lock().unwrap();
                commit_beside(&mut store, &[3], 9, &[0]);
                store.checkpoint_beside(&[0]).unwrap();
            });
            written.join().unwrap();
        });
        assert_eq!(pages.at(open).read_page(3).unwrap(), filled(3));

        let store = store.lock().unwrap();
        assert_eq!(on_disk(&store).read_page(3).unwrap(), filled(9));
        assert_eq!(store.read_page(3).unwrap(), filled(9));
        assert_eq!(store.cached(3).unwrap(), filled(9));
        assert_eq!(store.pages.at(open).read_page(3).unwrap(), filled(3));
    }

    /// The same, for the scan of damaged pages: a page written back
    /// under it isn't called damaged, nor skipped.
    #[test]
    fn a_checkpoint_beside_the_damaged_scan_is_no_damage() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        for round in 0..20 {
            commit_beside(&mut store, &[1, 2, 3, 5], round, &[]);
            let pages = store.pages.clone();
            std::thread::scope(|scope| {
                let scan = scope.spawn(move || pages.damaged(6).unwrap());
                store.checkpoint().unwrap();
                assert_eq!(scan.join().unwrap(), Vec::<PageId>::new(), "{round}");
            });
        }
    }

    #[test]
    #[should_panic(expected = "replaces the whole file committed beside a snapshot")]
    fn replacing_the_file_beside_a_snapshot_is_refused() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.begin();
        store.replace_all([(1, filled(8))], 2).unwrap();
        assert!(store.replaces_all());
        store.commit_beside(&[0]);
    }

    // --- A limit on the versions kept (SPEC §83) ---

    /// Whether a read as of `commit` is refused as too old.
    fn too_old(store: &FileStore, commit: Commit, id: PageId) -> bool {
        match store.at(commit).read_page(id) {
            Ok(_) => false,
            Err(e) => {
                assert!(e.get_ref().unwrap().is::<crate::storage::TooOld>(), "{e}");
                true
            }
        }
    }

    /// Past the limit the oldest open snapshot is ended: its versions go,
    /// and every read as of it is refused from then on, also of a page
    /// nobody changed, also after a checkpoint has put newer pages in the
    /// file — never handed one of those. A newer snapshot reads on. The
    /// writer isn't refused anything.
    #[test]
    fn past_the_limit_the_oldest_snapshot_is_ended() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        for fill in 1..=20u8 {
            let id = store.allocate_page().unwrap();
            store.write_page(id, filled(fill).into()).unwrap();
        }
        store.pages.set_version_limit(6);
        let oldest = store.last_commit();
        let kept = |store: &FileStore| store.pages.versions_kept();

        // Three pages, then the same three again: three older versions.
        commit_beside(&mut store, &[1, 2, 3], 31, &[0]);
        let newer = commit_beside(&mut store, &[1, 2, 3], 32, &[0]);
        assert_eq!(kept(&store), (3, 6, 0));
        // Two more pages, as both snapshots have them: five.
        commit_beside(&mut store, &[4, 5], 33, &[0, 2]);
        assert_eq!(kept(&store), (5, 6, 0));
        assert!(!too_old(&store, oldest, 1));
        assert_eq!(store.at(newer).read_page(1).unwrap(), filled(32));

        // The first three again, and a sixth: now pages 1 to 3 are kept
        // twice, as each snapshot has them. Nine, past the limit. The
        // one at 0 is ended, and what was kept for it alone goes: six
        // pages as the one at 2 has them, which is within it.
        commit_beside(&mut store, &[1, 2, 3, 6], 34, &[0, 2]);
        assert_eq!(kept(&store), (6, 6, 1));
        assert_eq!(store.pages.ended_before(), 1);
        for id in [1, 4, 6, 20] {
            assert!(too_old(&store, oldest, id), "page {id}");
        }
        assert_eq!(store.at(newer).read_page(1).unwrap(), filled(32));
        assert_eq!(store.at(newer).read_page(4).unwrap(), filled(4));
        assert_eq!(store.at(newer).read_page(6).unwrap(), filled(6));

        // Still refused once the file has the newest, and the header.
        store.checkpoint_beside(&[0, 2]).unwrap();
        assert!(too_old(&store, oldest, 1) && too_old(&store, oldest, 20));
        assert!(
            matches!(store.at(oldest).try_read_page(1), Err(e) if e.get_ref().unwrap().is::<crate::storage::TooOld>())
        );
        assert!(
            store.at(oldest).format_version().is_err(),
            "the header page too"
        );
        assert_eq!(store.at(newer).read_page(1).unwrap(), filled(32));

        // Then the next oldest, when its turn comes.
        let last = commit_beside(&mut store, &[7, 8, 9, 10, 11, 12], 35, &[0, 2]);
        assert_eq!(kept(&store).2, 2);
        assert!(too_old(&store, newer, 1) && too_old(&store, newer, 20));
        assert_eq!(kept(&store).0, 0, "nobody left to keep anything for");
        assert_eq!(store.at(last).read_page(7).unwrap(), filled(35));
        assert_eq!(
            store.read_page(1).unwrap(),
            filled(34),
            "the writer reads on"
        );
    }

    /// Within the limit nobody is ended, however many commits.
    #[test]
    fn within_the_limit_no_snapshot_is_ended() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.pages.set_version_limit(3);
        let open = store.last_commit();
        for round in 1..=30 {
            commit_beside(&mut store, &[1, 2, 3], round, &[0]);
        }
        assert_eq!(store.pages.versions_kept(), (3, 3, 0));
        assert_eq!(store.at(open).read_page(2).unwrap(), filled(2));
    }

    /// With a limit of nothing, a snapshot ends with the first page
    /// changed beside it; one nobody writes beside reads on.
    #[test]
    fn a_limit_of_nothing_ends_a_snapshot_at_the_first_change() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.pages.set_version_limit(0);
        let open = store.last_commit();
        assert_eq!(store.at(open).read_page(1).unwrap(), filled(1));
        let next = commit_beside(&mut store, &[1], 7, &[0]);
        assert!(too_old(&store, open, 1) && too_old(&store, open, 2));
        assert_eq!(store.pages.versions_kept(), (0, 0, 1));
        assert_eq!(store.at(next).read_page(1).unwrap(), filled(7));
    }
}
