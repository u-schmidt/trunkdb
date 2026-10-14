use super::Durability;
use crate::FormatError;
use crate::crc32::crc32;
#[cfg(test)]
use crate::storage::PageImage;
use crate::storage::{HEADER_PAGE, PageId, PendingPages, SeePage, TakePage, USABLE_PAGE_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The real `Durability` implementation: a single append-only file
/// alongside the main database file (`<path>.wal`), holding page images.
/// `log` writes each batch as one length-prefixed, checksummed record and
/// `fsync`s before the caller writes any of those pages to the main file;
/// `checkpoint` truncates the file back to empty once they're durably
/// there (see `Database::write_batch` and `storage::FileStore`'s staging).
///
/// Crash-safety argument: if the process dies after `log` returned, the
/// WAL holds every page the batch changed. `Database::open` writes them
/// all back (`FileStore::restore_pages`) before anything reads the main
/// file — whether the crash hit before, during, or after the batch's own
/// write-back, the result is the complete post-batch state. If it dies
/// during `log`, the record is torn and recovery ignores it; the main
/// file was never touched, so the result is the complete pre-batch state.
///
/// This replaced an op-level WAL (SPEC §16), whose replay couldn't repair
/// a structure a crash had left half-written — e.g. a B-tree split with
/// only some of its pages on disk (SPEC §19.1).
pub struct WalDurability {
    file: File,
    /// The file's length as of the last `log` that succeeded, or the
    /// last `checkpoint`: every byte up to here belongs to a committed
    /// batch.
    len: u64,
    /// The most pages `log` puts into one record: `RECORD_PAGES`, but
    /// for tests of batches in several.
    record_pages: usize,
    /// Whether `log` may add to what the file holds: not to records of
    /// an older version, nor behind a torn header. A checkpoint empties
    /// the file, and `Database::open` always makes one before it logs.
    appendable: bool,
    /// Test-only: how many times the file was flushed by `log`.
    #[cfg(test)]
    pub(crate) syncs: u32,
    /// Test-only fault injection: while non-zero, each `log` writes its
    /// record whole, then fails as if the flush had (and decrements
    /// this) — the case `undo_failed_log` is for.
    #[cfg(test)]
    pub(crate) failing_logs: u32,
    /// Test-only: while non-zero, each `undo_failed_log` fails without
    /// touching the file (and decrements this).
    #[cfg(test)]
    pub(crate) failing_undos: u32,
}

/// Every non-empty WAL file starts with this: magic, then a `u32` format
/// version — so a file from another tool, or from an older trunkdb whose
/// WAL held ops instead of pages (version 0) or pages of another size
/// (1, before page checksums: SPEC §40), is a clear error rather than a
/// misread. Version 2 had one record per batch, with no flags; it is
/// still read, so a WAL a crash left under an older build is recovered
/// (SPEC §91), and never added to.
/// Written together with the first record after a checkpoint, never on
/// its own.
const WAL_MAGIC: &[u8; 8] = b"TRUNKWAL";
const WAL_VERSION: u32 = 3;
const WAL_VERSION_WITHOUT_FLAGS: u32 = 2;
const WAL_HEADER_LEN: usize = 12;

/// Bytes per page entry in a record body: `[u64 page id][page bytes]`.
const PAGE_ENTRY_LEN: usize = 8 + USABLE_PAGE_SIZE;

/// A record's flag: its batch goes on in the next record (SPEC §91).
const MORE: u32 = 1;

/// The most pages `log` puts into one record: 32 MiB of them (SPEC
/// §91). A batch of more takes several records, so that neither `log`
/// nor recovery holds more than one of this size in memory.
const RECORD_PAGES: usize = (32 << 20) / crate::storage::PAGE_SIZE;

pub(crate) fn encode_header() -> [u8; WAL_HEADER_LEN] {
    let mut header = [0u8; WAL_HEADER_LEN];
    header[0..8].copy_from_slice(WAL_MAGIC);
    header[8..12].copy_from_slice(&WAL_VERSION.to_le_bytes());
    header
}

/// Appends one record of `pages` to `record`:
/// `[u32 body_len][u32 crc32(body)][body]`, where `body` is
/// `[u32 page_count][u32 flags]` followed by `page_count` ×
/// `[u64 page id][USABLE_PAGE_SIZE page bytes]` — fixed-size entries, so
/// a body's length is fully determined by its count.
///
/// A batch is one record, or several with `MORE` set in all but the last
/// (SPEC §91). Recovery restores a batch whose last record it finds
/// intact, with every page in every record of it, and none of a batch
/// that ends torn: that is what makes a batch atomic across a crash
/// during `log` itself (§16.6). The length prefix catches a record cut
/// short; the CRC catches one that is length-complete but whose bytes
/// didn't all make it to disk (a partially persisted sector).
fn encode_record_into(record: &mut Vec<u8>, pages: &[(PageId, &[u8])], more: bool) {
    let body_len = u32::try_from(8 + pages.len() * PAGE_ENTRY_LEN)
        .expect("a record of at most `RECORD_PAGES` pages fits its length");
    let start = record.len();
    record.extend_from_slice(&body_len.to_le_bytes());
    record.extend_from_slice(&[0; 4]); // the CRC, once the body is there
    record.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    record.extend_from_slice(&(if more { MORE } else { 0 }).to_le_bytes());
    for (id, page) in pages {
        assert_eq!(
            page.len(),
            USABLE_PAGE_SIZE,
            "WAL page images are whole pages"
        );
        record.extend_from_slice(&id.to_le_bytes());
        record.extend_from_slice(page);
    }
    let crc = crc32(&record[start + 8..]);
    record[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
}

/// A batch of `pages` as `log` writes it, in records of at most
/// `record_pages` pages: for the fuzz targets and the tests.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn encode_batch(pages: &[(PageId, &[u8])], record_pages: usize) -> Vec<u8> {
    let mut batch = Vec::new();
    let records = pages.chunks(record_pages);
    let last = records.len().saturating_sub(1);
    for (i, record) in records.enumerate() {
        encode_record_into(&mut batch, record, i < last);
    }
    batch
}

/// The records of a WAL, read one at a time (SPEC §91): what is in
/// memory is one record, whatever the WAL's length.
///
/// It ends — without an error — at a torn tail: a final record whose
/// length prefix promises more bytes than remain, or whose CRC doesn't
/// match. Either is what a crash mid-`log` looks like, and it means the
/// batch was never durably logged (and so never written back), so it's
/// correct to treat it as if it never happened.
///
/// A CRC mismatch on a record that is *not* the last one is a different,
/// stronger signal — a crash only ever tears the tail, since `log`
/// flushes every record of a batch before it writes the next — so that's
/// a hard error, as is a CRC-valid body that doesn't decode, or a bad
/// header.
struct Records<R> {
    input: R,
    version: u32,
    /// How many bytes of the WAL are still to be read.
    left: u64,
    /// How many have been read, as whole records and the header.
    offset: u64,
    /// The record last read: its body.
    body: Vec<u8>,
}

/// One record, as `Records::next` hands it out.
struct Record<'a> {
    /// Whether its batch goes on in the next record.
    more: bool,
    /// Its entries, `PAGE_ENTRY_LEN` bytes each.
    entries: &'a [u8],
}

