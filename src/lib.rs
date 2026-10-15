//! trunkdb — an embedded, single-file, schema-less document database.
//! A from-scratch, learning-first take on the LiteDB idea in Rust: no
//! tables, no joins, no schema migrations.

mod batch;
mod catalog;
mod check;
mod collection;
mod compact;
mod crc32;
mod cursor;
mod data;
mod database;
mod datetime;
mod decode;
mod document;
mod durability;
mod explain;
mod export;
mod free_space;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing;
mod id;
mod index;
mod json;
pub mod query;
mod serde_bridge;
mod snapshot;
mod stats;
mod storage;
mod txn;
mod update;

pub use batch::{Batch, IntoDocument};
pub use catalog::{IndexInfo, IndexOptions};
pub use check::{CheckReport, FileInfo};
pub use collection::{Collection, IndexFields, Upserted};
pub use compact::Compacted;
pub use cursor::Cursor;
pub use database::{Database, OpenOptions};
pub use datetime::{DateTime, ParseDateTimeError};
pub use document::{DocId, Document, ParseIdError};
pub use export::{ImportOptions, Summary};
pub use json::ParseDocumentError;
pub use serde_bridge::DocumentError;
pub use snapshot::{Snapshot, SnapshotInfo, View};
pub use stats::{CollectionStats, IndexStats, PageUse, Recovered, Stats};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Io(std::io::Error),
    #[error(transparent)]
    Document(#[from] serde_bridge::DocumentError),
    /// A batch was durably logged but writing it to the main file failed,
    /// even after a retry, so the file may be half-written. Every call
    /// returns this until the database is reopened — which restores the
    /// batch from the write-ahead log.
    #[error(
        "the database file may be inconsistent after a failed write; reopen the database to recover it from the write-ahead log"
    )]
    Poisoned,
    /// A batch tried to insert a document whose id its collection already
    /// has. The whole batch was rolled back.
    #[non_exhaustive]
    #[error("collection {collection:?} already has a document with id {id}")]
    DuplicateId {
        collection: String,
        id: document::DocId,
    },
    /// A write would give document `id` a value in a unique index's
    /// `field` equal to document `existing`'s (SPEC §33) — or, from
    /// `ensure_unique_index`, two stored documents already share one.
    /// Null and missing values never count. The whole batch was rolled
    /// back; `ensure_unique_index` created no index.
    #[non_exhaustive]
    #[error(
        "collection {collection:?} has a unique index on {field:?}, and documents {existing} and {id} would have the same value there"
    )]
    DuplicateValue {
        collection: String,
        field: String,
        id: document::DocId,
        existing: document::DocId,
    },
    /// A batch tried to update or delete a document its collection
    /// doesn't have (or the collection doesn't exist). The whole batch
    /// was rolled back.
    #[non_exhaustive]
    #[error("collection {collection:?} has no document with id {id}")]
    NotFound {
        collection: String,
        id: document::DocId,
    },
    /// `upsert` found more than one document matching its filter, so it
    /// couldn't tell which to replace. Nothing was written.
    #[non_exhaustive]
    #[error("upsert into {collection:?}: {count} documents match the filter, expected at most one")]
    MultipleMatches { collection: String, count: usize },
    /// `update_fields` couldn't make a change to document `id` (SPEC
    /// §68): an `inc` of something that isn't a number, or that
    /// overflows; a path through a value that isn't an object. The whole
    /// batch was rolled back.
    #[non_exhaustive]
    #[error("update of document {id} in collection {collection:?}: {message}")]
    Update {
        collection: String,
        id: document::DocId,
        message: String,
    },
    /// `Database::compact` was called while a snapshot was open (SPEC
    /// §80.5): a `Snapshot`, a `View` of one, or a `Cursor`. A compaction rewrites every page, and keeping each as it
    /// was for the snapshot would be keeping the whole file in memory.
    /// Nothing was changed; compact again when the snapshot is gone.
    #[error("a snapshot is open: the database can't be compacted until it is closed")]
    SnapshotOpen,
    /// A read through a snapshot that was open for too long beside
    /// writes (SPEC §83): the pages as it had them took more memory than
    /// `OpenOptions::snapshot_memory` allows, so they were let go, the
    /// oldest snapshots' first. The snapshot is of no more use: take a new
    /// one and read again. A `Snapshot`, a `View`, a `Cursor`, or one
    /// long read such as an `export` can meet this; writes never do.
    #[error(
        "the snapshot is too old: what was written since took more memory than snapshots may keep; take a new one"
    )]
    SnapshotTooOld,
    /// A write to a database opened with `OpenOptions::read_only` (SPEC
    /// §88) — or a `checkpoint` or `compact` of one. Nothing was changed.
    #[error("the database is open read-only: it can't be written to")]
    ReadOnly,
    /// `Database::import` found a line it can't use. Every chunk before
    /// the one holding this line was already imported (SPEC §30.3).
    #[non_exhaustive]
    #[error("import, line {line}: {message}")]
    Import { line: usize, message: String },
    /// An open found a file this build can't read at all: not a trunkdb
    /// file, or one of a format it doesn't read (SPEC §92). Nothing was
    /// changed. Damage is something else: `Io` of kind `InvalidData`.
    #[error(transparent)]
    Format(FormatError),
}

