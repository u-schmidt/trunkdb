use super::cache::PageCache;
use super::{PageId, PageImage, PageStore, PageType};
use crate::crc32::Crc32;
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Mutex;

/// A page's size in the file. Matches LiteDB's page size, partly so
/// numbers stay comparable to it later, and because a document DB's
/// records (whole JSON-ish objects) benefit more from fewer, larger pages
/// than a fixed-page-size OS-I/O alignment argument for 4096 would buy
/// back.
pub const PAGE_SIZE: usize = 8192;
/// Each page's last bytes: a CRC-32 of the page's id and the rest of its
/// bytes (SPEC §40).
const CHECKSUM_LEN: usize = 4;
/// A page's size above this file: what `read_page` returns and
/// `write_page` takes — `PAGE_SIZE` without the checksum, which only this
/// file ever sees. The WAL logs pages of this size too.
pub const USABLE_PAGE_SIZE: usize = PAGE_SIZE - CHECKSUM_LEN;
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
/// cell kinds (SPEC §43).
const FORMAT_VERSION: u32 = 8;
/// Older formats this build opens as they are, because such a file *is*
/// a valid `FORMAT_VERSION` file — one that uses none of what came since
/// (for 4: unique indexes). Every header write stamps `FORMAT_VERSION`,
/// and creating anything newer allocates a page, which writes the header
/// in the same batch — so a file that uses something newer always says
/// so, and an older build refuses it instead of misreading it (SPEC §33.4).
/// 4 and 5 aren't: every page of theirs lacks the checksum (6), and uses
/// the bytes where it now goes.
const COMPATIBLE_OLDER_FORMATS: [u32; 2] = [6, 7];
const HEADER_PAGE: PageId = 0;
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
struct Header {
    page_size: u32,
    page_count: u64,
    free_list_head: PageId,
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
    file: File,
    header: Header,
    /// Checked pages as they are in the file (SPEC §50). Behind a mutex
    /// because reads take `&self` and run in parallel (§27).
    cache: Mutex<PageCache>,
    /// Pages of committed batches — durable in the WAL — not yet written
    /// back to the file (SPEC §51): the newest image of each. Reads look
    /// here before the cache and the file; `checkpoint` writes them back.
    unwritten: BTreeMap<PageId, Vec<u8>>,
    /// Whether the header's checksum has been checked — not yet between
    /// `open_before_recovery` and `check_header` (SPEC §40.4).
    header_checked: bool,
    staging: Option<Staging>,
    /// Test-only fault injection: while non-zero, each `write_back` call
    /// writes `write_back_fails_after` pages, then fails (and decrements
    /// this) — leaving the file genuinely half-written, like a crash or a
    /// full disk would. Pages go out in ascending id order.
    #[cfg(test)]
    pub(crate) failing_write_backs: u32,
    #[cfg(test)]
    pub(crate) write_back_fails_after: usize,
}

