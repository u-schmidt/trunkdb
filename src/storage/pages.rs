use super::cache::PageCache;
use super::{Page, PageId};
use crate::crc32::Crc32;
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

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
/// `write_page` takes — `PAGE_SIZE` without the checksum, which only
/// this module ever sees. The WAL logs pages of this size too.
pub const USABLE_PAGE_SIZE: usize = PAGE_SIZE - CHECKSUM_LEN;

/// The committed pages (SPEC §77): the file, the cache of what's in it,
/// and the pages of committed batches not written back yet. What every
/// reader reads, and all a reader reads; a batch in the making is not
/// here but in `FileStore`'s staging, the writer's alone.
///
/// Shared between the writer and the readers (SPEC §79), so everything
/// takes `&self`. What's in memory sits behind one lock: a page read
/// takes it once, shared; `commit`, the end of a `checkpoint` and a
/// page coming in from the file take it alone, briefly. The file is
/// written by `checkpoint` and `restore` only, and never where a reader
/// reads it: a page is read from the file only while it has no version
/// waiting, and only pages with one are written.
///
/// One thread at a time may call `commit`, `checkpoint`, `restore` and
/// `write_through`: the writer, which `Database` makes sure of.
pub(crate) struct Pages {
    file: File,
    memory: RwLock<Memory>,
    /// Test-only fault injection: while non-zero, each `checkpoint`
    /// writes `write_back_fails_after` pages, then fails (and decrements
    /// this) — leaving the file genuinely half-written, like a crash or a
    /// full disk would. Pages go out in ascending id order.
    #[cfg(test)]
    failing_write_backs: AtomicUsize,
    #[cfg(test)]
    write_back_fails_after: AtomicUsize,
}

/// The pages in memory, behind `Pages`' lock.
pub(super) struct Memory {
    /// Checked pages as they are in the file (SPEC §50). A lookup
    /// changes only an atomic flag, so readers look pages up together
    /// under the read lock (SPEC §65).
    pub(super) cache: PageCache,
    /// Pages of committed batches — durable in the WAL — not yet written
    /// back to the file (SPEC §51): the newest version of each, with the
    /// number of the commit that wrote it. Reads look here before the
    /// cache and the file; `checkpoint` writes them back.
    versions: BTreeMap<PageId, Version>,
    /// The number of the last commit, counted from 0 at open: in memory
    /// only, the file knows nothing of it (SPEC §77).
    seq: u64,
}

/// A committed page, and the commit that made it what it is.
struct Version {
    seq: u64,
    page: Page,
}

impl Pages {
    pub(super) fn new(file: File, cache_pages: usize) -> Self {
        Pages {
            file,
            memory: RwLock::new(Memory {
                cache: PageCache::new(cache_pages),
                versions: BTreeMap::new(),
                seq: 0,
            }),
            #[cfg(test)]
            failing_write_backs: AtomicUsize::new(0),
            #[cfg(test)]
            write_back_fails_after: AtomicUsize::new(1),
        }
    }

    /// The file itself: for the header, which `FileStore` reads and
    /// checks on its own, and for tests.
    pub(super) fn file(&self) -> &File {
        &self.file
    }

    /// The pages in memory, to look one up: readers share it (SPEC §65).
    /// A panic while the lock was held can't have left them half-changed
    /// in a way that matters — at worst a page is missing from the cache
    /// — so a poisoned lock is taken over, not passed on.
    pub(super) fn memory(&self) -> RwLockReadGuard<'_, Memory> {
        self.memory
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The pages in memory, to change: one thread at a time, and no
    /// reader meanwhile. A read that misses the cache takes it to put
    /// the page in.
    fn memory_mut(&self) -> RwLockWriteGuard<'_, Memory> {
        self.memory
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The newest committed version of a page: from the pages not
    /// written back, then the cache, then the file. What the writer
    /// reads where its batch hasn't changed a page. Shared wherever it
    /// comes from, never copied (SPEC §64). No bounds check — callers do
    /// that.
    pub(super) fn read(&self, id: PageId) -> io::Result<Page> {
        self.read_at(id, u64::MAX)
    }

    /// A page as of commit `seq` (SPEC §78): what a snapshot reads. The
    /// version waiting for a checkpoint if that commit or an earlier one
    /// wrote it, otherwise the file's: from the cache, or the file — and
    /// a page read from the file is kept in the cache, its layout checked
    /// (SPEC §66). The lock is held for the lookup only, not for reading
    /// the file.
    ///
    /// A version from a later commit is an error: the one before it is
    /// gone, since only the newest is kept. No reader meets that while a
    /// commit waits for the readers before it (SPEC §79); §80 keeps the
    /// older versions a snapshot still needs. No bounds check — callers
    /// do that.
    pub(super) fn read_at(&self, id: PageId, seq: u64) -> io::Result<Page> {
        {
            let memory = self.memory();
            match memory.versions.get(&id) {
                Some(version) if version.seq <= seq => return Ok(version.page.clone()),
                Some(version) => {
                    return Err(io::Error::other(format!(
                        "snapshot too old: page {id} as of commit {seq} is gone, commit {} changed it",
                        version.seq
                    )));
                }
                None => {}
            }
            if let Some(page) = memory.cache.get(id) {
                return Ok(page);
            }
        }
        // Not written meanwhile: a checkpoint writes only pages with a
        // version waiting, this one had none, and gets one only by a
        // commit, which waits for this reader (SPEC §79.3).
        let page = read_page_at(&self.file, id)?.checked();
        self.memory_mut().cache.put(id, page.clone());
        Ok(page)
    }

