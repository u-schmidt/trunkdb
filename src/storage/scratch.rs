//! The new image a compaction builds, in a file beside the database
//! instead of in memory (SPEC §108).

use super::pages::{PAGE_SIZE, read_exact_at, read_page_at, write_all_at, write_page_at};
use super::{Page, PageId, PageStore, USABLE_PAGE_SIZE};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Where a compaction builds its image: `<database>.compact`, beside the
/// WAL. Only ever there while a compaction runs, and gone as soon as
/// it's opened (`ScratchStore::create`).
pub(crate) fn scratch_path(db_path: &Path) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".compact");
    PathBuf::from(os_string)
}

/// A `ScratchStore` holds up to twice this many pages in memory, 16 MiB,
/// and then writes the older half of them out. A compaction fills its
/// data pages and leaves one after another, so the pages it changes
/// again are the last few, and these are plenty.
const HELD_PAGES: usize = 1024;

/// A file to build a database image in, page by page: what a
/// `MemoryStore` was before (SPEC §41), in a file. Laid out as a
/// database file is, each page at its id's place with its checksum, so
/// the image goes into the database by copying its bytes (`Scratch`).
/// Page 0 is the header's and stays empty: `FileStore` writes the header.
///
/// Pages written lately are held in memory, so a page changed many times
/// — the leaf an index grows at, the data page being filled — is written
/// to the file once or twice, not every time.
///
/// Only grows, as a `MemoryStore` did: a freed page would reach the
/// database as a page nobody owns, so `free_page` is an error.
pub(crate) struct ScratchStore {
    file: File,
    /// Page 0 included: the image's page count, and the file's once it
    /// is copied in.
    page_count: u64,
    /// The pages written lately, each with when it was last written.
    held: HashMap<PageId, (Page, u64)>,
    /// Counts the writes, for `held`'s order.
    writes: u64,
}

impl ScratchStore {
    /// Creates the file at `path`, emptied if one is there, and removes
    /// its name at once: it lives as long as its handle does, and a crash
    /// leaves nothing behind. On Windows, where an open file can't lose
    /// its name, it is removed when it's closed, or the process ends.
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
            options.custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        }
        let file = options.open(path)?;
        #[cfg(not(windows))]
        std::fs::remove_file(path)?;
        Ok(ScratchStore {
            file,
            page_count: 1,
            held: HashMap::new(),
            writes: 0,
        })
    }

    /// How many pages the image has, page 0 included.
    pub(crate) fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Writes every page still held to the file: the image, whole, ready
    /// to be logged and copied into the database. Not flushed: nothing
    /// relies on it surviving a crash, the WAL is what does.
    pub(crate) fn finish(mut self) -> io::Result<Scratch> {
        let mut held: Vec<(PageId, Page)> = self
            .held
            .drain()
            .map(|(id, (page, _))| (id, page))
            .collect();
        held.sort_unstable_by_key(|(id, _)| *id);
        for (id, page) in held {
            write_page_at(&self.file, id, &page)?;
        }
        Ok(Scratch {
            file: self.file,
            page_count: self.page_count,
        })
    }

    /// When twice `HELD_PAGES` are held: the older half written out, in
    /// id order, and let go.
    fn spill(&mut self) -> io::Result<()> {
        let mut by_age: Vec<(u64, PageId)> = self
            .held
            .iter()
            .map(|(&id, &(_, written))| (written, id))
            .collect();
        by_age.sort_unstable();
        let mut oldest: Vec<PageId> = by_age[..HELD_PAGES].iter().map(|&(_, id)| id).collect();
        oldest.sort_unstable();
        for id in oldest {
            let (page, _) = self.held.remove(&id).expect("a held page");
            write_page_at(&self.file, id, &page)?;
        }
        Ok(())
    }

    fn check_id(&self, id: PageId) -> io::Result<()> {
        match id {
            0 => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page 0 is the header, not a page to read or write",
            )),
            id if id >= self.page_count => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("page {id} out of bounds (page_count = {})", self.page_count),
            )),
            _ => Ok(()),
        }
    }
}

impl PageStore for ScratchStore {
    fn allocate_page(&mut self) -> io::Result<PageId> {
        let id = self.page_count;
        self.page_count += 1;
        self.write_page(id, Page::from(vec![0u8; USABLE_PAGE_SIZE]))?;
        Ok(id)
    }