struct Staging {
    /// The header as of `begin`, restored by `rollback`.
    header_before: Header,
    /// Every page changed since `begin`, keyed by id — so a page written
    /// many times in one batch is held (and later logged) once. Includes
    /// the header, as page 0, once anything has changed it.
    dirty: BTreeMap<PageId, Vec<u8>>,
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
            }
        } else {
            Header::decode(&read_disk_page(&file, HEADER_PAGE)?[..USABLE_PAGE_SIZE])?
        };

        Ok(Self {
            file,
            header,
            cache: Mutex::new(PageCache::new(DEFAULT_CACHE_SIZE / PAGE_SIZE)),
            unwritten: BTreeMap::new(),
            // A fresh file's header isn't on disk yet: nothing to check.
            header_checked: is_fresh,
            staging: None,
            #[cfg(test)]
            failing_write_backs: 0,
            #[cfg(test)]
            write_back_fails_after: 1,
        })
    }

    /// Checks the header's checksum, if `open_before_recovery` left it
    /// unchecked.
    pub(crate) fn check_header(&mut self) -> io::Result<()> {
        if !self.header_checked {
            if !checksum_matches(HEADER_PAGE, &read_disk_page(&self.file, HEADER_PAGE)?) {
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
        let mut buf = [0u8; USABLE_PAGE_SIZE];
        self.read_raw(HEADER_PAGE, &mut buf)?;
        Ok(u32::from_le_bytes(buf[28..32].try_into().unwrap()))
    }

    /// The pages on the free list, in list order. An error if the list
    /// runs past the file's end, visits a page that isn't tagged free, or
    /// loops — each would make allocation hand out a page twice.
    pub(crate) fn free_pages(&self) -> io::Result<Vec<PageId>> {
        let corrupt =
            |what: String| io::Error::new(io::ErrorKind::InvalidData, format!("free list: {what}"));
        let mut pages = Vec::new();
        let mut next = self.header.free_list_head;
        while next != NO_FREE_PAGE {
            if next >= self.header.page_count {
                return Err(corrupt(format!("page {next} is past the end")));
            }
            if pages.len() as u64 >= self.header.page_count {
                return Err(corrupt("it loops".to_string()));
            }
            let mut buf = [0u8; USABLE_PAGE_SIZE];
            self.read_raw(next, &mut buf)?;
            if buf[0] != PageType::Free as u8 {
                return Err(corrupt(format!("page {next} isn't tagged free")));
            }
            pages.push(next);
            next = PageId::from_le_bytes(buf[1..9].try_into().unwrap());
        }
        Ok(pages)
    }

    /// The pages whose checksum doesn't match their bytes on disk, in id
    /// order — what a disk error or a change from outside trunkdb leaves.
    /// Reads the file itself, past any staged changes. Pages not written
    /// back yet are skipped: the WAL holds them, and the file's copy is
    /// older or missing (SPEC §51).
    pub(crate) fn damaged_pages(&self) -> io::Result<Vec<PageId>> {
        let mut damaged = Vec::new();
        for id in 0..self.header.page_count {
            if self.unwritten.contains_key(&id) {
                continue;
            }
            if !checksum_matches(id, &read_disk_page(&self.file, id)?) {
                damaged.push(id);
            }
        }
        Ok(damaged)
    }

    fn write_header(&mut self) -> io::Result<()> {
        let header = self.header.encode();
        self.write_raw(HEADER_PAGE, &header)
    }

    /// Reads a page's current bytes: from the dirty set if it's there,
    /// otherwise from the cache, otherwise from the file — and keeps it in
    /// the cache. No bounds check — callers do that.
    fn read_raw(&self, id: PageId, buf: &mut [u8]) -> io::Result<()> {
        if let Some(staging) = &self.staging
            && let Some(page) = staging.dirty.get(&id)
        {
            buf.copy_from_slice(page);
            return Ok(());
        }
        if let Some(page) = self.unwritten.get(&id) {
            buf.copy_from_slice(page);
            return Ok(());
        }
        if self.cache().read(id, buf) {
            return Ok(());
        }
        read_page_at(&self.file, id, buf)?;
        self.cache().put(id, buf);
        Ok(())
    }

    /// `read_raw` into a new `Vec`, copied straight from wherever the page
    /// is — no zeroed buffer first; a lookup reads several pages, and
    /// clearing 8 KB each time showed in the profile (SPEC §50.2).
    fn read_vec(&self, id: PageId) -> io::Result<Vec<u8>> {
        if let Some(staging) = &self.staging
            && let Some(page) = staging.dirty.get(&id)
        {
            return Ok(page.clone());
        }
        if let Some(page) = self.unwritten.get(&id) {
            return Ok(page.clone());
        }
        if let Some(page) = self.cache().get(id) {
            return Ok(page.to_vec());
        }
        let disk = read_disk_page(&self.file, id)?;
        if !checksum_matches(id, &disk) {
            return Err(damaged(id));
        }
        let page = &disk[..USABLE_PAGE_SIZE];
        self.cache().put(id, page);
        Ok(page.to_vec())
    }

    /// Writes a page: into the dirty set while staging, otherwise
    /// straight to the file — and the cache. No bounds check — callers do
    /// that.
    fn write_raw(&mut self, id: PageId, data: &[u8]) -> io::Result<()> {
        match &mut self.staging {
            Some(staging) => {
                staging.dirty.insert(id, data.to_vec());
                Ok(())
            }
            None => {
                self.cache().put(id, data);
                write_page_at(&self.file, id, data)
            }
        }
    }

    /// The page cache. A panic while it was held can't have left it
    /// half-changed in a way that matters — at worst a page is missing —
    /// so a poisoned lock is taken over, not passed on.
    fn cache(&self) -> std::sync::MutexGuard<'_, PageCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many bytes of pages the cache holds at most (SPEC §50); 0
    /// turns it off. Pages over the new size are forgotten.
    pub fn set_cache_size(&mut self, bytes: usize) {
        self.cache().resize(bytes / PAGE_SIZE);
    }

    #[cfg(test)]
    pub(crate) fn cache_size(&self) -> usize {
        self.cache().capacity()
    }

    /// Hits and misses so far.
    #[cfg(test)]
    pub(crate) fn cache_stats(&self) -> (usize, usize) {
        let cache = self.cache();
        (cache.hits, cache.misses)
    }

    #[cfg(test)]
    fn cached(&self, id: PageId) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; USABLE_PAGE_SIZE];
        self.cache().read(id, &mut buf).then_some(buf)
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

    /// Flushes every page write since the last call all the way to durable
    /// storage. `write_page`/`allocate_page`/`free_page` alone only
    /// guarantee the OS has the bytes (survives a process crash, not a
    /// power-loss/OS crash) — callers doing something that needs to survive
    /// that (see `durability::WalDurability`) call this once after a batch
    /// of writes, not after each individual one; a single `fsync` flushes
    /// every dirty page for this file regardless of how many separate
    /// writes produced them.
    pub fn sync(&self) -> io::Result<()> {
        super::sync(&self.file)
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
            .map(|(&id, page)| (id, page.as_slice()))
    }

    /// Makes `pages` — ids 1 and up, as a `MemoryStore` built them — the
    /// file's whole content, staged like any other change: the page count
    /// becomes `page_count`, the free list empty, and `write_back` cuts
    /// the file to that length (SPEC §41).
    pub(crate) fn replace_all(
        &mut self,
        pages: impl IntoIterator<Item = (PageId, Vec<u8>)>,
        page_count: u64,
    ) -> io::Result<()> {
        assert!(
            self.staging.is_some(),
            "FileStore::replace_all without begin"
        );
        for (id, page) in pages {
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
            self.write_raw(id, &page)?;
        }
        self.header.page_count = page_count;
        self.header.free_list_head = NO_FREE_PAGE;
        self.write_header()
    }

    /// Cuts the file to the header's page count, if it's longer — after
    /// a batch that shrank it (SPEC §41). Nothing past the page count is
    /// ever read, so the cut loses nothing.
    fn truncate_to_page_count(&self) -> io::Result<()> {
        let len = self.header.page_count * PAGE_SIZE as u64;
        if self.file.metadata()?.len() > len {
            self.file.set_len(len)?;
            let page_count = self.header.page_count;
            self.cache().retain(|id| id < page_count);
        }
        Ok(())
    }

    /// Ends staging for a batch the WAL now holds (SPEC §51): its pages
    /// become the newest committed ones, read from memory until
    /// `checkpoint` writes them back. Nothing is written to the file.
    pub fn commit(&mut self) {
        let staging = self
            .staging
            .take()
            .expect("FileStore::commit without begin");
        self.unwritten.extend(staging.dirty);
    }

    /// How many committed pages wait for `checkpoint`.
    pub fn unwritten_pages(&self) -> usize {
        self.unwritten.len()
    }

    /// Writes every committed page not written back yet to the file,
    /// cuts it to the page count, and `fsync`s; then they're read from
    /// the cache like any other page. On error they stay where they were,
    /// and reads still find them there — the file may now be half
    /// written, but none of the pages it's half-written with is read from
    /// it. A later call writes them all again.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        if self.unwritten.is_empty() {
            return Ok(());
        }
        self.write_unwritten_pages()?;
        self.truncate_to_page_count()?;
        super::sync(&self.file)?;
        let written = std::mem::take(&mut self.unwritten);
        let page_count = self.header.page_count;
        let mut cache = self.cache();
        // Not pages a later batch cut off the end: they're gone.
        for (id, page) in written.range(..page_count) {
            cache.put(*id, page);
        }
        Ok(())
    }

    /// `commit`, then `checkpoint`: the batch in the file at once — for
    /// the fresh file's bootstrap, and tests.
    pub fn write_back(&mut self) -> io::Result<()> {
        self.commit();
        self.checkpoint()
    }

    /// `checkpoint`'s writes: every unwritten page, in ascending id order.
    fn write_unwritten_pages(&mut self) -> io::Result<()> {
        #[cfg(test)]
        let mut written = 0;
        // The counter only exists in test builds, so `enumerate` would be
        // an unused index everywhere else.
        #[allow(clippy::explicit_counter_loop)]
        for (&id, page) in &self.unwritten {
            #[cfg(test)]
            {
                if self.failing_write_backs > 0 && written == self.write_back_fails_after {
                    self.failing_write_backs -= 1;
                    return Err(io::Error::other("injected write-back failure"));
                }
                written += 1;
            }
            write_page_at(&self.file, id, page)?;
        }
        Ok(())
    }

    /// Crash recovery: writes page images recovered from the WAL straight
    /// to the file, in the order given (so a page logged more than once
    /// ends at its latest image), then re-reads the header — its own image
    /// may have been among them — and `fsync`s. No bounds check: the batch
    /// that logged these pages may have grown the file past what the
    /// on-disk header says, and its header image is what makes them valid.
    pub fn restore_pages(&mut self, pages: &[PageImage]) -> io::Result<()> {
        assert!(
            self.staging.is_none() && self.unwritten.is_empty(),
            "FileStore::restore_pages while staging, or with pages to write back"
        );
        // A page past the end of the file, as the header before it has
        // it, is damage (SPEC §55): a batch that grows the file logs its
        // header too, and first, as page 0. Checked before anything is
        // written, so a damaged WAL leaves the file as it was.
        let mut page_count = read_header(&self.file).ok().map(|h| h.page_count);
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
        // Pages are about to change under it.
        self.cache().clear();
        for (id, page) in pages {
            write_page_at(&self.file, *id, page)?;
        }
        self.header = read_header(&self.file)?;
        self.header_checked = true;
        // A crash can come between a shrinking batch's write-back and
        // its cut.
        self.truncate_to_page_count()?;
        super::sync(&self.file)
    }
}