    /// The pages among the first `page_count` whose checksum doesn't
    /// match their bytes on disk, in id order — what a disk error or a
    /// change from outside trunkdb leaves. Reads the file itself. Pages
    /// not written back yet are skipped: the WAL holds them, and the
    /// file's copy is older or missing (SPEC §51).
    pub(super) fn damaged(&self, page_count: u64) -> io::Result<Vec<PageId>> {
        let mut damaged = Vec::new();
        for id in 0..page_count {
            if self.memory().versions.contains_key(&id) {
                continue;
            }
            if !checksum_matches(id, &read_disk_page(&self.file, id)?) {
                damaged.push(id);
            }
        }
        Ok(damaged)
    }

    /// Writes a page straight to the file, and the cache: for a store
    /// that isn't staging.
    pub(super) fn write_through(&self, id: PageId, data: Page) -> io::Result<()> {
        write_page_at(&self.file, id, &data)?;
        self.memory_mut().cache.put(id, data.checked());
        Ok(())
    }

    /// Makes a batch's pages, which the WAL now holds (SPEC §51), the
    /// newest committed ones, under the next commit number: read from
    /// memory until `checkpoint` writes them back. Nothing is written to
    /// the file. Their layout is checked first, once, so reads until then
    /// skip it (SPEC §66), as reads of the cache do — and outside the
    /// lock, which is held only to put the pages in.
    pub(super) fn commit(&self, dirty: BTreeMap<PageId, Page>) {
        let checked: Vec<(PageId, Page)> = dirty
            .into_iter()
            .map(|(id, page)| (id, page.checked()))
            .collect();
        let mut memory = self.memory_mut();
        memory.seq += 1;
        let seq = memory.seq;
        for (id, page) in checked {
            let replaced = memory.versions.insert(id, Version { seq, page });
            debug_assert!(
                replaced.is_none_or(|older| older.seq < seq),
                "page {id} had a version from a later commit"
            );
        }
    }

    /// The number of the last commit.
    pub(super) fn seq(&self) -> u64 {
        self.memory().seq
    }

    /// The number of the commit that wrote page `id`, while it waits for
    /// `checkpoint`.
    #[cfg(test)]
    pub(super) fn version_seq(&self, id: PageId) -> Option<u64> {
        self.memory().versions.get(&id).map(|version| version.seq)
    }

    /// How many committed pages wait for `checkpoint`.
    pub(super) fn unwritten(&self) -> usize {
        self.memory().versions.len()
    }

    /// Writes every committed page not written back yet to the file,
    /// cuts it to `page_count` pages, and `fsync`s; then they're read
    /// from the cache like any other page. On error they stay where they
    /// were, and reads still find them there — the file may now be half
    /// written, but none of the pages it's half-written with is read from
    /// it. A later call writes them all again.
    ///
    /// Beside the readers (SPEC §79): the writes and the flush hold no
    /// lock. A page being written still has its version, which is what a
    /// reader gets, so nobody reads the file where it changes. Only then,
    /// under the lock, do the pages move from the versions to the cache,
    /// at once: a reader finds each in one or the other.
    pub(super) fn checkpoint(&self, page_count: u64) -> io::Result<()> {
        // No commit comes between this and the end: the writer is here.
        let waiting: Vec<(PageId, Page)> = self
            .memory()
            .versions
            .iter()
            .map(|(&id, version)| (id, version.page.clone()))
            .collect();
        if waiting.is_empty() {
            return Ok(());
        }
        self.write_back(&waiting)?;
        self.truncate(page_count)?;
        self.sync()?;
        let mut memory = self.memory_mut();
        memory.versions.clear();
        // Not pages a later batch cut off the end: they're gone. Moved,
        // not copied: the cache takes the pages the batch staged.
        for (id, page) in waiting.into_iter().filter(|(id, _)| *id < page_count) {
            memory.cache.put(id, page);
        }
        Ok(())
    }

