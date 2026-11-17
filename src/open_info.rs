//! What an open database says about itself (SPEC §208): the file it
//! works on, how it was opened, whether it still works, and the options
//! it was opened with. See [`Database::open_info`](crate::Database::open_info).

use std::path::PathBuf;
use std::time::Duration;

/// How a database was opened — see [`OpenInfo::mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OpenMode {
    /// The one open that writes the file (SPEC §21): its batches commit.
    Writer,
    /// Read-only, by the lock files beside the file (SPEC §187): each
    /// call reads what a writer in another process committed before it,
    /// whether one is open now or not.
    ReadOnlyBesideWriter,
    /// Read-only, holding the file shared (SPEC §88): it reads the file
    /// as it was at the open, and no writer opens it meanwhile. Where
    /// the lock files can't be made, or the file is older than format 17.
    ReadOnlyAlone,
}

impl OpenMode {
    /// Whether this open refuses writes: every mode but `Writer`.
    pub fn is_read_only(self) -> bool {
        self != OpenMode::Writer
    }

    /// The mode's name in a report (SPEC §208): `writer`,
    /// `read_only_beside_writer`, `read_only_alone`.
    pub fn name(self) -> &'static str {
        match self {
            OpenMode::Writer => "writer",
            OpenMode::ReadOnlyBesideWriter => "read_only_beside_writer",
            OpenMode::ReadOnlyAlone => "read_only_alone",
        }
    }
}

/// An open database, as it says itself — see
/// [`Database::open_info`](crate::Database::open_info).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OpenInfo {
    /// The file it works on: the path given to the open, its symlinks
    /// followed to the file they lead to (SPEC §127), as the WAL and
    /// the lock files beside it are named from.
    pub path: PathBuf,
    /// Its write-ahead log, beside it: `<path>.wal`.
    pub wal_path: PathBuf,
    /// How it was opened.
    pub mode: OpenMode,
    /// A batch was logged but couldn't be written to the file, even on
    /// retry: every call fails with `Error::Poisoned` until the database
    /// is opened again, which recovers the batch from the WAL.
    pub poisoned: bool,
    /// The options it was opened with, as it uses them.
    pub options: OpenSettings,
}

/// The options a database was opened with, as it uses them: each
/// `OpenOptions` value, or the default it took for one not set — see
/// [`OpenInfo::options`]. Each field is the `OpenOptions` method of the
/// same name.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OpenSettings {
    /// Bytes of pages kept in memory.
    pub cache_size: usize,
    /// Pages in the WAL that start a checkpoint.
    pub checkpoint_pages: usize,
    /// Bytes of WAL that start a checkpoint: the value set, or twice
    /// `checkpoint_pages`' pages.
    pub checkpoint_wal_bytes: u64,
    /// Bytes snapshots may hold of pages written since they were taken.
    pub snapshot_memory: usize,
    /// How long a checkpoint waits for readers in other processes.
    pub checkpoint_wait: Duration,
    /// How long an open that has to write the file waits for readers in
    /// other processes.
    pub wait_for_readers: Duration,
    /// Bytes of WAL past which a commit asks readers in other processes
    /// to let go (SPEC §189): the value set, or four times
    /// `checkpoint_wal_bytes`; `None` where they are never asked.
    pub let_go_wal_bytes: Option<u64>,
    /// How often a read-only open beside a writer looks for its commits
    /// (SPEC §195).
    pub watch_every: Duration,
    /// How often a live query against the clock without an index is read
    /// again (SPEC §204).
    pub clock_every: Duration,
}
