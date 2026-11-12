//! The three lock files beside a database, from format 17 on (SPEC
//! §185.4, §185.6): `<path>.writer`, `<path>.readers` and `<path>.gate`,
//! one lock each, as std's locks cover a whole file. No lock stays on
//! the database file or the WAL, where on Windows it would keep others
//! from reading or writing them. And a fourth file, never locked:
//! `<path>.letgo`, the writer's request to readers to let go (SPEC
//! §189).
//!
//! - The writer's lock: exclusive, for as long as the writer has the
//!   database open.
//! - The readers' lock: shared by a reader for as long as a snapshot of
//!   its own is open; exclusive for whatever writes the database file.
//! - The gate: passed by a reader before it takes the readers' lock, and
//!   shut by the writer to get a turn between readers that overlap.
//! - The request: one byte, 1 while the writer, its gate shut, asks the
//!   readers to let go, 0 else. Each reader's watcher looks at it while
//!   its process holds the readers' lock (SPEC §189).
//!
//! Made by whoever needs one first, and never removed: a lock file
//! removed while another process opens it leaves two files, and two
//! "exclusive" locks.

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// The lock files of the database at `db`, the request among them.
pub(crate) fn lock_paths(db: &Path) -> [PathBuf; 4] {
    [".writer", ".readers", ".gate", ".letgo"].map(|suffix| lock_path(db, suffix))
}

/// How often a reader's watcher looks at the request to let go, while
/// its process holds the readers' lock (SPEC §189).
pub(crate) const LET_GO_LOOK: Duration = Duration::from_millis(20);

/// How long a writer that asks readers to let go waits for them, at
/// least (SPEC §189): many times `LET_GO_LOOK`.
pub(crate) const LET_GO_WAIT: Duration = Duration::from_secs(1);

