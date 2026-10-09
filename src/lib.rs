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
    /// `Database::import` found a line it can't use. Every chunk before
    /// the one holding this line was already imported (SPEC §30.3).
    #[non_exhaustive]
    #[error("import, line {line}: {message}")]
    Import { line: usize, message: String },
}

/// By hand, where the others are derived: a page read that found its
/// snapshot ended says so inside an `io::Error`, as a page read says
/// everything, and comes out as `Error::SnapshotTooOld`.
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        let too_old = error
            .get_ref()
            .is_some_and(|inner| inner.is::<storage::TooOld>());
        if too_old {
            Error::SnapshotTooOld
        } else {
            Error::Io(error)
        }
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
