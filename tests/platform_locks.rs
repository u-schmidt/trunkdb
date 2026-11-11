//! What the operating system's file locks do, as SPEC §185.6 assumes it:
//! the design of readers in other processes rests on these. std's locks
//! are `flock` on Unix and `LockFileEx` over the whole file on Windows.
//!
//! Two handles in one process stand for two processes here. `flock`
//! conflicts between two opens of a file, and `LockFileEx` applies to
//! every handle but the one that took it, in the same process too.
//!
//! Every test says what it found on stderr; CI runs this file with
//! `--nocapture`, so the log shows it for each platform.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Windows' `ERROR_LOCK_VIOLATION`: a read or write in a range another
/// handle has locked.
#[cfg(windows)]
const LOCK_VIOLATION: i32 = 33;

fn file_with(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("probe");
    std::fs::write(&path, bytes).unwrap();
    path
}

fn read_write(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

fn read_only(path: &Path) -> File {
    File::open(path).unwrap()
}

fn read_first(file: &mut File) -> io::Result<u8> {
    file.seek(SeekFrom::Start(0))?;
    let mut byte = [0u8];
    file.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn write_first(file: &mut File) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&[7])
}

fn say<T: std::fmt::Debug, E: std::fmt::Debug>(what: &str, result: &Result<T, E>) {
    eprintln!("[{}] {what}: {result:?}", std::env::consts::OS);
}

/// The writer holding the database file exclusively, as today (§21):
/// can a reader read it? §185.6 says not on Windows.
#[test]
fn an_exclusive_lock_and_another_handles_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[1, 2, 3]);
    let holder = read_write(&path);
    holder.try_lock().unwrap();

    let result = read_first(&mut read_only(&path));
    say("read beside an exclusive lock", &result);
    #[cfg(windows)]
    assert_eq!(result.unwrap_err().raw_os_error(), Some(LOCK_VIOLATION));
    #[cfg(not(windows))]
    assert_eq!(result.unwrap(), 1);
}

#[test]
fn an_exclusive_lock_and_another_handles_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[1, 2, 3]);
    let holder = read_write(&path);
    holder.try_lock().unwrap();

    let result = write_first(&mut read_write(&path));
    say("write beside an exclusive lock", &result);
    #[cfg(windows)]
    assert_eq!(result.unwrap_err().raw_os_error(), Some(LOCK_VIOLATION));
    #[cfg(not(windows))]
    result.unwrap();
}

/// Readers holding the database file shared, as §185's first draft
/// had it: can the writer's checkpoint still write it?
#[test]
fn a_shared_lock_and_another_handles_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[1, 2, 3]);
    let reader = read_only(&path);
    reader.try_lock_shared().unwrap();

    let result = write_first(&mut read_write(&path));
    say("write beside another handle's shared lock", &result);
    #[cfg(windows)]
    assert_eq!(result.unwrap_err().raw_os_error(), Some(LOCK_VIOLATION));
    #[cfg(not(windows))]
    result.unwrap();
}

/// A shared lock keeps every process from writing, Microsoft's docs say,
/// the one holding it too.
#[test]
fn a_shared_lock_and_its_own_handles_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[1, 2, 3]);
    let mut holder = read_write(&path);
    holder.try_lock_shared().unwrap();

    let result = write_first(&mut holder);
    say("write through the handle holding a shared lock", &result);
    #[cfg(windows)]
    assert_eq!(result.unwrap_err().raw_os_error(), Some(LOCK_VIOLATION));
    #[cfg(not(windows))]
    result.unwrap();
}

#[test]
fn a_shared_lock_and_another_handles_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[1, 2, 3]);
    let reader = read_only(&path);
    reader.try_lock_shared().unwrap();

    let result = read_first(&mut read_only(&path));
    say("read beside a shared lock", &result);
    assert_eq!(result.unwrap(), 1);
}

/// A reader in a directory it can't write opens the lock files for
/// reading only (§185.6): it still has to be able to lock them, both ways.
#[test]
fn a_read_only_handle_takes_both_locks() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[]);

    let shared = read_only(&path);
    let result = shared.try_lock_shared();
    say("shared lock through a read-only handle", &result);
    result.unwrap();
    shared.unlock().unwrap();

    let exclusive = read_only(&path);
    let result = exclusive.try_lock();
    say("exclusive lock through a read-only handle", &result);
    result.unwrap();

    // And it keeps others out like any other lock.
    let other = read_only(&path);
    assert!(matches!(
        other.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
}

/// Locks on an empty file, as the lock files are, between handles: what
/// the gate and the readers' lock rely on.
#[test]
fn lock_files_exclude_as_expected() {
    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[]);

    let reader_one = read_only(&path);
    let reader_two = read_only(&path);
    reader_one.try_lock_shared().unwrap();
    reader_two.try_lock_shared().unwrap();

    let writer = read_write(&path);
    assert!(matches!(
        writer.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    reader_one.unlock().unwrap();
    assert!(matches!(
        writer.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(reader_two);
    writer.try_lock().unwrap();

    let late_reader = read_only(&path);
    assert!(matches!(
        late_reader.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
}

/// Can a reader see a file's new length before its bytes, the bytes
/// reading as zeros? §185.4 makes a reader read again where it finds
/// damage, in case. This counts how often it happens here; it fails
/// only if the file can't be read at all.
#[test]
fn a_length_before_its_bytes() {
    const CHUNK: usize = 8 * 1024;
    const CHUNKS: usize = 10_000;

    let dir = tempfile::tempdir().unwrap();
    let path = file_with(dir.path(), &[]);
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writer = {
        let path = path.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            let chunk = vec![0xABu8; CHUNK];
            for _ in 0..CHUNKS {
                file.write_all(&chunk).unwrap();
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };

    let mut reader = read_only(&path);
    let mut looks = 0u64;
    let mut zeros_seen = 0u64;
    let mut buf = vec![0u8; 4096];
    loop {
        let finished = done.load(std::sync::atomic::Ordering::SeqCst);
        let len = std::fs::metadata(&path).unwrap().len();
        if len > 0 {
            let take = (len as usize).min(buf.len());
            reader.seek(SeekFrom::Start(len - take as u64)).unwrap();
            // A short read is fine: the file can't shrink here, so it
            // would mean the length showed before the bytes did.
            let got = reader.read(&mut buf[..take]).unwrap();
            looks += 1;
            if got < take || buf[..got].iter().any(|&b| b != 0xAB) {
                zeros_seen += 1;
            }
        }
        if finished {
            break;
        }
    }
    writer.join().unwrap();

    eprintln!(
        "[{}] a length before its bytes: {zeros_seen} of {looks} looks at the end found bytes not yet there",
        std::env::consts::OS
    );
}