impl Record<'_> {
    fn pages(&self) -> impl Iterator<Item = (PageId, &[u8])> {
        let entries = self.entries.as_chunks::<PAGE_ENTRY_LEN>().0;
        entries.iter().map(|entry| {
            let id = PageId::from_le_bytes(entry[0..8].try_into().unwrap());
            (id, &entry[8..])
        })
    }
}

impl<R: Read> Records<R> {
    /// The records in the first `len` bytes of `input`, a WAL from its
    /// start. `None` for one shorter than its header, which holds
    /// nothing: the header is only ever written together with a record.
    fn new(mut input: R, len: u64) -> io::Result<Option<Self>> {
        if len < WAL_HEADER_LEN as u64 {
            return Ok(None);
        }
        let mut header = [0u8; WAL_HEADER_LEN];
        input.read_exact(&mut header)?;
        if &header[0..8] != WAL_MAGIC {
            return Err(FormatError::WalNotTrunkdb.into_io());
        }
        let version = read_u32(&header[8..]);
        if version != WAL_VERSION && version != WAL_VERSION_WITHOUT_FLAGS {
            return Err(FormatError::WalVersion { found: version }.into_io());
        }
        Ok(Some(Records {
            input,
            version,
            left: len - WAL_HEADER_LEN as u64,
            offset: WAL_HEADER_LEN as u64,
            body: Vec::new(),
        }))
    }