/// The request to let go, as `<path>.letgo` holds it: 1 while asked.
fn asked(letgo: &File) -> io::Result<bool> {
    let mut byte = [0u8];
    match super::pages::read_exact_at(letgo, &mut byte, 0) {
        Ok(()) => Ok(byte[0] == 1),
        // Not written yet: nobody has asked.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn ask(letgo: &File, asking: bool) -> io::Result<()> {
    super::pages::write_all_at(letgo, &[u8::from(asking)], 0)
}

/// `<path>` with `suffix` after it, as the WAL is named (`wal_path`).
fn lock_path(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// The lock file `db` with `suffix`, opened for reading and writing and
/// made if it isn't there; or for reading only, where it is there but
/// may not be written, since a lock needs a handle, not the right to
/// write (SPEC §185.11). `None` where it isn't there and can't be made.
fn open_lock_file(db: &Path, suffix: &str) -> io::Result<Option<File>> {
    let path = lock_path(db, suffix);
    match super::open_or_create(&path) {
        Ok(file) => Ok(Some(file)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            match File::open(&path) {
                Ok(file) => Ok(Some(file)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// Whether `error` is Windows' refusal of a read or a write on a file
/// another handle has locked: `ERROR_LOCK_VIOLATION` (33), which std
/// reports as `ErrorKind::Uncategorized`, so the raw code is matched
/// (SPEC §185.11). Never elsewhere: there, locks don't keep anyone from
/// reading or writing.
pub(crate) fn is_lock_violation(error: &io::Error) -> bool {
    cfg!(windows) && error.raw_os_error() == Some(33)
}

/// How long a reader on Windows tries a read again that a lock refused,
/// before it gives up its snapshot (SPEC §185.6): well under
/// `OpenOptions::wait_for_readers`' default, so a writer's open waiting
/// for it gets its turn.
pub(crate) const REFUSED_FOR: Duration = Duration::from_secs(1);

/// The pause between two tries of a read a lock refused.
pub(crate) const REFUSED_PAUSE: Duration = Duration::from_millis(5);

/// Tries `lock` again and again, with short pauses, until it is had or
/// `deadline` has passed: std's locks wait for good or not at all
/// (SPEC §185.4). Whether it was had.
fn try_until(
    deadline: Instant,
    mut lock: impl FnMut() -> Result<(), TryLockError>,
) -> io::Result<bool> {
    let mut pause = Duration::from_micros(200);
    loop {
        match lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::Error(e)) => return Err(e),
            Err(TryLockError::WouldBlock) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        std::thread::sleep(pause.min(deadline - now));
        pause = (pause * 2).min(Duration::from_millis(10));
    }
}

/// A lock file held exclusively, let go of when dropped.
pub(crate) struct Exclusive(Arc<File>);

impl Drop for Exclusive {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// The writer's three lock files (SPEC §185.6): its own lock held for as
/// long as these live, the other two taken as a write of the database
/// file needs them.
pub(crate) struct WriterLocks {
    /// Held exclusively from the open on; let go of when dropped, with
    /// the handle.
    _writer: File,
    readers: Arc<File>,
    gate: Arc<File>,
    /// The request to let go (SPEC §189).
    letgo: File,
}

impl WriterLocks {
    /// Step 1 of a writer's open (SPEC §185.6): `<path>.writer`, made
    /// if it isn't there, and locked exclusively, without waiting. A
    /// second writer is refused here, before it touches the database
    /// file, where on Windows its lock would fail the running writer's
    /// checkpoints and the readers' reads.
    pub(crate) fn writer(db: &Path) -> io::Result<File> {
        let file = super::open_or_create(&lock_path(db, ".writer"))?;
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "database file is already open for writing (by another process, or another \
                 handle in this one)",
            ),
            TryLockError::Error(e) => e,
        })?;
        Ok(file)
    }

    /// Step 3 of a writer's open: the readers' lock file, the gate and
    /// the request, made if they aren't there, beside `writer`, the lock
    /// of step 1. Where a writer has been, all four are there. The
    /// request is withdrawn, should a writer have ended while it asked.
    pub(crate) fn new(writer: File, db: &Path) -> io::Result<Self> {
        let made = |suffix| super::open_or_create(&lock_path(db, suffix));
        let letgo = made(".letgo")?;
        ask(&letgo, false)?;
        Ok(WriterLocks {
            _writer: writer,
            readers: made(".readers").map(Arc::new)?,
            gate: made(".gate").map(Arc::new)?,
            letgo,
        })
    }

    /// The readers' lock, exclusively, if nobody reads now: no wait.
    pub(crate) fn try_readers(&self) -> io::Result<Option<Exclusive>> {
        match self.readers.try_lock() {
            Ok(()) => Ok(Some(Exclusive(Arc::clone(&self.readers)))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// The readers' lock, exclusively, through the gate (SPEC §185.4):
    /// the gate shut, so that no new reader begins, and the readers
    /// under way waited for, all within `bound`; the gate open again
    /// either way before this returns. `None` if a reader was still
    /// there at the bound. The readers' lock isn't held while waiting
    /// for the gate, so the writer and a reader can't hold each other.
    pub(crate) fn readers_through_gate(&self, bound: Duration) -> io::Result<Option<Exclusive>> {
        self.readers_gate(bound, false)
    }

    /// `readers_through_gate`, asking the readers under way to let go
    /// (SPEC §189): with the gate shut, the request is made, and
    /// withdrawn again before the gate opens, had or not. A reader's
    /// watcher ends its process's snapshots then, within `LET_GO_LOOK`.
    pub(crate) fn readers_asked(&self, bound: Duration) -> io::Result<Option<Exclusive>> {
        self.readers_gate(bound, true)
    }

    fn readers_gate(&self, bound: Duration, asking: bool) -> io::Result<Option<Exclusive>> {
        let deadline = Instant::now() + bound;
        if !try_until(deadline, || self.gate.try_lock())? {
            return Ok(None);
        }
        let gate = Exclusive(Arc::clone(&self.gate));
        if asking {
            ask(&self.letgo, true)?;
        }
        let had = try_until(deadline, || self.readers.try_lock());
        let withdrawn = if asking {
            ask(&self.letgo, false)
        } else {
            Ok(())
        };
        drop(gate);
        let had = had?;
        let readers = had.then(|| Exclusive(Arc::clone(&self.readers)));
        withdrawn?;
        Ok(readers)
    }
}

/// A reader's two lock files (SPEC §185.4), and who in this process
/// holds the readers' lock: one lock for the open, which its snapshots
/// and threads count, and the last lets go. With the request to let go
/// and the watcher that looks at it (SPEC §189).
pub(crate) struct ReaderLocks {
    readers: File,
    gate: File,
    /// Held while passing the gate, so that two threads don't each take
    /// and let go of the gate's one lock on this handle, the second
    /// letting go of the first's.
    passing: Mutex<()>,
    /// How many snapshots hold the readers' lock now.
    holders: Mutex<usize>,
    /// Woken when the readers' lock is taken, for the watcher to look.
    held: Condvar,
    /// Counts the times this process let go of the readers' lock when
    /// asked (SPEC §189). A snapshot taken before is ended: its hold no
    /// longer counts, and its reads fail as too old (`Pages::beside`).
    ended: Arc<AtomicU64>,
    /// The request to let go, and a handle of the gate of its own, to
    /// see whether a writer is asking now; `None` without the request's
    /// file, which every writer of this release makes.
    request: Option<(File, File)>,
    /// Run by the watcher between seeing a request and letting go, for a
    /// test to withdraw the request there.
    #[cfg(test)]
    before_let_go: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ReaderLocks {
    /// The readers' lock file and the gate, made if they aren't there;
    /// `None` if either isn't there and can't be made, as in a
    /// directory the reader may not write (SPEC §185.6). With a watcher
    /// of the request to let go, where the request is there (SPEC §189).
    pub(crate) fn open(db: &Path) -> io::Result<Option<Arc<Self>>> {
        let Some(readers) = open_lock_file(db, ".readers")? else {
            return Ok(None);
        };
        let Some(gate) = open_lock_file(db, ".gate")? else {
            return Ok(None);
        };
        let request = match (open_lock_file(db, ".letgo")?, open_lock_file(db, ".gate")?) {
            (Some(letgo), Some(gate)) => Some((letgo, gate)),
            _ => None,
        };
        let watched = request.is_some();
        let locks = Arc::new(ReaderLocks {
            readers,
            gate,
            passing: Mutex::new(()),
            holders: Mutex::new(0),
            held: Condvar::new(),
            ended: Arc::new(AtomicU64::new(0)),
            request,
            #[cfg(test)]
            before_let_go: Mutex::new(None),
        });
        if watched {
            let weak = Arc::downgrade(&locks);
            std::thread::Builder::new()
                .name("trunkdb-reader-watch".into())
                .spawn(move || watch(weak))?;
        }
        Ok(Some(locks))
    }

    /// How many times this process has let go when asked: what a
    /// snapshot's pages compare with (`Pages::beside`).
    pub(crate) fn ended(&self) -> &Arc<AtomicU64> {
        &self.ended
    }

    /// Whether a writer asks the readers to let go now: the request made,
    /// and the gate shut. A request a writer that ended left behind has
    /// the gate open, and is not one.
    fn asked_now(&self) -> io::Result<bool> {
        let Some((letgo, gate)) = &self.request else {
            return Ok(false);
        };
        if !asked(letgo)? {
            return Ok(false);
        }
        match gate.try_lock_shared() {
            Ok(()) => {
                let _ = gate.unlock();
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// Lets go of the readers' lock for every snapshot holding it, if a
    /// writer asks (SPEC §189): they are ended from here, their holds no
    /// longer count, and the next snapshot takes the lock anew. The
    /// request is looked at again under `holders`, where a snapshot
    /// counts itself in: a writer may have withdrawn it since the first
    /// look, and a snapshot that came after must not be ended (§189.7).
    fn let_go_if_asked(&self) -> io::Result<()> {
        if !self.asked_now()? {
            return Ok(());
        }
        #[cfg(test)]
        {
            let hook = self.before_let_go.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut holders = self.holders.lock().unwrap_or_else(|p| p.into_inner());
        if *holders > 0 && self.asked_now()? {
            self.ended.fetch_add(1, Ordering::SeqCst);
            *holders = 0;
            let _ = self.readers.unlock();
        }
        Ok(())
    }

    /// Passes the gate, waiting while the writer has it shut, then holds
    /// the readers' lock, waiting while a checkpoint has it. Every new
    /// snapshot passes the gate, also one that only counts up because
    /// another holds the lock already: otherwise threads reading in turn
    /// would keep the count above zero, and the checkpoint out.
    pub(crate) fn hold(self: &Arc<Self>) -> io::Result<ReadersHold> {
        {
            let _passing = self.passing.lock().unwrap_or_else(|p| p.into_inner());
            self.gate.lock_shared()?;
            let _ = self.gate.unlock();
        }
        let mut holders = self.holders.lock().unwrap_or_else(|p| p.into_inner());
        if *holders == 0 {
            self.readers.lock_shared()?;
            self.held.notify_all();
        }
        *holders += 1;
        Ok(ReadersHold {
            locks: Arc::clone(self),
            ended: self.ended.load(Ordering::SeqCst),
        })
    }
}

/// The watcher of a reader's process (SPEC §189): asleep while nothing
/// holds the readers' lock; while something does, it looks at the
/// request to let go every `LET_GO_LOOK`, and lets go when asked. Ends
/// with the reader's locks.
fn watch(locks: Weak<ReaderLocks>) {
    loop {
        let Some(strong) = locks.upgrade() else {
            return;
        };
        {
            let holders = strong.holders.lock().unwrap_or_else(|p| p.into_inner());
            if *holders == 0 {
                // Woken when a snapshot takes the lock; else once a second
                // or so, to end with the locks.
                let _ = strong.held.wait_timeout(holders, Duration::from_secs(1));
                continue;
            }
        }
        // Errors: looked at again next time.
        let _ = strong.let_go_if_asked();
        drop(strong);
        std::thread::sleep(LET_GO_LOOK);
    }
}

/// One snapshot's share of the readers' lock (`ReaderLocks::hold`): the
/// last to go lets go of the lock. One taken before the process let go
/// when asked (SPEC §189) counts no more.
pub(crate) struct ReadersHold {
    locks: Arc<ReaderLocks>,
    /// `ReaderLocks::ended` when it was taken.
    ended: u64,
}

impl ReadersHold {
    /// `ReaderLocks::ended` when it was taken: what the pages of the
    /// snapshot it holds for compare with.
    pub(crate) fn ended_at(&self) -> u64 {
        self.ended
    }

    /// Whether the process has let go since it was taken, as asked.
    pub(crate) fn is_ended(&self) -> bool {
        self.locks.ended.load(Ordering::SeqCst) != self.ended
    }
}

impl Drop for ReadersHold {
    fn drop(&mut self) {
        let mut holders = self.locks.holders.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_ended() {
            return;
        }
        *holders -= 1;
        if *holders == 0 {
            let _ = self.locks.readers.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.trunkdb");
        (dir, path)
    }

    /// A second writer is refused at `.writer`, and gets it once the
    /// first is gone.
    #[test]
    fn one_writer_at_a_time() {
        let (_dir, path) = temp();
        let first = WriterLocks::writer(&path).unwrap();
        let err = WriterLocks::writer(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(first);
        WriterLocks::writer(&path).unwrap();
    }

    /// The readers' lock: a reader keeps the writer's try out, and the
    /// last of a process's snapshots lets it go.
    #[test]
    fn readers_hold_the_lock_until_the_last_lets_go() {
        let (_dir, path) = temp();
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        let reader = ReaderLocks::open(&path).unwrap().unwrap();
        let first = reader.hold().unwrap();
        let second = reader.hold().unwrap();
        assert!(writer.try_readers().unwrap().is_none());
        drop(first);
        assert!(writer.try_readers().unwrap().is_none(), "one still reads");
        drop(second);
        let turn = writer.try_readers().unwrap().expect("nobody reads");
        drop(turn);
        drop(reader.hold().unwrap());
    }

    /// Asked to let go (SPEC §189), a reader's watcher lets go of the
    /// readers' lock for every snapshot of its process, within a few
    /// looks: the writer gets it, the holds are ended and count no more,
    /// and the next snapshot takes the lock anew. The request is
    /// withdrawn before the gate opens again.
    #[test]
    fn a_reader_asked_to_let_go_lets_go() {
        let (_dir, path) = temp();
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        let reader = ReaderLocks::open(&path).unwrap().unwrap();
        // The watcher asleep, so that the hold has to wake it.
        std::thread::sleep(Duration::from_millis(100));
        let first = reader.hold().unwrap();
        let second = reader.hold().unwrap();
        let start = Instant::now();
        let turn = writer.readers_asked(LET_GO_WAIT).unwrap().expect("let go");
        assert!(start.elapsed() < LET_GO_WAIT / 2);
        assert!(first.is_ended() && second.is_ended());
        assert!(!asked(&writer.letgo).unwrap(), "withdrawn");
        drop(turn);

        let third = reader.hold().unwrap();
        assert!(!third.is_ended());
        drop((first, second));
        assert!(
            writer.try_readers().unwrap().is_none(),
            "the third holds it"
        );
        drop(third);
        drop(writer.try_readers().unwrap().expect("nobody reads"));
    }

    /// A request withdrawn between the watcher's first look and its
    /// letting go: a snapshot that came after the gate opened again is
    /// not ended, nor the one under way (SPEC §189.7).
    #[test]
    fn a_request_withdrawn_before_the_watcher_lets_go_ends_nobody() {
        let (_dir, path) = temp();
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        let reader = ReaderLocks::open(&path).unwrap().unwrap();
        let first = reader.hold().unwrap();
        let (seen, seen_rx) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        *reader.before_let_go.lock().unwrap() = Some(Box::new(move || {
            seen.send(()).unwrap();
            go_rx.recv().unwrap();
        }));

        // The writer asks, as `readers_gate` does.
        writer.gate.try_lock().unwrap();
        ask(&writer.letgo, true).unwrap();
        seen_rx.recv_timeout(Duration::from_secs(5)).expect("seen");
        // ... and withdraws, having waited in vain; then a new snapshot.
        ask(&writer.letgo, false).unwrap();
        writer.gate.unlock().unwrap();
        let second = reader.hold().unwrap();
        go.send(()).unwrap();

        std::thread::sleep(LET_GO_LOOK * 5);
        assert!(!second.is_ended(), "came after the request");
        assert!(!first.is_ended());
        drop((first, second));
        drop(writer.try_readers().unwrap().expect("nobody reads"));
    }

    /// A request with the gate open is one a writer left as it ended:
    /// the readers hold on (SPEC §189).
    #[test]
    fn a_request_left_behind_is_no_request() {
        let (_dir, path) = temp();
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        let reader = ReaderLocks::open(&path).unwrap().unwrap();
        let hold = reader.hold().unwrap();
        ask(&writer.letgo, true).unwrap();
        std::thread::sleep(LET_GO_LOOK * 5);
        assert!(!hold.is_ended());
        assert!(writer.try_readers().unwrap().is_none());
        drop(writer);
        // The next writer withdraws it at its open.
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        assert!(!asked(&writer.letgo).unwrap());
        drop(hold);
    }

    /// Through the gate: a reader under way is waited for up to the
    /// bound, and the gate is open again either way.
    #[test]
    fn the_gate_waits_a_bounded_time_and_opens_again() {
        let (_dir, path) = temp();
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        let reader = ReaderLocks::open(&path).unwrap().unwrap();
        let hold = reader.hold().unwrap();
        let start = Instant::now();
        let bound = Duration::from_millis(30);
        assert!(writer.readers_through_gate(bound).unwrap().is_none());
        assert!(start.elapsed() >= bound);
        // Open again: a new snapshot passes.
        drop(reader.hold().unwrap());

        // A reader that lets go within the bound: the writer gets its turn.
        let let_go = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(hold);
        });
        let turn = writer
            .readers_through_gate(Duration::from_secs(10))
            .unwrap();
        assert!(turn.is_some());
        let_go.join().unwrap();
    }

    /// While the writer waits through the gate, a reader in another
    /// process doesn't begin: it waits until the gate opens again.
    #[test]
    fn no_new_reader_begins_while_the_writer_waits_at_the_gate() {
        let (_dir, path) = temp();
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        let reading = ReaderLocks::open(&path).unwrap().unwrap();
        let other = ReaderLocks::open(&path).unwrap().unwrap();
        let hold = reading.hold().unwrap();
        let bound = Duration::from_millis(300);
        let start = Instant::now();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| writer.readers_through_gate(bound).unwrap().is_none());
            std::thread::sleep(Duration::from_millis(50));
            drop(other.hold().unwrap());
            let began = start.elapsed();
            assert!(waiting.join().unwrap(), "the reader under way stayed");
            assert!(
                began >= bound,
                "began after {began:?}, inside the writer's wait"
            );
        });
        drop(hold);
    }

    /// A new snapshot waits at the gate while the writer has it shut,
    /// though its process holds the readers' lock already.
    #[test]
    fn a_new_snapshot_waits_at_a_shut_gate() {
        let (_dir, path) = temp();
        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        let reader = ReaderLocks::open(&path).unwrap().unwrap();
        let held = reader.hold().unwrap();
        writer.gate.try_lock().unwrap();
        let gate = Exclusive(Arc::clone(&writer.gate));
        let passed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiting = {
            let (reader, passed) = (Arc::clone(&reader), Arc::clone(&passed));
            std::thread::spawn(move || {
                let hold = reader.hold().unwrap();
                passed.store(true, std::sync::atomic::Ordering::SeqCst);
                drop(hold);
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!passed.load(std::sync::atomic::Ordering::SeqCst));
        drop(gate);
        waiting.join().unwrap();
        assert!(passed.load(std::sync::atomic::Ordering::SeqCst));
        drop(held);
    }

    /// A reader in a directory it may not write: the lock files a writer
    /// made are opened for reading, and lock; without them, `None`.
    #[cfg(unix)]
    #[test]
    fn a_reader_takes_part_where_it_may_not_write() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = temp();
        let set = |mode| {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap()
        };
        set(0o555);
        // Root writes anyway: nothing to test then.
        let root = std::fs::File::create(dir.path().join("probe")).is_ok();
        let none = ReaderLocks::open(&path);
        set(0o755);
        if root {
            return;
        }
        assert!(none.unwrap().is_none());

        let writer = WriterLocks::new(WriterLocks::writer(&path).unwrap(), &path).unwrap();
        for suffix in [".readers", ".gate"] {
            let file = lock_path(&path, suffix);
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        set(0o555);
        let reader = ReaderLocks::open(&path);
        set(0o755);
        let reader = Arc::new(reader.unwrap().expect("opened for reading"));
        let hold = reader.hold().unwrap();
        assert!(writer.try_readers().unwrap().is_none());
        drop(hold);
    }
}
