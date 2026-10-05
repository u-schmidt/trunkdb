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
/// takes it once, shared; `commit`, the two ends of a `checkpoint` and a
/// page coming in from the file take it alone, briefly.
///
/// A page may have several versions here (SPEC §80): the newest, and
/// older ones for as long as a snapshot would read them. A reader names
/// its commit and gets the newest version at or before it, whatever has
/// been committed or written to the file since.
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
    /// Test-only: called by a read once it has found a page in neither
    /// the versions nor the cache, before it reads the file — to let a
    /// commit and a checkpoint of that very page pass in between.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    before_file_read: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// The pages in memory, behind `Pages`' lock.
pub(super) struct Memory {
    /// Checked pages as they are in the file (SPEC §50). A lookup
    /// changes only an atomic flag, so readers look pages up together
    /// under the read lock (SPEC §65).
    pub(super) cache: PageCache,
    /// The versions of pages kept in memory (SPEC §80), oldest first:
    /// - the newest, from its commit until a checkpoint has written it
    ///   back (SPEC §51) — durable in the WAL meanwhile;
    /// - older ones, and a newest one already written back, for as long
    ///   as a snapshot open at their commit would read one of them.
    ///
    /// A page with no entry has one version, the file's, and every
    /// reader reads that. So a page keeps its entry as long as the file
    /// holds anything but what every open snapshot should see.
    versions: BTreeMap<PageId, Vec<Version>>,
    /// How many pages' newest version waits for `checkpoint`.
    unwritten: usize,
    /// The number of the last commit, counted from 0 at open: in memory
    /// only, the file knows nothing of it (SPEC §77).
    seq: u64,
    /// Counts the checkpoints begun. A page read from the file while
    /// one began may be a page half written, or one newer than its
    /// reader's commit: the reader sees the count has moved, and looks
    /// again (SPEC §80.4).
    file_epoch: u64,
    /// How many versions are kept for open snapshots only: every version
    /// but the newest of its page.
    older: usize,
    /// At most this many of those (SPEC §83). A commit that takes them
    /// past it ends the oldest open snapshots, until what the rest read
    /// fits.
    limit: usize,
    /// Every snapshot of a commit before this one has been ended by the
    /// limit: its versions are gone, and a read as of it is refused. 0
    /// until the limit first ends one.
    ended_before: u64,
    /// How many times the limit has ended snapshots, counted by their
    /// commits.
    ended: u64,
}

/// How many older versions are kept unless `OpenOptions::snapshot_memory`
/// says otherwise (SPEC §83): 256 MiB of them, as much again as the page
/// cache's default.
pub(crate) const DEFAULT_VERSION_LIMIT: usize = (256 << 20) / PAGE_SIZE;

/// What a read gets when its snapshot has been ended by the limit, or
/// the version it needs was never kept: inside an `io::Error`, which
/// `crate::Error` turns into `Error::SnapshotTooOld`.
#[derive(Debug)]
pub(crate) struct TooOld;

impl std::fmt::Display for TooOld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("snapshot too old: the pages as it had them are no longer kept")
    }
}

impl std::error::Error for TooOld {}

fn too_old() -> io::Error {
    io::Error::other(TooOld)
}

impl Memory {
    /// Of the open snapshots' commits `live`, ascending, those the limit
    /// hasn't ended.
    fn still_open<'a>(&self, live: &'a [u64]) -> &'a [u64] {
        &live[live.partition_point(|&seq| seq < self.ended_before)..]
    }

    /// Drops the versions no snapshot in `live` would read, in every
    /// page, and counts what is left. A page left with one version,
    /// which the file holds, has none in memory any more.
    fn forget(&mut self, live: &[u64]) {
        let live = self.still_open(live);
        let mut older = 0;
        self.versions.retain(|_id, chain| {
            prune(chain, live);
            older += chain.len() - 1;
            !(chain.len() == 1 && chain[0].written)
        });
        self.older = older;
    }

    /// Ends the oldest open snapshots, one commit at a time, until the
    /// versions kept for the rest are within the limit (SPEC §83). The
    /// writer goes on either way: it is the reader that kept a snapshot
    /// too long that hears of it, on its next read.
    fn keep_within_limit(&mut self, live: &[u64]) {
        while self.older > self.limit {
            let Some(&oldest) = self.still_open(live).first() else {
                // Nobody open: what's left is on its way out.
                self.forget(live);
                return;
            };
            self.ended_before = oldest + 1;
            self.ended += 1;
            self.forget(live);
        }
    }
}