    /// The next record, or `None` at the end or at a torn tail.
    fn next(&mut self) -> io::Result<Option<Record<'_>>> {
        if self.left < 8 {
            return Ok(None);
        }
        let mut prefix = [0u8; 8];
        self.input.read_exact(&mut prefix)?;
        let body_len = u64::from(read_u32(&prefix));
        let checksum = read_u32(&prefix[4..]);
        self.left -= 8;
        if self.left < body_len {
            return Ok(None); // torn tail — the final record was cut short
        }
        // No more than is left of the file, whatever the length says.
        self.body.resize(body_len as usize, 0);
        self.input.read_exact(&mut self.body)?;
        self.left -= body_len;
        if crc32(&self.body) != checksum {
            if self.left == 0 {
                return Ok(None); // torn tail — the final record's bytes didn't all land
            }
            return Err(corrupt("WAL record checksum mismatch before the tail"));
        }
        self.offset += 8 + body_len;

        let flags_len = if self.version == WAL_VERSION { 4 } else { 0 };
        if self.body.len() < 4 + flags_len {
            return Err(corrupt("WAL batch record too short for its page count"));
        }
        let count = read_u32(&self.body) as usize;
        let flags = match flags_len {
            0 => 0,
            _ => read_u32(&self.body[4..]),
        };
        if flags & !MORE != 0 {
            return Err(corrupt(&format!(
                "WAL record with flags this build doesn't know ({flags:#x})"
            )));
        }
        let entries = &self.body[4 + flags_len..];
        if Some(entries.len()) != count.checked_mul(PAGE_ENTRY_LEN) {
            return Err(corrupt(
                "WAL batch record's length doesn't match its page count",
            ));
        }
        Ok(Some(Record {
            more: flags & MORE != 0,
            entries,
        }))
    }
}

/// What a WAL holds of batches a prior run logged and never checkpointed
/// (SPEC §91): which pages, and where the last whole batch ends — not the
/// pages themselves, which are read from the file again when they are
/// restored (`PendingPages::pages`), a record at a time. Kept beside
/// that: every image of the header page, which the checks before a
/// restore need (SPEC §85, §87), and 8 bytes a page.
///
/// Read in `scan`, once through: every record's CRC checked, a torn tail
/// found, and a batch without its last record left out, with all its
/// records.
pub struct Pending {
    /// The WAL, on a handle of its own; `None` when there is none.
    file: Option<File>,
    /// Where the last whole batch ends.
    end: u64,
    /// The page of every image in the whole batches, in log order —
    /// restoring them in that order leaves each page at its latest
    /// logged state.
    ids: Vec<PageId>,
    /// The header page's images, each with its place in `ids`.
    headers: Vec<(usize, Vec<u8>)>,
}