impl PageStore for FileStore {
    fn allocate_page(&mut self) -> io::Result<PageId> {
        let id = if self.header.free_list_head != NO_FREE_PAGE {
            let id = self.header.free_list_head;
            let mut buf = [0u8; USABLE_PAGE_SIZE];
            self.read_raw(id, &mut buf)?;
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
            self.write_raw(id, &[0u8; USABLE_PAGE_SIZE])?;
            id
        };
        self.write_header()?;
        Ok(id)
    }

    fn read_page(&self, id: PageId) -> io::Result<Vec<u8>> {
        self.check_bounds(id)?;
        self.read_vec(id)
    }

    fn try_read_page(&self, id: PageId) -> io::Result<Option<Vec<u8>>> {
        if id >= self.header.page_count {
            return Ok(None);
        }
        self.read_vec(id).map(Some)
    }

    fn write_page(&mut self, id: PageId, data: &[u8]) -> io::Result<()> {
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
        self.write_raw(id, data)
    }

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
        self.write_raw(id, &buf)?;

        self.header.free_list_head = id;
        self.write_header()
    }
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

/// The page's id is part of what's summed, so a page written to the
/// wrong place, or copied onto another, fails the check too.
pub(crate) fn checksum(id: PageId, usable: &[u8]) -> [u8; CHECKSUM_LEN] {
    let mut crc = Crc32::new();
    crc.update(&id.to_le_bytes());
    crc.update(usable);
    crc.finish().to_le_bytes()
}