    fn read_page(&self, id: PageId) -> io::Result<Page> {
        self.check_id(id)?;
        match self.held.get(&id) {
            Some((page, _)) => Ok(page.clone()),
            None => read_page_at(&self.file, id),
        }
    }

    fn try_read_page(&self, id: PageId) -> io::Result<Option<Page>> {
        if id >= self.page_count {
            return Ok(None);
        }
        self.read_page(id).map(Some)
    }

    fn write_page(&mut self, id: PageId, data: Page) -> io::Result<()> {
        self.check_id(id)?;
        if data.len() != USABLE_PAGE_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "page data must be exactly {USABLE_PAGE_SIZE} bytes, got {}",
                    data.len()
                ),
            ));
        }
        self.writes += 1;
        self.held.insert(id, (data, self.writes));
        if self.held.len() >= 2 * HELD_PAGES {
            self.spill()?;
        }
        Ok(())
    }

    fn free_page(&mut self, id: PageId) -> io::Result<()> {
        Err(io::Error::other(format!(
            "page {id}: a compaction's image doesn't free pages (it only grows)"
        )))
    }
}

/// A finished image (`ScratchStore::finish`): what a compaction stages
/// in place of the whole file (`FileStore::replace_all`), logs from here,
/// and copies into the database at its checkpoint. Shared, read-only,
/// between the writer and the readers that read the new pages before
/// then (`Pages`). The file goes with the last handle.
pub(crate) struct Scratch {
    file: File,
    page_count: u64,
}

/// How many bytes a copy into the database moves at once: 1 MiB.
const COPY_PAGES: usize = (1 << 20) / PAGE_SIZE;

impl Scratch {
    /// How many pages the image has, page 0 included.
    pub(crate) fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Page `id`, 1 to `page_count - 1`, its checksum verified, its
    /// layout checked as a page read from the database's file is.
    pub(crate) fn read(&self, id: PageId) -> io::Result<Page> {
        debug_assert!(id != 0 && id < self.page_count, "page {id} of the image");
        Ok(read_page_at(&self.file, id)?.checked())
    }

