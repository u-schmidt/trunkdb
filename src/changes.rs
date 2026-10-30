//! The hook after each commit (SPEC §135): `Database::subscribe` hands
//! out a `Changes`, which receives a `Commit` for every batch committed
//! after it, with the documents the batch inserted, updated or deleted.
//! `ChangeLog` is what a batch records while it runs, the net effect on
//! each document, kept in the batch's own catalog.

use crate::document::DocId;
use crate::txn::WriteOp;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

/// What one committed batch changed (SPEC §135): each document it
/// inserted, updated or deleted, once, in the order the batch first
/// touched it, and the collections it dropped.
///
/// A document is listed with the batch's net effect: inserted and then
/// updated is `Inserted`, updated and then deleted `Deleted`, and one
/// inserted and deleted in the same batch isn't listed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Never empty: a batch that changed no document isn't reported.
    pub changes: Vec<Change>,
}

/// One document's change in a [`Commit`], or a collection dropped.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The document wasn't there before the batch, and is after it.
    #[non_exhaustive]
    Inserted {
        /// The collection it is in.
        collection: String,
        /// Its id.
        id: DocId,
    },
    /// The document was there before the batch and is after it, written
    /// in between: by an update, or deleted and inserted again. Its
    /// contents may be the same as before.
    #[non_exhaustive]
    Updated {
        /// The collection it is in.
        collection: String,
        /// Its id.
        id: DocId,
    },
    /// The document was there before the batch, and isn't after it.
    #[non_exhaustive]
    Deleted {
        /// The collection it was in.
        collection: String,
        /// Its id.
        id: DocId,
    },
    /// `Database::drop_collection`: the collection and every document in
    /// it are gone. Its documents aren't listed one by one.
    #[non_exhaustive]
    Dropped {
        /// The collection dropped.
        collection: String,
    },
}

impl Change {
    /// The collection changed.
    pub fn collection(&self) -> &str {
        match self {
            Change::Inserted { collection, .. }
            | Change::Updated { collection, .. }
            | Change::Deleted { collection, .. }
            | Change::Dropped { collection } => collection,
        }
    }

    /// The document changed; `None` for a collection dropped.
    pub fn id(&self) -> Option<DocId> {
        match self {
            Change::Inserted { id, .. }
            | Change::Updated { id, .. }
            | Change::Deleted { id, .. } => Some(*id),
            Change::Dropped { .. } => None,
        }
    }
}

/// A subscription to what each commit changes, from
/// `Database::subscribe` (SPEC §135): a [`Commit`] for every batch
/// committed after it was made, in the order they were committed.
///
/// Each report is sent once the batch is durable and new reads see it,
/// so a `get` of an id it names finds what the batch wrote, or what a
/// later batch did. A batch that failed is never reported.
///
/// Reports wait in memory, without limit, until they are read: a writer
/// never waits for a subscriber. Dropping the `Changes` ends the
/// subscription. Iterating ends when the database is dropped, its last
/// clone and every handle on it.
///
/// ```
/// # let dir = tempfile::tempdir().unwrap();
/// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb"))?;
/// use trunkdb::{Change, Document};
///
/// let changes = db.subscribe()?;
/// let tasks = db.collection::<Document>("tasks");
/// let id = tasks.insert(r#"{"title": "write the docs"}"#.parse()?)?;
///
/// let commit = changes.recv().unwrap();
/// assert!(matches!(
///     &commit.changes[..],
///     [Change::Inserted { collection, id: inserted, .. }]
///         if collection == "tasks" && *inserted == id
/// ));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Changes {
    receiver: mpsc::Receiver<Commit>,
}

impl Changes {
    pub(crate) fn new(receiver: mpsc::Receiver<Commit>) -> Self {
        Changes { receiver }
    }

    /// The next report, waiting for one as long as it takes. `None` once
    /// the database is dropped and every report before has been read.
    pub fn recv(&self) -> Option<Commit> {
        self.receiver.recv().ok()
    }

    /// The next report if one is waiting, without waiting.
    pub fn try_recv(&self) -> Option<Commit> {
        self.receiver.try_recv().ok()
    }

    /// The next report, waiting at most `timeout`. `None` if none came,
    /// or the database is dropped.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Commit> {
        self.receiver.recv_timeout(timeout).ok()
    }
}

/// Each report in turn, as `recv` hands them out: the loop waits for the
/// next commit, and ends when the database is dropped.
impl Iterator for Changes {
    type Item = Commit;

    fn next(&mut self) -> Option<Commit> {
        self.recv()
    }
}

/// What a batch has done to each document so far (SPEC §135.2), while
/// it runs: kept in the batch's catalog, so a batch that fails drops it
/// with the rest.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChangeLog {
    /// In the order first touched.
    entries: Vec<Entry>,
    /// Where each document's entry is in `entries`.
    at: HashMap<(String, DocId), usize>,
}

#[derive(Debug, Clone)]
struct Entry {
    collection: String,
    /// `None` for a collection dropped.
    id: Option<DocId>,
    /// Whether the document was there before the batch: what its first
    /// write says, as an insert fails where one is and an update or a
    /// delete where none is.
    existed: bool,
    /// Whether it is there now.
    exists: bool,
}