    /// `checkpoint`'s writes: every waiting page, in ascending id order.
    fn write_back(&self, waiting: &[(PageId, Page)]) -> io::Result<()> {
        #[cfg(test)]
        let mut written = 0;
        // The counter only exists in test builds, so `enumerate` would be
        // an unused index everywhere else.
        #[allow(clippy::explicit_counter_loop)]
        for (id, page) in waiting {
            #[cfg(test)]
            {
                let failing = self.failing_write_backs.load(Ordering::Relaxed);
                if failing > 0 && written == self.write_back_fails_after.load(Ordering::Relaxed) {
                    self.failing_write_backs
                        .store(failing - 1, Ordering::Relaxed);
                    return Err(io::Error::other("injected write-back failure"));
                }
                written += 1;
            }
            write_page_at(&self.file, *id, page)?;
        }
        Ok(())
    }

    /// Test-only: makes the next `times` checkpoints fail, each after
    /// writing `after` pages.
    #[cfg(test)]
    pub(crate) fn fail_write_backs(&self, times: usize, after: usize) {
        self.failing_write_backs.store(times, Ordering::Relaxed);
        self.write_back_fails_after.store(after, Ordering::Relaxed);
    }

    /// Crash recovery's writes: page images straight to the file, in the
    /// order given, so a page logged more than once ends at its latest
    /// image. The cache is emptied first: pages are about to change under
    /// it. No bounds check, no flush: `FileStore::restore_pages` does
    /// both. At open only, before there is a reader.
    pub(super) fn restore(&self, pages: &[super::PageImage]) -> io::Result<()> {
        {
            let mut memory = self.memory_mut();
            assert!(
                memory.versions.is_empty(),
                "Pages::restore with pages to write back"
            );
            memory.cache.clear();
        }
        for (id, page) in pages {
            write_page_at(&self.file, *id, page)?;
        }
        Ok(())
    }

    /// Cuts the file to `page_count` pages, if it's longer — after a
    /// batch that shrank it (SPEC §41). Nothing past the page count is
    /// ever read, so the cut loses nothing.
    pub(super) fn truncate(&self, page_count: u64) -> io::Result<()> {
        let len = page_count * PAGE_SIZE as u64;
        if self.file.metadata()?.len() > len {
            self.file.set_len(len)?;
            self.memory_mut().cache.retain(|id| id < page_count);
        }
        Ok(())
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        super::sync(&self.file)
    }

    /// How many pages the cache holds at most (SPEC §50); 0 turns it
    /// off. Pages over the new size are forgotten.
    pub(super) fn set_cache_pages(&self, pages: usize) {
        self.memory_mut().cache.resize(pages);
    }
}

/// The page's id is part of what's summed, so a page written to the
/// wrong place, or copied onto another, fails the check too.
pub(crate) fn checksum(id: PageId, usable: &[u8]) -> [u8; CHECKSUM_LEN] {
    let mut crc = Crc32::new();
    crc.update(&id.to_le_bytes());
    crc.update(usable);
    crc.finish().to_le_bytes()
}

pub(super) fn checksum_matches(id: PageId, disk: &[u8; PAGE_SIZE]) -> bool {
    let (usable, stored) = disk.split_at(USABLE_PAGE_SIZE);
    checksum(id, usable) == stored
}

pub(super) fn damaged(id: PageId) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "page {id} is damaged: its checksum doesn't match its bytes \
             (a disk error, or a change from outside trunkdb)"
        ),
    )
}

/// A page's bytes above this module, checksum verified and cut off.
pub(super) fn read_page_at(file: &File, id: PageId) -> io::Result<Page> {
    let disk = read_disk_page(file, id)?;
    if !checksum_matches(id, &disk) {
        return Err(damaged(id));
    }
    Ok(Page::from(&disk[..USABLE_PAGE_SIZE]))
}

/// Writes a page's bytes with their checksum appended.
pub(super) fn write_page_at(file: &File, id: PageId, data: &[u8]) -> io::Result<()> {
    let mut disk = [0u8; PAGE_SIZE];
    disk[..USABLE_PAGE_SIZE].copy_from_slice(data);
    disk[USABLE_PAGE_SIZE..].copy_from_slice(&checksum(id, data));
    write_all_at(file, &disk, id * PAGE_SIZE as u64)
}

/// A page as it is in the file, checksum included, unchecked.
pub(super) fn read_disk_page(file: &File, id: PageId) -> io::Result<[u8; PAGE_SIZE]> {
    let mut disk = [0u8; PAGE_SIZE];
    read_exact_at(file, &mut disk, id * PAGE_SIZE as u64)?;
    Ok(disk)
}

// Positional I/O (pread/pwrite-style) so reads only ever need `&File` — no
// shared mutable seek cursor to coordinate, matching `PageStore::read_page`
// taking `&self`.
#[cfg(unix)]
pub(super) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
pub(super) fn write_all_at(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, offset)
}

#[cfg(windows)]
pub(super) fn read_exact_at(file: &File, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
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
pub(super) fn write_all_at(file: &File, data: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0;
    while written < data.len() {
        let n = file.seek_write(&data[written..], offset)?;
        written += n;
        offset += n as u64;
    }
    Ok(())
}