/// Why an open can't read a file at all (SPEC §92), as
/// `Error::Format`. The message is the one people see; the variant is
/// for programs, which shouldn't have to read it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FormatError {
    /// The file doesn't start with trunkdb's magic: some other
    /// program's file.
    #[error("not a trunkdb file (bad magic)")]
    NotTrunkdb,
    /// Format 0: written before format versioning, by trunkdb 0.1.0 or
    /// an early development build.
    #[error(
        "file was written before format versioning (trunkdb 0.1.0 or an early development build), which this build can't read"
    )]
    Unversioned,
    /// A format newer than this build's `current`: open it with a newer
    /// trunkdb.
    #[non_exhaustive]
    #[error(
        "file has format {found}, newer than this build's {current} — open it with a newer trunkdb"
    )]
    Newer { found: u32, current: u32 },
    /// A format older than `oldest`, the oldest this build reads as it
    /// is: export it with the build that wrote it, and import the export
    /// with this one.
    #[non_exhaustive]
    #[error(
        "file has format {found}, older than this build's {current}: export it with the trunkdb version that wrote it (`trunkdb export`), and import the export into a new file with this one"
    )]
    Older {
        found: u32,
        oldest: u32,
        current: u32,
    },
    /// The WAL beside the file doesn't start with the WAL's magic — one
    /// from before the page-image WAL (SPEC §19), or another program's.
    #[error("not a trunkdb page-image WAL (bad magic) — possibly one written by an older trunkdb")]
    WalNotTrunkdb,
    /// The WAL has a version this build doesn't read: one a crash left
    /// under a newer build, say.
    #[non_exhaustive]
    #[error(
        "unsupported WAL version {found}, this build reads {}",
        durability::readable_wal_versions()
    )]
    WalVersion { found: u32 },
}

impl FormatError {
    /// As an `io::Error`, which is how an open carries it until
    /// `From<io::Error> for Error` takes it out again.
    pub(crate) fn into_io(self) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, self)
    }
}

/// By hand, where the others are derived: a page read that found its
/// snapshot ended says so inside an `io::Error`, as a page read says
/// everything, and comes out as `Error::SnapshotTooOld`; an open that
/// can't read the file at all says so the same way, and comes out as
/// `Error::Format` (SPEC §92).
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        let inner = error.get_ref();
        if inner.is_some_and(|inner| inner.is::<storage::TooOld>()) {
            return Error::SnapshotTooOld;
        }
        if inner.is_some_and(|inner| inner.is::<FormatError>()) {
            let format = error
                .into_inner()
                .and_then(|inner| inner.downcast::<FormatError>().ok())
                .expect("checked above");
            return Error::Format(*format);
        }
        Error::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Helpers shared by tests in several modules.
#[cfg(test)]
pub(crate) mod testing {
    /// A tiny deterministic random number generator (xorshift) — enough
    /// to shuffle test data without a `rand` dependency.
    pub struct XorShift(pub u64);

    impl XorShift {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// A number in `0..n`.
        pub fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }
}
