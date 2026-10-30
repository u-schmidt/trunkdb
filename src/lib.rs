//! trunkdb — an embedded, single-file, schema-less document database.
//!
//! Open a file, get collections of documents: no server, no tables, no
//! joins, no schema migrations. Any `T: Serialize + Deserialize` is a
//! document, and [`Document`] is the untyped one.
//!
//! ```
//! use serde::{Deserialize, Serialize};
//! use trunkdb::{Database, DocId, query::Filter};
//!
//! #[derive(Serialize, Deserialize)]
//! struct Task {
//!     #[serde(rename = "_id")]
//!     id: Option<DocId>, // None on insert: one is made; filled in on every read
//!     title: String,
//!     priority: i64,
//! }
//!
//! # fn main() -> trunkdb::Result<()> {
//! # let dir = tempfile::tempdir().unwrap();
//! # let path = dir.path().join("app.trunkdb");
//! let db = Database::open(&path)?;
//! let tasks = db.collection::<Task>("tasks");
//! tasks.ensure_index("priority")?;
//!
//! tasks.insert(Task { id: None, title: "write the docs".into(), priority: 2 })?;
//! tasks.insert(Task { id: None, title: "ship it".into(), priority: 1 })?;
//!
//! let urgent = tasks.find(Filter::new().lte("priority", 1))?;
//! assert_eq!(urgent[0].title, "ship it");
//!
//! // The same filter, as text.
//! let urgent = tasks.find("priority <= 1 order by title".parse().unwrap())?;
//! assert_eq!(urgent.len(), 1);
//! # Ok(())
//! # }
//! ```
//!
//! Where to start:
//! - [`Database`]: open a file, get a [`Collection`], start a [`Batch`]
//!   (atomic across collections) or a [`Snapshot`] (reads of one moment).
//! - [`Collection`]: insert, get, update, delete, `find`, `count`,
//!   `aggregate`, indexes.
//! - [`query`]: [`Filter`](query::Filter), [`Condition`](query::Condition),
//!   [`Update`](query::Update) and [`Aggregate`](query::Aggregate), built
//!   with methods or parsed from text.
//!
//! Every commit goes through a write-ahead log, so a crash leaves a batch
//! either entirely there or not at all. Readers run in parallel beside one
//! writer, and neither waits for the other. How it all works is in the
//! repository's `DESIGN.md`, and why in its `spec/`.

// Every public item documented (ROADMAP: a settled API and format).
#![warn(missing_docs)]

mod aggregate;
mod batch;
mod catalog;
mod changes;
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
pub mod map;
mod path;
pub mod query;
mod query_text;
mod release;
mod select;
mod serde_bridge;
mod serde_stored;
mod snapshot;
mod stats;
mod storage;
mod txn;
mod update;

