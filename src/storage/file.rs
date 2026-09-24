use super::{PageId, PageImage, PageStore, PageType};
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;

/// Matches LiteDB's page size, partly so numbers stay comparable to it
/// later, and because a document DB's records (whole JSON-ish objects)
/// benefit more from fewer, larger pages than a fixed-page-size OS-I/O
/// alignment argument for 4096 would buy back.
pub const PAGE_SIZE: usize = 8192;

const MAGIC: &[u8; 8] = b"TRUNKDB1";
/// The on-disk format this build reads and writes — bumped whenever a
/// page or cell layout changes. `0` (the field's bytes in every header
/// written before it existed) marks a pre-versioning file: trunkdb 0.1.0,
/// or a development build between 0.1.0 and this field (SPEC §21.2).
/// History: 1 = SPEC §21; 2 = `u32` lengths in documents and overflow
/// pages (SPEC §26); 3 = catalog cells with a kind byte, and index
/// entries (SPEC §28); 4 = indexes hold null and missing fields (SPEC §32);
/// 5 = unique indexes, a new catalog cell kind (SPEC §33).
const FORMAT_VERSION: u32 = 5;
/// Older formats this build opens as they are, because such a file *is*
/// a valid `FORMAT_VERSION` file — one that uses none of what came since
/// (for 4: unique indexes). Every header write stamps `FORMAT_VERSION`,
/// and creating anything newer allocates a page, which writes the header
/// in the same batch — so a file that uses something newer always says
/// so, and an older build refuses it instead of misreading it (SPEC §33.4).
const COMPATIBLE_OLDER_FORMATS: [u32; 1] = [4];
const HEADER_PAGE: PageId = 0;
// Page 0 is reserved for the header and is never itself a free/data page,
// so 0 doubles safely as "no free page" within the free list.
const NO_FREE_PAGE: PageId = 0;

// Header page layout (rest of the page beyond this is reserved/zeroed):
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
    fn encode(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
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
                     export it with the trunkdb version that wrote it, and import the \
                     export into a new file with this one"
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
            let mut buf = [0u8; PAGE_SIZE];
            read_page_at(&file, HEADER_PAGE, &mut buf)?;
            Header::decode(&buf)?
        };

        Ok(Self {
            file,
            header,
            staging: None,
            #[cfg(test)]
            failing_write_backs: 0,
            #[cfg(test)]
            write_back_fails_after: 1,
        })
    }

    fn write_header(&mut self) -> io::Result<()> {
        let header = self.header.encode();
        self.write_raw(HEADER_PAGE, &header)
    }

    /// Reads a page's current bytes: from the dirty set if it's there,
    /// otherwise from the file. No bounds check — callers do that.
    fn read_raw(&self, id: PageId, buf: &mut [u8]) -> io::Result<()> {
        if let Some(staging) = &self.staging
            && let Some(page) = staging.dirty.get(&id)
        {
            buf.copy_from_slice(page);
            return Ok(());
        }
        read_page_at(&self.file, id, buf)
    }

    /// Writes a page: into the dirty set while staging, otherwise
    /// straight to the file. No bounds check — callers do that.
    fn write_raw(&mut self, id: PageId, data: &[u8]) -> io::Result<()> {
        match &mut self.staging {
            Some(staging) => {
                staging.dirty.insert(id, data.to_vec());
                Ok(())
            }
            None => write_page_at(&self.file, id, data),
        }
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
        self.file.sync_all()
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

    /// Writes every dirty page to the file, `fsync`s, and ends staging.
    /// On error the dirty set is kept, still staging, so the caller can
    /// retry or `rollback` — though by then the file may hold some of the
    /// pages already (SPEC §19.6's poisoning covers that case).
    pub fn write_back(&mut self) -> io::Result<()> {
        let staging = self
            .staging
            .as_ref()
            .expect("FileStore::write_back without begin");
        #[cfg(test)]
        let mut written = 0;
        // The counter only exists in test builds, so `enumerate` would be
        // an unused index everywhere else.
        #[allow(clippy::explicit_counter_loop)]
        for (&id, page) in &staging.dirty {
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
        self.file.sync_all()?;
        self.staging = None;
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
            self.staging.is_none(),
            "FileStore::restore_pages while staging"
        );
        for (id, page) in pages {
            write_page_at(&self.file, *id, page)?;
        }
        let mut buf = [0u8; PAGE_SIZE];
        read_page_at(&self.file, HEADER_PAGE, &mut buf)?;
        self.header = Header::decode(&buf)?;
        self.file.sync_all()
    }
}

