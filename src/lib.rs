//! trunkdb — an embedded, single-file, schema-less document database.
//! A from-scratch, learning-first take on the LiteDB idea in Rust: no
//! tables, no joins, no schema migrations.

pub mod batch;
pub mod catalog;
pub mod check;
pub mod collection;
pub mod compact;
mod crc32;
pub mod cursor;
pub mod data;
pub mod database;
pub mod document;
pub mod durability;
pub mod export;
pub mod id;
pub mod index;
pub mod json;
pub mod query;
pub mod serde_bridge;
pub mod storage;
pub mod txn;

pub use batch::Batch;
pub use catalog::IndexOptions;
pub use check::{CheckReport, FileInfo};
pub use collection::{Collection, IndexFields, Upserted};
pub use compact::Compacted;
pub use cursor::Cursor;
pub use database::{Database, OpenOptions};
pub use document::{DocId, Document};
pub use export::Summary;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Txn(#[from] txn::TxnError),
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
    #[error("collection {collection:?} has no document with id {id}")]
    NotFound {
        collection: String,
        id: document::DocId,
    },
    /// `upsert` found more than one document matching its filter, so it
    /// couldn't tell which to replace. Nothing was written.
    #[error("upsert into {collection:?}: {count} documents match the filter, expected at most one")]
    MultipleMatches { collection: String, count: usize },
    /// `Database::import` found a line it can't use. Every chunk before
    /// the one holding this line was already imported (SPEC §30.3).
    #[error("import, line {line}: {message}")]
    Import { line: usize, message: String },
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
