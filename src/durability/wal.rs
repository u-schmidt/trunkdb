use super::Durability;
use crate::crc32::crc32;
use crate::storage::{PageId, PageImage, USABLE_PAGE_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
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
/// misread.
/// Written together with the first record after a checkpoint, never on
/// its own.
const WAL_MAGIC: &[u8; 8] = b"TRUNKWAL";
const WAL_VERSION: u32 = 2;
const WAL_HEADER_LEN: usize = 12;

/// Bytes per page entry in a record body: `[u64 page id][page bytes]`.
const PAGE_ENTRY_LEN: usize = 8 + USABLE_PAGE_SIZE;

pub(crate) fn encode_header() -> [u8; WAL_HEADER_LEN] {
    let mut header = [0u8; WAL_HEADER_LEN];
    header[0..8].copy_from_slice(WAL_MAGIC);
    header[8..12].copy_from_slice(&WAL_VERSION.to_le_bytes());
    header
}

/// `[u32 body_len][u32 crc32(body)][body]`, one record per logged batch.
/// `body` is `[u32 page_count]` followed by `page_count` ×
/// `[u64 page id][USABLE_PAGE_SIZE page bytes]` — fixed-size entries, so a body's
/// length is fully determined by its count.
///
/// One record per batch is what makes a batch atomic across a crash
/// during `log` itself: recovery either finds the whole record intact and
/// restores every page in it, or finds it torn and restores none (§16.6).
/// The length prefix catches a record cut short; the CRC catches one that
/// is length-complete but whose bytes didn't all make it to disk (a
/// partially persisted sector).
pub(crate) fn encode_record(pages: &[(PageId, &[u8])]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + pages.len() * PAGE_ENTRY_LEN);
    body.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for (id, page) in pages {
        assert_eq!(
            page.len(),
            USABLE_PAGE_SIZE,
            "WAL page images are whole pages"
        );
        body.extend_from_slice(&id.to_le_bytes());
        body.extend_from_slice(page);
    }

    let mut record = Vec::with_capacity(8 + body.len());
    record.extend_from_slice(&(body.len() as u32).to_le_bytes());
    record.extend_from_slice(&crc32(&body).to_le_bytes());
    record.extend_from_slice(&body);
    record
}

/// Reads every complete batch record in `bytes` and returns their page
/// images, flattened in log order — restoring them in that order leaves
/// each page at its latest logged state.
///
/// Stops — without erroring — at a torn tail: a final record whose length
/// prefix promises more bytes than remain, or whose CRC doesn't match.
/// Either is what a crash mid-`log` looks like, and it means that batch
/// was never durably logged (and so never written back), so it's correct
/// to treat it as if it never happened. A file shorter than the header is
/// the same case: the header is only ever written together with a record.
///
/// A CRC mismatch on a record that is *not* the last one is a different,
/// stronger signal — a crash only ever tears the tail — so that's a hard
/// error, as is a CRC-valid body that doesn't decode, or a bad header.
fn decode_pending(bytes: &[u8]) -> io::Result<Vec<PageImage>> {
    if bytes.len() < WAL_HEADER_LEN {
        return Ok(Vec::new());
    }
    if &bytes[0..8] != WAL_MAGIC {
        return Err(corrupt(
            "not a trunkdb page-image WAL (bad magic) — possibly one written by an older trunkdb",
        ));
    }
    let version = read_u32(&bytes[8..]);
    if version != WAL_VERSION {
        return Err(corrupt(&format!(
            "unsupported WAL version {version}, this build expects {WAL_VERSION}"
        )));
    }

    let mut pages = Vec::new();
    let mut bytes = &bytes[WAL_HEADER_LEN..];
    while bytes.len() >= 8 {
        let body_len = read_u32(bytes) as usize;
        let checksum = read_u32(&bytes[4..]);
        let after_header = &bytes[8..];
        if after_header.len() < body_len {
            break; // torn tail — the final record was cut short
        }
        let (body, rest) = after_header.split_at(body_len);
        if crc32(body) != checksum {
            if rest.is_empty() {
                break; // torn tail — the final record's bytes didn't all land
            }
            return Err(corrupt("WAL record checksum mismatch before the tail"));
        }
        decode_batch(body, &mut pages)?;
        bytes = rest;
    }
    Ok(pages)
}

fn decode_batch(body: &[u8], pages: &mut Vec<PageImage>) -> io::Result<()> {
    if body.len() < 4 {
        return Err(corrupt("WAL batch record too short for its page count"));
    }
    let count = read_u32(body) as usize;
    let entries = &body[4..];
    if Some(entries.len()) != count.checked_mul(PAGE_ENTRY_LEN) {
        return Err(corrupt(
            "WAL batch record's length doesn't match its page count",
        ));
    }
    for entry in entries.as_chunks::<PAGE_ENTRY_LEN>().0 {
        let id = PageId::from_le_bytes(entry[0..8].try_into().unwrap());
        pages.push((id, entry[8..].to_vec()));
    }
    Ok(())
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[0..4].try_into().unwrap())
}

fn corrupt(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn wal_path(db_path: &Path) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".wal");
    PathBuf::from(os_string)
}

impl WalDurability {
    /// Opens (creating if needed) the WAL file next to `db_path`, and
    /// returns the page images of every batch still pending from a prior
    /// unclean shutdown — `Database::open` writes those back to the main
    /// file, then checkpoints, before handing out a usable `Database`.
    pub fn open(db_path: &Path) -> io::Result<(Self, Vec<PageImage>)> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(wal_path(db_path))?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let pending = decode_pending(&bytes)?;

        let len = bytes.len() as u64;
        let wal = Self {
            file,
            len,
            #[cfg(test)]
            failing_logs: 0,
            #[cfg(test)]
            failing_undos: 0,
        };
        Ok((wal, pending))
    }
}

/// The page images of every batch still pending in the WAL next to
/// `db_path`, for a read-only open (SPEC §88): read, never written —
/// a torn tail is left where it is, and a WAL that isn't there is not
/// created. Nothing pending then.
pub fn read_pending(db_path: &Path) -> io::Result<Vec<PageImage>> {
    let bytes = match std::fs::read(wal_path(db_path)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    decode_pending(&bytes)
}

impl Durability for WalDurability {
    fn log(&mut self, pages: &[(PageId, &[u8])]) -> io::Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let end = self.file.seek(SeekFrom::End(0))?;
        let mut bytes = Vec::new();
        if end == 0 {
            bytes.extend_from_slice(&encode_header());
        }
        bytes.extend_from_slice(&encode_record(pages));
        self.file.write_all(&bytes)?;
        #[cfg(test)]
        if self.failing_logs > 0 {
            self.failing_logs -= 1;
            return Err(io::Error::other("injected log failure"));
        }
        crate::storage::sync(&self.file)?;
        self.len = end + bytes.len() as u64;
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
        assert_eq!(
            recovered,
            vec![(5, page(5))],
            "header rewritten after the checkpoint"
        );
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
        let left_behind = decode_pending(&wal_bytes(&db_path)).unwrap();
        assert_eq!(left_behind.len(), 3);

        wal.undo_failed_log().unwrap();
        assert_eq!(wal_bytes(&db_path), good);
        log(&mut wal, &[(6, page(4))]);
        drop(wal);
        let (_wal, recovered) = WalDurability::open(&db_path).unwrap();
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