/// A committed page, and the commit that made it what it is.
struct Version {
    /// The commit that wrote it. 0 for a page as it was before the
    /// first commit that changed it since it was last without versions:
    /// kept for the snapshots open then, all of them older than that
    /// commit (SPEC §80.2).
    seq: u64,
    page: Page,
    /// Whether the file holds this version: a checkpoint wrote it, or it
    /// was read from there.
    written: bool,
}

/// The versions in `chain` no open snapshot would read, dropped: all but
/// the newest, and those an open snapshot's commit falls on — at or
/// after the version's, and before the next one's. `live` is the open
/// snapshots' commit numbers, ascending.
fn prune(chain: &mut Vec<Version>, live: &[u64]) {
    if chain.len() < 2 {
        return;
    }
    let mut next_seq = u64::MAX;
    let mut keep = vec![false; chain.len()];
    for (at, version) in chain.iter().enumerate().rev() {
        let first_at_or_after = live.partition_point(|&seq| seq < version.seq);
        let read = live
            .get(first_at_or_after)
            .is_some_and(|&seq| seq < next_seq);
        keep[at] = read || at == chain.len() - 1;
        next_seq = version.seq;
    }
    let mut keep = keep.into_iter();
    chain.retain(|_| keep.next().unwrap());
}

impl Pages {
    pub(super) fn new(file: File, cache_pages: usize) -> Self {
        Pages {
            file,
            memory: RwLock::new(Memory {
                cache: PageCache::new(cache_pages),
                versions: BTreeMap::new(),
                unwritten: 0,
                seq: 0,
                file_epoch: 0,
                older: 0,
                limit: DEFAULT_VERSION_LIMIT,
                ended_before: 0,
                ended: 0,
            }),
            #[cfg(test)]
            failing_write_backs: AtomicUsize::new(0),
            #[cfg(test)]
            write_back_fails_after: AtomicUsize::new(1),
            #[cfg(test)]
            before_file_read: std::sync::Mutex::new(None),
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

    /// The newest committed version of a page: from the versions in
    /// memory, then the cache, then the file. What the writer reads where
    /// its batch hasn't changed a page. Shared wherever it comes from,
    /// never copied (SPEC §64). No bounds check — callers do that.
    pub(super) fn read(&self, id: PageId) -> io::Result<Page> {
        self.read_at(id, u64::MAX)
    }

    /// A page as of commit `seq` (SPEC §78): what a snapshot reads. Of
    /// the versions in memory, the newest at or before `seq`; without
    /// any, the file's: from the cache, or the file — and a page read
    /// from the file is kept in the cache, its layout checked (SPEC
    /// §66). The lock is held for the lookup only, not for reading the
    /// file.
    ///
    /// `TooOld` if the limit has ended the snapshots of that commit (SPEC
    /// §83), or there are versions but none at or before `seq`: the one
    /// this reader needs wasn't kept, which doesn't happen to a snapshot
    /// the writer was told is open (`commit`, `checkpoint`). No bounds
    /// check — callers do that.
    pub(super) fn read_at(&self, id: PageId, seq: u64) -> io::Result<Page> {
        loop {
            let epoch = {
                let memory = self.memory();
                // Under the lock the versions are dropped under: a read
                // either gets its version or learns that it's gone.
                if seq < memory.ended_before {
                    return Err(too_old());
                }
                if let Some(chain) = memory.versions.get(&id) {
                    let version = chain.iter().rev().find(|version| version.seq <= seq);
                    return version.map(|v| v.page.clone()).ok_or_else(too_old);
                }
                if let Some(page) = memory.cache.get(id) {
                    return Ok(page);
                }
                memory.file_epoch
            };
            #[cfg(test)]
            {
                // Taken first, so the hook runs without the hook's lock.
                let hook = self.before_file_read.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            // No version, so the file's page is this reader's — as it was
            // at the lookup. Since then a commit may have changed the
            // page and a checkpoint written it: only a checkpoint begun
            // since can have, and that shows in the count.
            let read = read_page_at(&self.file, id);
            let mut memory = self.memory_mut();
            if memory.file_epoch != epoch {
                // Maybe half written, maybe too new: look again. The
                // version this reader needs is in memory by now.
                continue;
            }
            let page = read?.checked();
            // Still what the file holds: no checkpoint began.
            memory.cache.put(id, page.clone());
            return Ok(page);
        }
    }

    /// Test-only: `hook` runs once, in the next read that goes to the
    /// file, just before it does.
    #[cfg(test)]
    pub(crate) fn before_next_file_read(&self, hook: impl FnOnce() + Send + 'static) {
        *self.before_file_read.lock().unwrap() = Some(Box::new(hook));
    }

    /// The pages among the first `page_count` whose checksum doesn't
    /// match their bytes on disk, in id order — what a disk error or a
    /// change from outside trunkdb leaves. Reads the file itself. Pages
    /// not written back yet are skipped: the WAL holds them, and the
    /// file's copy is older or missing (SPEC §51). A page read while a
    /// checkpoint began is read again, as in `read_at`.
    pub(super) fn damaged(&self, page_count: u64) -> io::Result<Vec<PageId>> {
        let mut damaged = Vec::new();
        for id in 0..page_count {
            loop {
                let epoch = {
                    let memory = self.memory();
                    let waiting = memory
                        .versions
                        .get(&id)
                        .and_then(|chain| chain.last())
                        .is_some_and(|newest| !newest.written);
                    if waiting {
                        break;
                    }
                    memory.file_epoch
                };
                let disk = read_disk_page(&self.file, id);
                if self.memory().file_epoch != epoch {
                    continue;
                }
                if !checksum_matches(id, &disk?) {
                    damaged.push(id);
                }
                break;
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
    ///
    /// `live` is the commit numbers of the snapshots open, ascending, all
    /// of them before this commit (SPEC §80). For them the older versions
    /// stay: a page that had none in memory keeps the one from `before`,
    /// the page as this batch found it. Without a snapshot open nothing
    /// older is kept, and a commit costs what it did.
    pub(super) fn commit(
        &self,
        dirty: BTreeMap<PageId, Page>,
        mut before: BTreeMap<PageId, Page>,
        live: &[u64],
    ) {
        let checked: Vec<(PageId, Page)> = dirty
            .into_iter()
            .map(|(id, page)| (id, page.checked()))
            .collect();
        let mut memory = self.memory_mut();
        let memory = &mut *memory;
        memory.seq += 1;
        let seq = memory.seq;
        debug_assert!(live.iter().all(|&open| open < seq) && live.is_sorted());
        let open = memory.still_open(live);
        for (id, page) in checked {
            let chain = memory.versions.entry(id).or_default();
            let older_before = chain.len().saturating_sub(1);
            match chain.last() {
                Some(newest) => {
                    debug_assert!(
                        newest.seq < seq,
                        "page {id} had a version from a later commit"
                    );
                    if newest.written {
                        memory.unwritten += 1;
                    }
                }
                None => {
                    memory.unwritten += 1;
                    if !open.is_empty()
                        && let Some(page) = before.remove(&id)
                    {
                        chain.push(Version {
                            seq: 0,
                            page,
                            written: true,
                        });
                    }
                }
            }
            chain.push(Version {
                seq,
                page,
                written: false,
            });
            prune(chain, open);
            memory.older = memory.older - older_before + (chain.len() - 1);
        }
        memory.keep_within_limit(live);
    }

    /// Drops the versions no open snapshot would read (`prune`), in every
    /// page: for when snapshots have closed. A page left with one
    /// version, which the file holds, has none in memory any more.
    pub(super) fn forget(&self, live: &[u64]) {
        self.memory_mut().forget(live);
    }

    /// At most `pages` older versions kept for open snapshots (SPEC
    /// §83), from the next commit on.
    pub(super) fn set_version_limit(&self, pages: usize) {
        self.memory_mut().limit = pages;
    }

    /// How many older versions are kept for open snapshots, at most how
    /// many, and how many times that limit has ended snapshots.
    pub(crate) fn versions_kept(&self) -> (usize, usize, u64) {
        let memory = self.memory();
        (memory.older, memory.limit, memory.ended)
    }

    /// The first commit whose snapshots the limit hasn't ended: a read
    /// as of an earlier one is refused.
    pub(crate) fn ended_before(&self) -> u64 {
        self.memory().ended_before
    }

    /// The number of the last commit.
    pub(super) fn seq(&self) -> u64 {
        self.memory().seq
    }

    /// The number of the commit that wrote page `id`'s newest version,
    /// while it has versions in memory.
    #[cfg(test)]
    pub(super) fn version_seq(&self, id: PageId) -> Option<u64> {
        let memory = self.memory();
        memory
            .versions
            .get(&id)
            .map(|chain| chain.last().unwrap().seq)
    }

    /// The commits of the versions page `id` has in memory, oldest
    /// first: 0 for the page as it was before the first of them.
    #[cfg(test)]
    pub(crate) fn version_seqs(&self, id: PageId) -> Vec<u64> {
        let memory = self.memory();
        let chain = memory.versions.get(&id);
        chain.map_or(Vec::new(), |chain| chain.iter().map(|v| v.seq).collect())
    }

    /// How many versions are in memory, over all pages.
    #[cfg(test)]
    pub(crate) fn versions_held(&self) -> usize {
        self.memory().versions.values().map(Vec::len).sum()
    }

    /// How many committed pages wait for `checkpoint`.
    pub(super) fn unwritten(&self) -> usize {
        self.memory().unwritten
    }

    /// Writes every committed page not written back yet to the file,
    /// cuts it to `page_count` pages, and `fsync`s; then they're read
    /// from the cache like any other page. On error they stay where they
    /// were, and reads still find them there — the file may now be half
    /// written, but none of the pages it's half-written with is read from
    /// it. A later call writes them all again.
    ///
    /// Beside the readers (SPEC §79): the writes and the flush hold no
    /// lock. A page being written has versions in memory, which is what a
    /// reader gets, so nobody reads the file where it changes — and a
    /// reader that went to the file before the page had any sees that a
    /// checkpoint began, and looks again (`read_at`).
    ///
    /// `live` as for `commit` (SPEC §80): the file gets the newest
    /// version whoever is reading, and the versions an open snapshot
    /// reads stay in memory, the newest with them, marked as written.
    /// The rest go (`forget`).
    pub(super) fn checkpoint(&self, page_count: u64, live: &[u64]) -> io::Result<()> {
        // No commit comes between this and the end: the writer is here.
        let waiting: Vec<(PageId, Page)> = {
            let mut memory = self.memory_mut();
            // Before the first write, and with the list: a reader that
            // found no version for a page reads the file safely unless
            // the count moves under it.
            memory.file_epoch += 1;
            let unwritten = memory.versions.iter().filter_map(|(&id, chain)| {
                let newest = chain.last().unwrap();
                (!newest.written).then(|| (id, newest.page.clone()))
            });
            unwritten.collect()
        };
        if waiting.is_empty() {
            self.forget(live);
            return Ok(());
        }
        self.write_back(&waiting)?;
        self.truncate(page_count)?;
        self.sync()?;
        {
            let mut memory = self.memory_mut();
            let memory = &mut *memory;
            for (id, page) in waiting {
                let chain = memory.versions.get_mut(&id).unwrap();
                chain.last_mut().unwrap().written = true;
                // Not pages a later batch cut off the end: they're gone.
                // Shared, not copied: the cache takes the pages the batch
                // staged.
                if id < page_count {
                    memory.cache.put(id, page);
                }
            }
            memory.unwritten = 0;
        }
        self.forget(live);
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
            memory.file_epoch += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What `prune` leaves of versions from the commits `seqs`, with
    /// snapshots open at `live`.
    fn pruned(seqs: &[u64], live: &[u64]) -> Vec<u64> {
        let mut chain: Vec<Version> = seqs
            .iter()
            .map(|&seq| Version {
                seq,
                page: Page::from(Vec::new()),
                written: false,
            })
            .collect();
        prune(&mut chain, live);
        chain.iter().map(|version| version.seq).collect()
    }

    /// SPEC §80.3: the newest version stays, and of the others those an
    /// open snapshot reads: the newest at or before its commit.
    #[test]
    fn prune_keeps_the_newest_and_what_an_open_snapshot_reads() {
        // Nobody open: the newest.
        assert_eq!(pruned(&[0, 3, 5, 9], &[]), [9]);
        assert_eq!(pruned(&[9], &[]), [9]);
        assert_eq!(pruned(&[9], &[2]), [9], "one version is never dropped");
        // One snapshot, many commits since: two versions, not one each.
        assert_eq!(pruned(&[0, 3, 5, 9], &[2]), [0, 9]);
        assert_eq!(pruned(&[0, 3, 5, 9], &[3]), [3, 9], "its own commit's");
        assert_eq!(pruned(&[0, 3, 5, 9], &[4]), [3, 9]);
        assert_eq!(pruned(&[0, 3, 5, 9], &[8]), [5, 9]);
        // At the newest or after it: it reads the newest.
        assert_eq!(pruned(&[0, 3, 5, 9], &[9]), [9]);
        assert_eq!(pruned(&[0, 3, 5, 9], &[12]), [9]);
        // Several: each one's, once.
        assert_eq!(pruned(&[0, 3, 5, 9], &[1, 2, 6, 7]), [0, 5, 9]);
        assert_eq!(pruned(&[0, 3, 5, 9], &[1, 4, 8, 11]), [0, 3, 5, 9]);
        // No version at or before the snapshot: nothing kept for it.
        assert_eq!(pruned(&[3, 5, 9], &[2]), [9]);
    }
}