pub use batch::{Batch, IntoDocument};
pub use catalog::{IndexInfo, IndexOptions};
pub use changes::{Change, Changes, Commit};
pub use check::{CheckReport, FileInfo};
pub use collection::{Collection, IndexFields, Upserted};
pub use compact::Compacted;
pub use cursor::Cursor;
pub use database::{Database, OpenOptions};
pub use datetime::{DateTime, ParseDateTimeError};
pub use document::{DocId, Document, ParseIdError};
pub use export::{ImportOptions, Summary};
pub use json::ParseDocumentError;
pub use map::Map;
pub use release::{RELEASE, Release};
pub use serde_bridge::DocumentError;
pub use snapshot::{Snapshot, SnapshotInfo, View};
pub use stats::{CollectionStats, IndexStats, PageUse, Recovered, Stats};
/// Marks a struct as a document and one of its fields as its id (SPEC
/// §114), with the `derive` feature: `#[id]` on the field becomes
/// `#[serde(rename = "_id")]` before serde's derive, written below it,
/// sees the struct.
///
/// ```
/// use serde::{Deserialize, Serialize};
/// use trunkdb::DocId;
///
/// #[trunkdb::document]
/// #[derive(Serialize, Deserialize)]
/// struct Task {
///     #[id]
///     id: Option<DocId>, // None on insert: one is made; filled in on every read
///     title: String,
/// }
/// ```
///
/// The id is a `DocId` or an `Option<DocId>`, at most one field has
/// `#[id]`, and neither attribute takes arguments; anything else is a
/// compile error. The type is checked as written, so an alias of
/// `DocId` is refused.
#[cfg(feature = "derive")]
pub use trunkdb_derive::document;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
/// Everything a trunkdb call can fail with. `#[non_exhaustive]`: a
/// `match` on it needs a `_` arm, so a new kind of failure isn't a
/// breaking change.
pub enum Error {
    #[error(transparent)]
    /// Reading or writing the file or its WAL failed. Of kind
    /// `InvalidData`, the file is damaged (SPEC §92); of kind `WouldBlock`
    /// at an open, another process has it open.
    Io(std::io::Error),
    #[error(transparent)]
    /// A value couldn't be converted between a `T` and a `Document` (SPEC
    /// §13): a type serde can't map onto a document, or a stored document
    /// that doesn't fit `T`.
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
        /// The collection.
        collection: String,
        /// The id, which a document in the collection already has.
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
        /// The collection.
        collection: String,
        /// The unique index, as `Collection::indexes` names it.
        field: String,
        /// The id of the document being written — or, from
        /// `ensure_unique_index`, being added to the new index.
        id: document::DocId,
        /// The id of the document that already has this value in
        /// `field`.
        existing: document::DocId,
    },
    /// A batch tried to update or delete a document its collection
    /// doesn't have (or the collection doesn't exist). The whole batch
    /// was rolled back.
    #[non_exhaustive]
    #[error("collection {collection:?} has no document with id {id}")]
    NotFound {
        /// The collection.
        collection: String,
        /// The id, which no document in the collection has.
        id: document::DocId,
    },
    /// `upsert` found more than one document matching its filter, so it
    /// couldn't tell which to replace. Nothing was written.
    #[non_exhaustive]
    #[error("upsert into {collection:?}: {count} documents match the filter, expected at most one")]
    MultipleMatches {
        /// The collection.
        collection: String,
        /// How many documents matched.
        count: usize,
    },
    /// `update_fields` couldn't make a change to document `id` (SPEC
    /// §68): an `inc` of something that isn't a number, or that
    /// overflows; a path through a value that isn't an object. The whole
    /// batch was rolled back.
    #[non_exhaustive]
    #[error("update of document {id} in collection {collection:?}: {message}")]
    Update {
        /// The collection.
        collection: String,
        /// The id of the document being updated.
        id: document::DocId,
        /// What couldn't be done, as people read it.
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
    /// A write gave collection `collection` a document that isn't an
    /// object: a number, a string, an array, a bool or null (SPEC §95).
    /// Every document is an object, so that it carries its id as `_id`;
    /// wrap a plain value in a one-field struct or map. The whole batch
    /// was rolled back.
    #[non_exhaustive]
    #[error(
        "collection {collection:?}: a document must be an object (a struct or a map), not a plain value or an array"
    )]
    NotAnObject {
        /// The collection written to.
        collection: String,
    },
    /// A filter's hint, `Filter::use_index` (SPEC §132), can't be
    /// followed: the collection has no index on those fields, or the
    /// index can't be read for the filter — no condition it can bound,
    /// no sort with a limit it can give the order of. Nothing was read,
    /// and a write was rolled back. The planner never drops a hint
    /// quietly, as it would have to do to read anything.
    #[non_exhaustive]
    #[error("collection {collection:?}: the hint to use index {index} can't be followed: {reason}")]
    Hint {
        /// The collection read.
        collection: String,
        /// The hinted index, as `Collection::indexes` names it: `_id`
        /// for the primary index.
        index: String,
        /// Why not, as people read it.
        reason: String,
    },
    /// `Database::import` found a line it can't use. Every chunk before
    /// the one holding this line was already imported (SPEC §30.3).
    #[non_exhaustive]
    #[error("import, line {line}: {message}")]
    Import {
        /// The line, counting from 1.
        line: usize,
        /// What's wrong with it.
        message: String,
    },
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
    Newer {
        /// The file's format.
        found: u32,
        /// This build's format, the newest it reads.
        current: u32,
    },
    /// A format older than `oldest`, the oldest this build reads as it
    /// is: export it with the build that wrote it, and import the export
    /// with this one.
    #[non_exhaustive]
    #[error(
        "file has format {found}, older than this build's {current}: export it with the trunkdb version that wrote it (`trunkdb export`), and import the export into a new file with this one"
    )]
    Older {
        /// The file's format.
        found: u32,
        /// The oldest format this build reads as it is.
        oldest: u32,
        /// This build's format.
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
    WalVersion {
        /// The WAL's version.
        found: u32,
    },
    /// A file of a format before 12 holds a document that isn't an
    /// object, which this build can't hold (SPEC §95): `id`, in
    /// `collection`, is the first one found. Open the file with trunkdb
    /// 0.19.0, delete such documents or wrap each in an object, and open
    /// it with this build again.
    #[non_exhaustive]
    #[error(
        "collection {collection:?} has a document that isn't an object ({id}), which this build can't hold: open the file with trunkdb 0.19.0, delete or wrap such documents, and open it again"
    )]
    NotAnObject {
        /// The collection holding it.
        collection: String,
        /// The id of the first such document found.
        id: document::DocId,
    },
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

/// What trunkdb's calls return: a value, or an `Error`.
pub type Result<T> = std::result::Result<T, Error>;

/// Helpers shared by tests in several modules.
#[cfg(test)]
pub(crate) mod testing {
    /// A number as a document, for tests that stored plain numbers
    /// before every document had to be an object (SPEC §95).
    #[derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        serde::Serialize,
        serde::Deserialize,
    )]
    pub struct Num {
        pub n: i64,
    }

    pub fn num(n: i64) -> Num {
        Num { n }
    }

    impl PartialEq<i64> for Num {
        fn eq(&self, other: &i64) -> bool {
            self.n == *other
        }
    }

    /// A string as a document, as `Num` is a number.
    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct Text {
        pub s: String,
    }

    pub fn text(s: impl Into<String>) -> Text {
        Text { s: s.into() }
    }

    impl PartialEq<&str> for Text {
        fn eq(&self, other: &&str) -> bool {
            self.s == *other
        }
    }

    impl PartialEq<str> for Text {
        fn eq(&self, other: &str) -> bool {
            self.s == other
        }
    }

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