impl Pending {
    fn nothing() -> Self {
        Pending {
            file: None,
            end: 0,
            ids: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Reads the WAL `file` through, and returns what is pending in it
    /// with the version its header says, if it has one.
    fn scan(file: File) -> io::Result<(Self, Option<u32>)> {
        let len = file.metadata()?.len();
        let input = BufReader::with_capacity(READ_BUFFER, &file);
        let Some(mut records) = Records::new(input, len)? else {
            return Ok((Self::nothing(), None));
        };
        let version = records.version;
        let (mut ids, mut headers) = (Vec::new(), Vec::new());
        // How much of `ids`, and of the file, whole batches make up.
        let (mut whole, mut end) = (0, records.offset);
        while let Some(record) = records.next()? {
            for (id, page) in record.pages() {
                if id == HEADER_PAGE {
                    headers.push((ids.len(), page.to_vec()));
                }
                ids.push(id);
            }
            if !record.more {
                (whole, end) = (ids.len(), records.offset);
            }
        }
        // A batch without its last record: a crash mid-`log`. Its header
        // images would never be shown, with their places cut off: they
        // go only to free them.
        ids.truncate(whole);
        headers.retain(|(at, _)| *at < whole);
        let pending = Pending {
            file: Some(file),
            end,
            ids,
            headers,
        };
        Ok((pending, Some(version)))
    }

    /// Whether nothing is pending.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Every pending page image, in log order: for tests, which read
    /// small WALs.
    #[cfg(test)]
    pub(crate) fn images(&self) -> Vec<PageImage> {
        let mut images = Vec::new();
        self.pages(&mut |id, page| {
            images.push((id, page.to_vec()));
            Ok(())
        })
        .unwrap();
        images
    }
}

impl PendingPages for Pending {
    fn count(&self) -> u64 {
        self.ids.len() as u64
    }

    fn outline(&self, see: &mut SeePage<'_>) -> io::Result<()> {
        let mut headers = self.headers.iter().peekable();
        for (at, &id) in self.ids.iter().enumerate() {
            let header = headers.next_if(|(place, _)| *place == at);
            see(id, header.map(|(_, image)| &image[..]))?;
        }
        Ok(())
    }

    /// Reads the WAL again, up to the end of its last whole batch, and
    /// hands out each page as it comes: one record in memory at a time.
    fn pages(&self, take: &mut TakePage<'_>) -> io::Result<()> {
        let Some(mut file) = self.file.as_ref() else {
            return Ok(());
        };
        file.seek(SeekFrom::Start(0))?;
        let input = BufReader::with_capacity(READ_BUFFER, file);
        let mut taken = 0;
        if let Some(mut records) = Records::new(input, self.end)? {
            while let Some(record) = records.next()? {
                for (id, page) in record.pages() {
                    take(id, page)?;
                    taken += 1;
                }
            }
        }
        // As `scan` found them: we hold the database's lock, so nobody
        // else writes this WAL.
        if taken != self.ids.len() {
            return Err(corrupt("the WAL changed while it was being recovered"));
        }
        Ok(())
    }
}

/// The WAL versions recovery reads, as `FormatError::WalVersion`'s
/// message names them.
pub(crate) fn readable_wal_versions() -> String {
    format!("{WAL_VERSION_WITHOUT_FLAGS} and {WAL_VERSION}")
}

/// How many bytes of the WAL a read asks the file for at once.
const READ_BUFFER: usize = 1 << 20;

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[0..4].try_into().unwrap())
}

fn corrupt(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn wal_path(db_path: &Path) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".wal");
    PathBuf::from(os_string)
}

impl WalDurability {
    /// Opens (creating if needed) the WAL file next to `db_path`, and
    /// returns what is still pending in it from a prior unclean shutdown
    /// — `Database::open` writes those pages back to the main file, then
    /// checkpoints, before handing out a usable `Database`.
    pub fn open(db_path: &Path) -> io::Result<(Self, Pending)> {
        let path = wal_path(db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        // On a handle of its own, so reading it again for the restore
        // doesn't move the place `log` writes at.
        let (pending, version) = Pending::scan(File::open(&path)?)?;

        let wal = Self {
            file,
            len,
            record_pages: RECORD_PAGES,
            appendable: len == 0 || version == Some(WAL_VERSION),
            #[cfg(test)]
            failing_logs: 0,
            #[cfg(test)]
            failing_undos: 0,
            #[cfg(test)]
            syncs: 0,
        };
        Ok((wal, pending))
    }

    /// Test-only: at most `pages` pages in a record from here on, for
    /// batches in several records without 32 MiB of pages.
    #[cfg(test)]
    pub(crate) fn set_record_pages(&mut self, pages: usize) {
        self.record_pages = pages;
    }

    /// Flushes the file, and counts it for the tests.
    fn sync(&mut self) -> io::Result<()> {
        #[cfg(test)]
        {
            self.syncs += 1;
        }
        crate::storage::sync(&self.file)
    }
}

/// What is still pending in the WAL next to `db_path`, for a read-only
/// open (SPEC §88): read, never written — a torn tail is left where it
/// is, and a WAL that isn't there is not created. Nothing pending then.
pub fn read_pending(db_path: &Path) -> io::Result<Pending> {
    match File::open(wal_path(db_path)) {
        Ok(file) => Ok(Pending::scan(file)?.0),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Pending::nothing()),
        Err(e) => Err(e),
    }
}