fn checksum_matches(id: PageId, disk: &[u8; PAGE_SIZE]) -> bool {
    let (usable, stored) = disk.split_at(USABLE_PAGE_SIZE);
    checksum(id, usable) == stored
}

fn damaged(id: PageId) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "page {id} is damaged: its checksum doesn't match its bytes \
             (a disk error, or a change from outside trunkdb)"
        ),
    )
}

/// A page's bytes above this file, checksum verified and cut off.
fn read_page_at(file: &File, id: PageId, buf: &mut [u8]) -> io::Result<()> {
    let disk = read_disk_page(file, id)?;
    if !checksum_matches(id, &disk) {
        return Err(damaged(id));
    }
    buf.copy_from_slice(&disk[..USABLE_PAGE_SIZE]);
    Ok(())
}

/// Writes a page's bytes with their checksum appended.
fn write_page_at(file: &File, id: PageId, data: &[u8]) -> io::Result<()> {
    let mut disk = [0u8; PAGE_SIZE];
    disk[..USABLE_PAGE_SIZE].copy_from_slice(data);
    disk[USABLE_PAGE_SIZE..].copy_from_slice(&checksum(id, data));
    write_all_at(file, &disk, id * PAGE_SIZE as u64)
}

/// A page as it is in the file, checksum included, unchecked.
fn read_disk_page(file: &File, id: PageId) -> io::Result<[u8; PAGE_SIZE]> {
    let mut disk = [0u8; PAGE_SIZE];
    read_exact_at(file, &mut disk, id * PAGE_SIZE as u64)?;
    Ok(disk)
}