    /// Every page but 0, in id order, read one at a time: what the WAL
    /// logs.
    pub(crate) fn pages(&self) -> impl Iterator<Item = io::Result<(PageId, Page)>> + '_ {
        (1..self.page_count).map(|id| Ok((id, read_page_at(&self.file, id)?)))
    }

    /// Copies pages `1..page_count` into `to` at the same places, their
    /// checksums with them, a megabyte at a time. `stop_after`, in tests,
    /// fails the copy once so many pages are written.
    pub(crate) fn copy_into(&self, to: &File, stop_after: Option<u64>) -> io::Result<()> {
        let mut buffer = vec![0u8; COPY_PAGES * PAGE_SIZE];
        let mut id = 1;
        while id < self.page_count {
            let mut n = (self.page_count - id).min(COPY_PAGES as u64);
            let stop = stop_after.is_some_and(|after| id - 1 + n > after);
            if let Some(after) = stop_after.filter(|_| stop) {
                n = after - (id - 1);
            }
            let bytes = &mut buffer[..n as usize * PAGE_SIZE];
            read_exact_at(&self.file, bytes, id * PAGE_SIZE as u64)?;
            write_all_at(to, bytes, id * PAGE_SIZE as u64)?;
            if stop {
                return Err(io::Error::other("injected write-back failure"));
            }
            id += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8) -> Page {
        Page::from(vec![fill; USABLE_PAGE_SIZE])
    }

    #[test]
    fn allocates_from_1_and_reads_back_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ScratchStore::create(&dir.path().join("image")).unwrap();
        assert_eq!(store.try_read_page(1).unwrap(), None);
        assert_eq!(store.allocate_page().unwrap(), 1);
        assert_eq!(store.allocate_page().unwrap(), 2);
        store.write_page(2, page(7)).unwrap();
        assert_eq!(store.read_page(1).unwrap(), vec![0u8; USABLE_PAGE_SIZE]);
        assert_eq!(store.try_read_page(2).unwrap().unwrap(), page(7));
        assert_eq!(store.page_count(), 3);
        // Held as given, and read back shared (SPEC §67).
        let shared = page(5);
        store.write_page(1, shared.clone()).unwrap();
        assert!(store.read_page(1).unwrap().shares(&shared));

        let image = store.finish().unwrap();
        assert_eq!(image.page_count(), 3);
        let pages: Vec<(PageId, Page)> = image.pages().map(Result::unwrap).collect();
        assert_eq!(pages, vec![(1, page(5)), (2, page(7))]);
        assert_eq!(image.read(2).unwrap(), page(7));
    }

    #[test]
    fn the_header_page_bounds_sizes_and_freeing_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ScratchStore::create(&dir.path().join("image")).unwrap();
        store.allocate_page().unwrap();
        assert!(store.read_page(0).is_err());
        assert!(store.write_page(0, page(0)).is_err());
        assert!(store.read_page(2).is_err());
        assert!(store.write_page(1, vec![0u8; 10].into()).is_err());
        assert!(store.free_page(1).is_err());
    }

    /// Five times as many pages as are held, each written twice — the
    /// second time long after it left memory: what's held stays under
    /// twice `HELD_PAGES`, and every page reads back as last written,
    /// before `finish` and after.
    #[test]
    fn pages_past_what_is_held_go_to_the_file_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ScratchStore::create(&dir.path().join("image")).unwrap();
        let count = 5 * HELD_PAGES as u64;
        let fill = |id: PageId, round: u64| (id * 7 + round) as u8;
        for round in 0..2 {
            for id in 1..=count {
                if round == 0 {
                    assert_eq!(store.allocate_page().unwrap(), id);
                }
                store.write_page(id, page(fill(id, round))).unwrap();
                assert!(store.held.len() < 2 * HELD_PAGES);
            }
        }
        for id in 1..=count {
            assert_eq!(store.read_page(id).unwrap(), page(fill(id, 1)), "{id}");
        }
        let image = store.finish().unwrap();
        for (id, read) in image.pages().map(Result::unwrap) {
            assert_eq!(read, page(fill(id, 1)), "{id}");
        }
        assert_eq!(image.pages().count() as u64, count);
    }

    /// The name goes at once where a file can lose it while open, and
    /// with the last handle everywhere: nothing is left beside the
    /// database.
    #[test]
    fn the_file_is_gone_with_its_last_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        std::fs::write(&path, b"left by a crash").unwrap();
        let mut store = ScratchStore::create(&path).unwrap();
        assert_eq!(path.exists(), cfg!(windows));
        store.allocate_page().unwrap();
        let image = store.finish().unwrap();
        assert_eq!(image.read(1).unwrap(), page(0));
        drop(image);
        assert!(!path.exists());
    }

    /// Pages 1 and up land at their places with their checksums, a
    /// megabyte at a time; page 0 and what's past the image stay as they
    /// were. A copy stopped after `n` pages wrote exactly those.
    #[test]
    fn copy_into_writes_the_pages_at_their_places() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ScratchStore::create(&dir.path().join("image")).unwrap();
        let count = 3 * COPY_PAGES as u64 + 5;
        for id in 1..count {
            store.allocate_page().unwrap();
            store.write_page(id, page(id as u8)).unwrap();
        }
        let image = store.finish().unwrap();
        for stop in [
            None,
            Some(0),
            Some(1),
            Some(COPY_PAGES as u64),
            Some(count - 2),
        ] {
            let path = dir.path().join("to");
            let to = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            let len = (count + 2) * PAGE_SIZE as u64;
            to.set_len(len).unwrap();
            let copied = image.copy_into(&to, stop);
            assert_eq!(copied.is_err(), stop.is_some(), "{stop:?}");
            let written = stop.unwrap_or(count - 1);
            for id in 0..count + 2 {
                let disk = super::super::pages::read_disk_page(&to, id).unwrap();
                match id >= 1 && id <= written {
                    true => assert_eq!(read_page_at(&to, id).unwrap(), page(id as u8)),
                    false => assert_eq!(disk, [0u8; PAGE_SIZE], "{stop:?} {id}"),
                }
            }
            assert_eq!(to.metadata().unwrap().len(), len);
        }
    }
}