impl Durability for WalDurability {
    /// One record for a batch of up to `RECORD_PAGES` pages, written
    /// with one write and flushed once, as ever. A larger batch goes out
    /// in several records (SPEC §91), each built in the same buffer and
    /// flushed before the next is written: so the last record, which
    /// makes the batch whole, is only ever on disk behind all the
    /// others, and a crash tears the WAL's tail and nothing before it.
    fn log(&mut self, pages: &[(PageId, &[u8])]) -> io::Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let end = self.file.seek(SeekFrom::End(0))?;
        if end != 0 && !self.appendable {
            return Err(io::Error::other(
                "the WAL holds records of an older version, or a torn header: checkpoint it before logging",
            ));
        }
        let records = pages.chunks(self.record_pages);
        let last = records.len() - 1;
        let largest = pages.len().min(self.record_pages);
        let mut record = Vec::with_capacity(WAL_HEADER_LEN + 16 + largest * PAGE_ENTRY_LEN);
        if end == 0 {
            record.extend_from_slice(&encode_header());
        }
        let mut len = 0;
        for (i, chunk) in records.enumerate() {
            encode_record_into(&mut record, chunk, i < last);
            self.file.write_all(&record)?;
            len += record.len() as u64;
            record.clear();
            if i < last {
                self.sync()?;
            }
        }
        #[cfg(test)]
        if self.failing_logs > 0 {
            self.failing_logs -= 1;
            return Err(io::Error::other("injected log failure"));
        }
        self.sync()?;
        self.len = end + len;
        self.appendable = true;
        Ok(())
    }

    fn undo_failed_log(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if self.failing_undos > 0 {
            self.failing_undos -= 1;
            return Err(io::Error::other("injected undo failure"));
        }
        // To `len`, not to nothing: what's before it are committed
        // batches no checkpoint has written back yet (SPEC §51).
        self.file.set_len(self.len)?;
        self.file.seek(SeekFrom::Start(self.len))?;
        crate::storage::sync(&self.file)
    }

    fn checkpoint(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        crate::storage::sync(&self.file)?;
        self.len = 0;
        self.appendable = true;
        Ok(())
    }

    fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(fill: u8) -> Vec<u8> {
        vec![fill; USABLE_PAGE_SIZE]
    }

    fn log(wal: &mut WalDurability, pages: &[(PageId, Vec<u8>)]) {
        let borrowed: Vec<(PageId, &[u8])> =
            pages.iter().map(|(id, p)| (*id, p.as_slice())).collect();
        wal.log(&borrowed).unwrap();
    }

    fn two_page_batch() -> Vec<PageImage> {
        vec![(3, page(1)), (7, page(2))]
    }

    fn wal_bytes(db_path: &Path) -> Vec<u8> {
        std::fs::read(wal_path(db_path)).unwrap()
    }

    fn set_wal_bytes(db_path: &Path, bytes: &[u8]) {
        std::fs::write(wal_path(db_path), bytes).unwrap();
    }

    #[test]
    fn log_then_open_recovers_pending_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");

        let (mut wal, pending) = WalDurability::open(&db_path).unwrap();
        assert!(pending.is_empty(), "fresh database has nothing pending");
        log(&mut wal, &two_page_batch());
        drop(wal); // simulates a crash: never checkpointed

        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert_eq!(recovered, two_page_batch());
    }

    #[test]
    fn several_batches_are_recovered_in_log_order() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");

        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        log(&mut wal, &[(3, page(1))]);
        log(&mut wal, &[(3, page(9)), (4, page(4))]); // e.g. a checkpoint that failed in between
        drop(wal);

        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert_eq!(recovered, vec![(3, page(1)), (3, page(9)), (4, page(4))]);
    }

    #[test]
    fn checkpoint_clears_pending_pages_and_the_next_log_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");

        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        log(&mut wal, &two_page_batch());
        wal.checkpoint().unwrap();
        assert!(wal_bytes(&db_path).is_empty());
        log(&mut wal, &[(5, page(5))]);
        drop(wal);

        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert_eq!(
            recovered,
            vec![(5, page(5))],
            "header rewritten after the checkpoint"
        );
    }

    fn borrowed(images: &[PageImage]) -> Vec<(PageId, &[u8])> {
        images.iter().map(|(id, p)| (*id, &p[..])).collect()
    }

    /// Pages 0 to `count`, the header's among them, each filled with its
    /// id.
    fn images(count: u64) -> Vec<PageImage> {
        (0..count).map(|id| (id, page(id as u8))).collect()
    }

    /// A WAL open on `db_path` that puts at most three pages in a record.
    fn open_with_small_records(db_path: &Path) -> WalDurability {
        let (mut wal, _) = WalDurability::open(db_path).unwrap();
        wal.record_pages = 3;
        wal
    }

    /// A batch of one record costs one write and one flush, as it always
    /// did. One over the record size goes out in several (SPEC §91),
    /// each flushed before the next is written, and is recovered whole,
    /// with the batches around it.
    #[test]
    fn a_batch_over_one_record_is_logged_in_several_and_recovered_whole() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let mut wal = open_with_small_records(&db_path);
        log(&mut wal, &two_page_batch());
        assert_eq!(wal.syncs, 1);

        let eight = images(8);
        log(&mut wal, &eight);
        assert_eq!(wal.syncs, 1 + 3, "three records, each flushed");
        let mut expected = encode_header().to_vec();
        expected.extend(encode_batch(&borrowed(&two_page_batch()), 3));
        expected.extend(encode_batch(&borrowed(&eight), 3));
        assert_eq!(wal_bytes(&db_path), expected);
        assert_eq!(wal.len(), expected.len() as u64);
        // Three records: two that say more follows, and one that doesn't.
        let one = 8 + 8 + 3 * PAGE_ENTRY_LEN;
        let batch = &expected[expected.len() - (2 * one + 16 + 2 * PAGE_ENTRY_LEN)..];
        let flags = |record: &[u8]| read_u32(&record[12..]);
        assert_eq!(
            [flags(batch), flags(&batch[one..]), flags(&batch[2 * one..])],
            [MORE, MORE, 0]
        );
        log(&mut wal, &[(9, page(9))]);
        drop(wal);

        let (_wal, pending) = WalDurability::open(&db_path).unwrap();
        let mut all = two_page_batch();
        all.extend(eight);
        all.push((9, page(9)));
        assert_eq!(pending.images(), all);
        assert_eq!(pending.count(), 11);
        // The outline: every page, and the image of the header's alone.
        let mut outline = Vec::new();
        pending
            .outline(&mut |id, header| {
                outline.push((id, header.map(<[u8]>::to_vec)));
                Ok(())
            })
            .unwrap();
        let expected: Vec<_> = all
            .iter()
            .map(|(id, page)| (*id, (*id == HEADER_PAGE).then(|| page.clone())))
            .collect();
        assert_eq!(outline, expected);
    }

    /// A batch in several records is restored whole or not at all: cut
    /// anywhere before its end, at a record's end or inside one, or with
    /// its last record damaged, none of it is recovered, and the batch
    /// before it is (SPEC §91).
    #[test]
    fn a_batch_torn_in_any_of_its_records_recovers_none_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let mut wal = open_with_small_records(&db_path);
        log(&mut wal, &two_page_batch());
        let before = wal.len() as usize;
        log(&mut wal, &images(8));
        drop(wal);
        let whole = wal_bytes(&db_path);
        let one = 8 + 8 + 3 * PAGE_ENTRY_LEN;

        let cuts = [
            before + 5,         // inside the first record's prefix
            before + one,       // after the first record
            before + one + 100, // inside the second
            before + 2 * one,   // after the second: only the last missing
            whole.len() - 1,    // one byte short
        ];
        for cut in cuts {
            set_wal_bytes(&db_path, &whole[..cut]);
            let (_wal, pending) = WalDurability::open(&db_path).unwrap();
            assert_eq!(pending.images(), two_page_batch(), "cut at {cut}");
            assert_eq!(pending.count(), 2, "cut at {cut}");
            let mut headers = 0;
            pending
                .outline(&mut |_, header| {
                    headers += header.is_some() as u32;
                    Ok(())
                })
                .unwrap();
            assert_eq!(headers, 0, "the torn batch's header image, cut at {cut}");
        }
        let mut damaged = whole.clone();
        *damaged.last_mut().unwrap() ^= 0xFF;
        set_wal_bytes(&db_path, &damaged);
        let (_wal, pending) = WalDurability::open(&db_path).unwrap();
        assert_eq!(
            pending.images(),
            two_page_batch(),
            "the last record damaged"
        );

        set_wal_bytes(&db_path, &whole);
        let (_wal, pending) = WalDurability::open(&db_path).unwrap();
        assert_eq!(pending.count(), 10);
    }

    /// Every record of a batch is flushed before the next is written, so
    /// a damaged record with another behind it is damage, not a crash:
    /// an error, inside a batch as between batches.
    #[test]
    fn a_bad_checksum_inside_a_batch_before_its_last_record_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let mut wal = open_with_small_records(&db_path);
        log(&mut wal, &images(8));
        drop(wal);
        let one = 8 + 8 + 3 * PAGE_ENTRY_LEN;
        for record in [0, 1] {
            let mut bytes = wal_bytes(&db_path);
            bytes[WAL_HEADER_LEN + record * one + 20] ^= 0xFF;
            set_wal_bytes(&db_path, &bytes);
            let err = WalDurability::open(&db_path)
                .err()
                .expect("must not recover");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "record {record}");
            bytes[WAL_HEADER_LEN + record * one + 20] ^= 0xFF;
            set_wal_bytes(&db_path, &bytes);
        }
    }

    /// A record as version 2 wrote it: no flags, a batch of its own.
    fn version_2_record(pages: &[PageImage]) -> Vec<u8> {
        let mut body = (pages.len() as u32).to_le_bytes().to_vec();
        for (id, page) in pages {
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(page);
        }
        let mut record = (body.len() as u32).to_le_bytes().to_vec();
        record.extend_from_slice(&crc32(&body).to_le_bytes());
        record.extend(body);
        record
    }

    /// A WAL a crash left under a build before §91 is recovered: version
    /// 2, a record a batch, no flags. It isn't added to: the first log
    /// after the checkpoint starts a file of this version.
    #[test]
    fn a_wal_of_the_version_before_is_recovered_and_not_added_to() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let mut old = WAL_MAGIC.to_vec();
        old.extend_from_slice(&2u32.to_le_bytes());
        old.extend(version_2_record(&two_page_batch()));
        old.extend(version_2_record(&[(5, page(5))]));
        let torn = version_2_record(&[(6, page(6))]);
        old.extend_from_slice(&torn[..torn.len() - 7]);
        set_wal_bytes(&db_path, &old);

        let (mut wal, pending) = WalDurability::open(&db_path).unwrap();
        let mut both = two_page_batch();
        both.push((5, page(5)));
        assert_eq!(pending.images(), both);
        drop(pending);
        let one = page(1);
        wal.log(&[(1, &one[..])]).unwrap_err();
        assert_eq!(wal_bytes(&db_path), old, "nothing added");

        wal.checkpoint().unwrap();
        wal.log(&[(1, &one[..])]).unwrap();
        assert_eq!(wal_bytes(&db_path)[..WAL_HEADER_LEN], encode_header());
        drop(wal);
        let (_wal, pending) = WalDurability::open(&db_path).unwrap();
        assert_eq!(pending.images(), vec![(1, one)]);

        // Any other version is refused, as before.
        for version in [1u32, 4] {
            let mut other = WAL_MAGIC.to_vec();
            other.extend_from_slice(&version.to_le_bytes());
            other.extend(version_2_record(&two_page_batch()));
            set_wal_bytes(&db_path, &other);
            let err = WalDurability::open(&db_path).err().expect("refused");
            assert!(err.to_string().contains("unsupported WAL version"), "{err}");
            assert_eq!(
                err.get_ref().unwrap().downcast_ref::<FormatError>(),
                Some(&FormatError::WalVersion { found: version })
            );
        }
    }

    /// A header a crash cut short holds nothing, and isn't written
    /// behind either.
    #[test]
    fn a_torn_header_is_not_added_to() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        set_wal_bytes(&db_path, &encode_header()[..5]);
        let (mut wal, pending) = WalDurability::open(&db_path).unwrap();
        assert!(pending.is_empty());
        let one = page(1);
        wal.log(&[(1, &one[..])]).unwrap_err();
        wal.checkpoint().unwrap();
        wal.log(&[(1, &one[..])]).unwrap();
    }

    /// A flag this build doesn't know is from a later one, which meant
    /// something by it: an error, not a guess.
    #[test]
    fn a_record_with_unknown_flags_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let mut bytes = encode_header().to_vec();
        bytes.extend(encode_batch(&borrowed(&two_page_batch()), 3));
        let record = WAL_HEADER_LEN;
        bytes[record + 12] = 2; // the flags, after length, CRC and count
        let crc = crc32(&bytes[record + 8..]);
        bytes[record + 4..record + 8].copy_from_slice(&crc.to_le_bytes());
        set_wal_bytes(&db_path, &bytes);
        let err = WalDurability::open(&db_path).err().expect("refused");
        assert!(err.to_string().contains("flags"), "{err}");
    }

    #[test]
    fn logging_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");

        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        wal.log(&[]).unwrap();
        assert!(wal_bytes(&db_path).is_empty());
    }

    /// A crash during `log` that leaves the first page of a batch fully on
    /// disk but cuts the second short must recover *neither*.
    #[test]
    fn a_batch_torn_between_its_pages_recovers_none_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");

        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        log(&mut wal, &two_page_batch());
        drop(wal);

        let bytes = wal_bytes(&db_path);
        set_wal_bytes(&db_path, &bytes[..bytes.len() - USABLE_PAGE_SIZE / 2]);

        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert!(recovered.is_empty(), "got a partial batch");
    }

    /// Length-complete but with bytes that never made it to disk —
    /// simulated by flipping the record's final bytes.
    #[test]
    fn a_length_complete_tail_with_a_bad_checksum_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");

        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        log(&mut wal, &two_page_batch());
        drop(wal);

        let mut bytes = wal_bytes(&db_path);
        let len = bytes.len();
        bytes[len - 4..].iter_mut().for_each(|b| *b ^= 0xFF);
        set_wal_bytes(&db_path, &bytes);

        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert!(recovered.is_empty());
    }

    /// A crash only ever tears the *last* record, so a bad checksum with
    /// another record after it is real corruption, not a crash artifact.
    #[test]
    fn a_bad_checksum_before_the_tail_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");

        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        log(&mut wal, &two_page_batch());
        log(&mut wal, &two_page_batch());
        drop(wal);

        let mut bytes = wal_bytes(&db_path);
        bytes[WAL_HEADER_LEN + 8] ^= 0xFF; // first byte of the first record's body
        set_wal_bytes(&db_path, &bytes);

        let err = WalDurability::open(&db_path)
            .err()
            .expect("must not recover");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// E.g. a leftover WAL from before page images: it started directly
    /// with a record length, not the magic.
    #[test]
    fn a_file_without_the_magic_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        set_wal_bytes(&db_path, &[42u8; 64]);

        let err = WalDurability::open(&db_path)
            .err()
            .expect("must not recover");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_file_shorter_than_its_header_is_a_torn_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        set_wal_bytes(&db_path, &WAL_MAGIC[..5]);

        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert!(recovered.is_empty());
    }

    // --- A failed log taken back (SPEC §81) ---

    /// A log whose flush fails may leave its record whole in the file —
    /// the next open would restore it. Taking it back cuts the log to
    /// where it was: the batches before it stay, the failed one is gone,
    /// and the next batch follows the last good one.
    #[test]
    fn a_failed_log_is_taken_back_and_the_batches_before_it_stay() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        log(&mut wal, &[(3, page(1))]);
        log(&mut wal, &[(4, page(2))]);
        let good = wal_bytes(&db_path);
        assert_eq!(wal.len(), good.len() as u64);

        wal.failing_logs = 1;
        assert!(wal.log(&[(5, &page(3))]).is_err());
        assert_eq!(wal.len(), good.len() as u64, "not counted");
        // What makes it dangerous: the record is there, and complete.
        let left_behind = read_pending(&db_path).unwrap().images();
        assert_eq!(left_behind.len(), 3);

        wal.undo_failed_log().unwrap();
        assert_eq!(wal_bytes(&db_path), good);
        log(&mut wal, &[(6, page(4))]);
        drop(wal);
        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert_eq!(recovered, vec![(3, page(1)), (4, page(2)), (6, page(4))]);
    }

    /// The first log after a checkpoint writes the file's header with
    /// its record: taken back, the file is empty again, and the next log
    /// writes the header anew.
    #[test]
    fn a_failed_first_log_is_taken_back_to_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        wal.failing_logs = 1;
        assert!(wal.log(&[(5, &page(3))]).is_err());
        assert!(!wal_bytes(&db_path).is_empty());

        wal.undo_failed_log().unwrap();
        assert!(wal_bytes(&db_path).is_empty());
        log(&mut wal, &two_page_batch());
        drop(wal);
        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
        let recovered = recovered.images();
        assert_eq!(recovered, two_page_batch());
    }

    /// After a log that succeeded there is nothing to take back.
    #[test]
    fn undoing_after_a_good_log_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.trunkdb");
        let (mut wal, _) = WalDurability::open(&db_path).unwrap();
        log(&mut wal, &two_page_batch());
        let good = wal_bytes(&db_path);
        wal.undo_failed_log().unwrap();
        assert_eq!(wal_bytes(&db_path), good);
    }
}