// Positional I/O (pread/pwrite-style) so reads only ever need `&File` — no
// shared mutable seek cursor to coordinate, matching `PageStore::read_page`
// taking `&self`.
#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0;
    while read < buf.len() {
        let n = file.seek_read(&mut buf[read..], offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF reading the file",
            ));
        }
        read += n;
        offset += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, data: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0;
    while written < data.len() {
        let n = file.seek_write(&data[written..], offset)?;
        written += n;
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        store.write_page(id, &data).unwrap();

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
            store.write_page(id, &data).unwrap();
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
        assert!(store.write_page(99, &[0u8; USABLE_PAGE_SIZE]).is_err());
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
        store.write_page(id, &[7u8; USABLE_PAGE_SIZE]).unwrap();
        assert_eq!(
            store.try_read_page(id).unwrap(),
            Some(vec![7u8; USABLE_PAGE_SIZE])
        );
    }

    #[test]
    fn write_page_rejects_wrong_size() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let id = store.allocate_page().unwrap();
        assert!(store.write_page(id, &[0u8; 10]).is_err());
    }

    #[test]
    fn header_page_is_protected() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        assert!(
            store
                .write_page(HEADER_PAGE, &[0u8; USABLE_PAGE_SIZE])
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
    fn two_page_file(path: &Path) {
        let mut store = FileStore::open(path).unwrap();
        for fill in [1u8, 2] {
            let id = store.allocate_page().unwrap();
            store.write_page(id, &[fill; USABLE_PAGE_SIZE]).unwrap();
        }
    }

    fn change_file(path: &Path, change: impl FnOnce(&mut Vec<u8>)) {
        let mut bytes = std::fs::read(path).unwrap();
        change(&mut bytes);
        std::fs::write(path, bytes).unwrap();
    }

    fn assert_damaged(result: io::Result<Vec<u8>>, id: PageId) {
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
        store.write_page(2, &[9u8; USABLE_PAGE_SIZE]).unwrap();
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
        let mut free = store.read_page(2).unwrap();
        free[1..9].copy_from_slice(&u64::MAX.to_le_bytes());
        store.write_raw(2, &free).unwrap();

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
        let before = std::fs::read(&path).unwrap();
        let page = vec![7u8; USABLE_PAGE_SIZE];
        let header = |page_count| {
            let header = Header {
                page_size: PAGE_SIZE as u32,
                page_count,
                free_list_head: NO_FREE_PAGE,
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
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing written");

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
            store.write_page(id, &[fill; USABLE_PAGE_SIZE]).unwrap();
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
        FileStore::from_file(store.file.try_clone().unwrap(), false).unwrap()
    }

    #[test]
    fn staged_writes_are_visible_to_reads_but_not_on_disk() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();

        store.begin();
        let id = store.allocate_page().unwrap();
        store.write_page(id, &[7u8; USABLE_PAGE_SIZE]).unwrap();

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
        store.write_page(id, &[9u8; USABLE_PAGE_SIZE]).unwrap();
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
        store.write_page(a, &[1u8; USABLE_PAGE_SIZE]).unwrap();
        let b = store.allocate_page().unwrap();
        store.free_page(b).unwrap(); // free list: b

        store.begin();
        assert_eq!(
            store.allocate_page().unwrap(),
            b,
            "pops b off the free list"
        );
        let c = store.allocate_page().unwrap(); // grows the file
        store.write_page(a, &[2u8; USABLE_PAGE_SIZE]).unwrap();
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
        store.write_page(a, &[1u8; USABLE_PAGE_SIZE]).unwrap();
        store.write_page(b, &[1u8; USABLE_PAGE_SIZE]).unwrap();

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
        store.write_page(a, &[1u8; USABLE_PAGE_SIZE]).unwrap();
        store.write_page(a, &[2u8; USABLE_PAGE_SIZE]).unwrap();
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
                .write_page(HEADER_PAGE, &[0u8; USABLE_PAGE_SIZE])
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
        let mut store = FileStore::open(&path).unwrap();
        store.set_cache_size(0);
        let before = store.cache_stats();
        store.read_page(1).unwrap();
        store.read_page(1).unwrap();
        assert_eq!(since(&store, before), (0, 2));
    }

    /// Staged pages never reach the cache: a rolled-back write isn't read
    /// back, a written-back one is, from the cache.
    #[test]
    fn the_cache_holds_the_file_never_a_staged_page() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        assert_eq!(store.read_page(1).unwrap(), filled(1));
        store.begin();
        store.write_page(1, &filled(9)).unwrap();
        assert_eq!(store.read_page(1).unwrap(), filled(9));
        store.rollback();
        assert_eq!(store.read_page(1).unwrap(), filled(1));

        store.begin();
        store.write_page(1, &filled(8)).unwrap();
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
            store.write_page(id, &filled(7)).unwrap();
        }
        store.commit();
        store.failing_write_backs = 1;
        store.write_back_fails_after = 1;
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
            assert_eq!(store.cached(id), Some(filled(7)), "{id}");
        }
    }

    /// Committed pages are read from memory, not the file, until the
    /// checkpoint — including pages past the file's end.
    #[test]
    fn committed_pages_are_read_before_the_checkpoint_writes_them() {
        let (_dir, path) = open_temp();
        let mut store = five_page_file(&path);
        store.begin();
        store.write_page(2, &filled(9)).unwrap();
        let new = store.allocate_page().unwrap();
        store.write_page(new, &filled(6)).unwrap();
        store.commit();
        // A second batch, with a page past the file's end.
        store.begin();
        let beyond = store.allocate_page().unwrap();
        store.write_page(beyond, &filled(5)).unwrap();
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
        assert_eq!(store.cached(1), Some(filled(8)));
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
        let mut disk = read_disk_page(&store.file, 1).unwrap();
        disk[100] ^= 1;
        write_all_at(&store.file, &disk, PAGE_SIZE as u64).unwrap();
        assert_eq!(store.read_page(1).unwrap(), filled(1));
        assert_eq!(store.damaged_pages().unwrap(), [1]);
        drop(store);
        assert_damaged(FileStore::open(&path).unwrap().read_page(1), 1);
    }
}