impl ChangeLog {
    /// `op`, which has just been applied.
    pub(crate) fn written(&mut self, op: &WriteOp) {
        let (collection, id, exists) = match op {
            WriteOp::Insert(collection, id, _) => (collection, *id, true),
            WriteOp::Update(collection, id, _) => (collection, *id, true),
            WriteOp::Delete(collection, id) => (collection, *id, false),
        };
        let key = (collection.clone(), id);
        match self.at.get(&key) {
            Some(&at) => self.entries[at].exists = exists,
            None => {
                self.at.insert(key, self.entries.len());
                self.entries.push(Entry {
                    collection: collection.clone(),
                    id: Some(id),
                    existed: !matches!(op, WriteOp::Insert(..)),
                    exists,
                });
            }
        }
    }

    /// `collection` dropped, every document in it with it.
    pub(crate) fn dropped(&mut self, collection: &str) {
        self.entries.retain(|entry| entry.collection != collection);
        self.at.clear();
        for (at, entry) in self.entries.iter().enumerate() {
            if let Some(id) = entry.id {
                self.at.insert((entry.collection.clone(), id), at);
            }
        }
        self.entries.push(Entry {
            collection: collection.to_string(),
            id: None,
            existed: true,
            exists: false,
        });
    }

    /// The batch's changes, each document once, those it left as they
    /// were before left out.
    pub(crate) fn finish(self) -> Vec<Change> {
        self.entries
            .into_iter()
            .filter_map(
                |Entry {
                     collection,
                     id,
                     existed,
                     exists,
                 }| match (id, existed, exists) {
                    (None, _, _) => Some(Change::Dropped { collection }),
                    (Some(id), false, true) => Some(Change::Inserted { collection, id }),
                    (Some(id), true, true) => Some(Change::Updated { collection, id }),
                    (Some(id), true, false) => Some(Change::Deleted { collection, id }),
                    (Some(_), false, false) => None,
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn id(n: u8) -> DocId {
        DocId([n; 16])
    }

    fn insert(c: &str, n: u8) -> WriteOp {
        WriteOp::Insert(c.into(), id(n), Document::Null)
    }

    fn update(c: &str, n: u8) -> WriteOp {
        WriteOp::Update(c.into(), id(n), Document::Null)
    }

    fn delete(c: &str, n: u8) -> WriteOp {
        WriteOp::Delete(c.into(), id(n))
    }

    fn net(ops: &[WriteOp]) -> Vec<Change> {
        let mut log = ChangeLog::default();
        for op in ops {
            log.written(op);
        }
        log.finish()
    }

    fn inserted(c: &str, n: u8) -> Change {
        Change::Inserted {
            collection: c.into(),
            id: id(n),
        }
    }

    fn updated(c: &str, n: u8) -> Change {
        Change::Updated {
            collection: c.into(),
            id: id(n),
        }
    }

    fn deleted(c: &str, n: u8) -> Change {
        Change::Deleted {
            collection: c.into(),
            id: id(n),
        }
    }

    #[test]
    fn a_batch_reports_each_documents_net_effect() {
        // Each write alone.
        assert_eq!(net(&[insert("a", 1)]), [inserted("a", 1)]);
        assert_eq!(net(&[update("a", 1)]), [updated("a", 1)]);
        assert_eq!(net(&[delete("a", 1)]), [deleted("a", 1)]);
        // Folded: what was there before and what is after.
        assert_eq!(net(&[insert("a", 1), update("a", 1)]), [inserted("a", 1)]);
        assert_eq!(net(&[update("a", 1), delete("a", 1)]), [deleted("a", 1)]);
        assert_eq!(net(&[insert("a", 1), delete("a", 1)]), []);
        assert_eq!(net(&[delete("a", 1), insert("a", 1)]), [updated("a", 1)]);
        assert_eq!(
            net(&[insert("a", 1), delete("a", 1), insert("a", 1)]),
            [inserted("a", 1)]
        );
        assert_eq!(
            net(&[
                update("a", 1),
                delete("a", 1),
                insert("a", 1),
                update("a", 1)
            ]),
            [updated("a", 1)]
        );
        // In the order first touched; the same id in two collections is
        // two documents.
        assert_eq!(
            net(&[
                insert("a", 2),
                update("b", 1),
                insert("a", 1),
                update("a", 2),
                delete("a", 1),
                delete("b", 2),
                insert("b", 3),
                delete("a", 2),
                update("a", 4),
            ]),
            [
                updated("b", 1),
                deleted("b", 2),
                inserted("b", 3),
                updated("a", 4)
            ]
        );
        assert_eq!(
            net(&[insert("a", 1), insert("b", 1)]),
            [inserted("a", 1), inserted("b", 1)]
        );
    }

    #[test]
    fn a_collection_dropped_is_one_change_and_ends_its_documents() {
        let mut log = ChangeLog::default();
        log.written(&insert("a", 1));
        log.written(&insert("b", 1));
        log.written(&update("a", 2));
        log.dropped("a");
        // Writes to other collections go on beside it.
        log.written(&update("b", 1));
        log.written(&insert("b", 2));
        assert_eq!(
            log.finish(),
            [
                inserted("b", 1),
                Change::Dropped {
                    collection: "a".into()
                },
                inserted("b", 2),
            ]
        );
    }

    #[test]
    fn a_change_names_its_collection_and_id() {
        assert_eq!(inserted("a", 1).collection(), "a");
        assert_eq!(deleted("b", 2).id(), Some(id(2)));
        let dropped = Change::Dropped {
            collection: "c".into(),
        };
        assert_eq!((dropped.collection(), dropped.id()), ("c", None));
    }
}