impl PageStore for FileStore {
    fn allocate_page(&mut self) -> io::Result<PageId> {
        let id = if self.header.free_list_head != NO_FREE_PAGE {
            let id = self.header.free_list_head;
            let mut buf = [0u8; PAGE_SIZE];
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
            self.header.free_list_head = next;
            id
        } else {
            let id = self.header.page_count;
            self.header.page_count += 1;
            // Materialize the page so the file's length matches page_count
            // (or, while staging, so reads of it don't run past the file's
            // end); content is unspecified (see struct doc) until the
            // caller writes it.
            self.write_raw(id, &[0u8; PAGE_SIZE])?;
            id
        };
        self.write_header()?;
        Ok(id)
    }

    fn read_page(&self, id: PageId) -> io::Result<Vec<u8>> {
        self.check_bounds(id)?;
        let mut buf = vec![0u8; PAGE_SIZE];
        self.read_raw(id, &mut buf)?;
        Ok(buf)
    }

    fn try_read_page(&self, id: PageId) -> io::Result<Option<Vec<u8>>> {
        if id >= self.header.page_count {
            return Ok(None);
        }
        let mut buf = vec![0u8; PAGE_SIZE];
        self.read_raw(id, &mut buf)?;
        Ok(Some(buf))
    }

    fn write_page(&mut self, id: PageId, data: &[u8]) -> io::Result<()> {
        if id == HEADER_PAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the header page is managed internally, not through write_page",
            ));
        }
        self.check_bounds(id)?;
        if data.len() != PAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page data must be exactly {PAGE_SIZE} bytes, got {}",
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

        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = PageType::Free as u8;
        buf[1..9].copy_from_slice(&self.header.free_list_head.to_le_bytes());
        self.write_raw(id, &buf)?;

        self.header.free_list_head = id;
        self.write_header()
    }
}

// Positional I/O (pread/pwrite-style) so reads only ever need `&File` — no
// shared mutable seek cursor to coordinate, matching `PageStore::read_page`
// taking `&self`.
fn read_page_at(file: &File, id: PageId, buf: &mut [u8]) -> io::Result<()> {
    read_exact_at(file, buf, id * PAGE_SIZE as u64)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn write_page_at(file: &File, id: PageId, data: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, id * PAGE_SIZE as u64)
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
fn write_page_at(file: &File, id: PageId, data: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut offset = id * PAGE_SIZE as u64;
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

        let mut data = vec![0u8; PAGE_SIZE];
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
            let mut data = vec![0u8; PAGE_SIZE];
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
        assert!(store.write_page(99, &[0u8; PAGE_SIZE]).is_err());
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
        store.write_page(id, &[7u8; PAGE_SIZE]).unwrap();
        assert_eq!(store.try_read_page(id).unwrap(), Some(vec![7u8; PAGE_SIZE]));
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
        assert!(store.write_page(HEADER_PAGE, &[0u8; PAGE_SIZE]).is_err());
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
        std::fs::write(path, header).unwrap();
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
        store.write_page(id, &[7u8; PAGE_SIZE]).unwrap();

        assert_eq!(store.read_page(id).unwrap(), vec![7u8; PAGE_SIZE]);
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
        store.write_page(id, &[9u8; PAGE_SIZE]).unwrap();
        store.write_back().unwrap();

        assert_eq!(store.dirty_pages().count(), 0, "write_back ends staging");
        let disk = on_disk(&store);
        assert_eq!(disk.header.page_count, 2);
        assert_eq!(disk.read_page(id).unwrap(), vec![9u8; PAGE_SIZE]);
    }

    #[test]
    fn rollback_restores_page_count_free_list_and_page_contents() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        let a = store.allocate_page().unwrap();
        store.write_page(a, &[1u8; PAGE_SIZE]).unwrap();
        let b = store.allocate_page().unwrap();
        store.free_page(b).unwrap(); // free list: b

        store.begin();
        assert_eq!(
            store.allocate_page().unwrap(),
            b,
            "pops b off the free list"
        );
        let c = store.allocate_page().unwrap(); // grows the file
        store.write_page(a, &[2u8; PAGE_SIZE]).unwrap();
        store.free_page(a).unwrap();
        store.rollback();

        assert_eq!(store.header.page_count, 3, "c's growth undone");
        assert_eq!(store.read_page(a).unwrap(), vec![1u8; PAGE_SIZE]);
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
        store.write_page(a, &[1u8; PAGE_SIZE]).unwrap();
        store.write_page(b, &[1u8; PAGE_SIZE]).unwrap();

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
        store.write_page(a, &[1u8; PAGE_SIZE]).unwrap();
        store.write_page(a, &[2u8; PAGE_SIZE]).unwrap();
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
        assert_eq!(dirty[1].1, vec![2u8; PAGE_SIZE], "the last write wins");
        assert_eq!(Header::decode(&dirty[0].1).unwrap().page_count, 3);
    }

    #[test]
    fn the_header_page_stays_protected_while_staging() {
        let (_dir, path) = open_temp();
        let mut store = FileStore::open(&path).unwrap();
        store.begin();
        assert!(store.write_page(HEADER_PAGE, &[0u8; PAGE_SIZE]).is_err());
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
}
