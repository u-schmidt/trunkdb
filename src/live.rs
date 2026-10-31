//! Live queries (SPEC §138): `Collection::watch` keeps a `Filter` as a
//! subscription, and reports the documents that come to match it, those
//! that no longer do, and those written that still do. `Watcher` is the
//! writer's side, checked inside each commit against the documents the
//! commit changed; `LiveQuery` the application's.

use crate::changes::Change;
use crate::database::Reading;
use crate::document::{DocId, Document};
use crate::query::{Filter, Panicked};
use crate::select::Selection;
use std::collections::BTreeSet;
use std::sync::mpsc;
use std::time::Duration;

/// What a live query reports (SPEC §138): a document that came to match
/// its filter, one that no longer does, or one written that still does.
/// Each holds the document as the commit that caused it left it, cut to
/// the filter's `select`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum Live<T> {
    /// The document matches, and didn't before this commit; or it matched
    /// when `watch` was called, reported first.
    #[non_exhaustive]
    Entered {
        /// Its id.
        id: DocId,
        /// The document as it is now.
        doc: T,
    },
    /// The document matched before this commit, and no longer does.
    #[non_exhaustive]
    Left {
        /// Its id.
        id: DocId,
        /// The document as it is now; `None` if it was deleted, or its
        /// collection dropped.
        doc: Option<T>,
    },
    /// The document matched before this commit and still does, and the
    /// commit wrote it. Its contents may be the same as before.
    #[non_exhaustive]
    Changed {
        /// Its id.
        id: DocId,
        /// The document as it is now.
        doc: T,
    },
}

impl<T> Live<T> {
    /// The document reported on.
    pub fn id(&self) -> DocId {
        match self {
            Live::Entered { id, .. } | Live::Left { id, .. } | Live::Changed { id, .. } => *id,
        }
    }

    fn try_map<U, E>(self, convert: impl Fn(T) -> Result<U, E>) -> Result<Live<U>, E> {
        Ok(match self {
            Live::Entered { id, doc } => Live::Entered {
                id,
                doc: convert(doc)?,
            },
            Live::Left { id, doc } => Live::Left {
                id,
                doc: doc.map(convert).transpose()?,
            },
            Live::Changed { id, doc } => Live::Changed {
                id,
                doc: convert(doc)?,
            },
        })
    }
}

/// What a watcher sends: an event, or the error that ended it.
pub(crate) type Sent = Result<Live<Document>, crate::Error>;

/// A live query, from `Collection::watch` (SPEC §138): first an
/// `Entered` for every document that matched when it was made, then, for
/// each commit after, what came to match its filter, what no longer
/// does, and what was written that still does.
///
/// Each commit is checked as it commits, against the documents it
/// changed and the state it left them in: no change is lost between two
/// reads, however late they come. Reports wait in memory, without
/// limit, until they are read; a writer never waits for them. Dropping
/// the `LiveQuery` ends it.
///
/// An item is `Err` once, as the live query ends: a `Condition::custom`
/// that panicked as a commit was checked (`Error::ClosurePanicked`), or a
/// document that couldn't be read. For a typed collection, an `Err` is
/// also a document that doesn't convert to `T`; the query goes on after
/// that one.
///
/// ```
/// # let dir = tempfile::tempdir().unwrap();
/// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb"))?;
/// use trunkdb::{Document, Live, query::Filter};
///
/// let vans = db.collection::<Document>("vans");
/// let id = vans.insert(r#"{"name": "van-7", "zone": "road"}"#.parse()?)?;
/// let in_depot = vans.watch(Filter::new().eq("zone", "depot"))?;
///
/// vans.update_fields(Filter::new().eq("name", "van-7"),
///     &trunkdb::query::Update::new().set("zone", "depot"))?;
/// assert!(matches!(in_depot.recv(), Some(Ok(Live::Entered { id: entered, .. })) if entered == id));
///
/// vans.delete(&id)?;
/// assert!(matches!(in_depot.recv(), Some(Ok(Live::Left { doc: None, .. }))));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct LiveQuery<T> {
    receiver: mpsc::Receiver<Sent>,
    /// `Ok` for a `Document`, `from_document` for a `T`: on the reader's
    /// thread, not under the writer's lock.
    convert: fn(Document) -> crate::Result<T>,
}

impl<T> std::fmt::Debug for LiveQuery<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveQuery").finish_non_exhaustive()
    }
}

impl<T> LiveQuery<T> {
    pub(crate) fn new(
        receiver: mpsc::Receiver<Sent>,
        convert: fn(Document) -> crate::Result<T>,
    ) -> Self {
        LiveQuery { receiver, convert }
    }

    fn converted(&self, sent: Sent) -> crate::Result<Live<T>> {
        sent?.try_map(self.convert)
    }

    /// The next report, waiting for one as long as it takes. `None` once
    /// the query has ended, by an error or by the database being dropped,
    /// and every report before has been read.
    pub fn recv(&self) -> Option<crate::Result<Live<T>>> {
        let sent = self.receiver.recv().ok()?;
        Some(self.converted(sent))
    }

    /// The next report if one is waiting, without waiting.
    pub fn try_recv(&self) -> Option<crate::Result<Live<T>>> {
        let sent = self.receiver.try_recv().ok()?;
        Some(self.converted(sent))
    }

    /// The next report, waiting at most `timeout`. `None` if none came,
    /// or the query has ended.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<crate::Result<Live<T>>> {
        let sent = self.receiver.recv_timeout(timeout).ok()?;
        Some(self.converted(sent))
    }
}

/// Each report in turn, as `recv` hands them out: the loop waits for the
/// next one, and ends with the query.
impl<T> Iterator for LiveQuery<T> {
    type Item = crate::Result<Live<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv()
    }
}

/// `filter`'s refusal, if it can't be live (SPEC §138.4): an order and a
/// window move with every write.
pub(crate) fn refused(filter: &Filter) -> Option<&'static str> {
    if !filter.sort.is_empty() {
        Some("a sort")
    } else if filter.skip > 0 {
        Some("a skip")
    } else if filter.limit.is_some() {
        Some("a limit")
    } else {
        None
    }
}

/// A live query, as the writer keeps it.
pub(crate) struct Watcher {
    collection: String,
    filter: Filter,
    select: Option<Selection>,
    /// The ids of the documents that match as of the last commit.
    matching: BTreeSet<DocId>,
    sender: mpsc::Sender<Sent>,
}

impl Watcher {
    /// A live query on `collection`, whose `matching` documents were sent
    /// already, as `Entered`.
    pub(crate) fn new(
        collection: String,
        filter: Filter,
        matching: BTreeSet<DocId>,
        sender: mpsc::Sender<Sent>,
    ) -> Self {
        let select = filter.selection();
        Watcher {
            collection,
            filter,
            select,
            matching,
            sender,
        }
    }

    /// `false` once it has ended: its `LiveQuery` dropped, or an error
    /// sent.
    fn send(&self, live: Live<Document>) -> bool {
        let live = match &self.select {
            Some(select) => live
                .try_map(|doc| Ok::<_, std::convert::Infallible>(select.project(doc)))
                .unwrap_or_else(|never| match never {}),
            None => live,
        };
        self.sender.send(Ok(live)).is_ok()
    }

    /// One change of a commit to its collection, `now` the document as
    /// the commit left it. `false` once it has ended.
    fn changed(&mut self, id: DocId, now: Option<&Document>) -> Result<bool, Panicked> {
        let was = self.matching.contains(&id);
        let is = match now {
            Some(doc) => self.filter.matcher().matches(doc)?,
            None => false,
        };
        let live = match (was, is) {
            (false, false) => return Ok(true),
            (false, true) => {
                self.matching.insert(id);
                Live::Entered {
                    id,
                    doc: now.cloned().expect("it matches, so it is there"),
                }
            }
            (true, false) => {
                self.matching.remove(&id);
                Live::Left {
                    id,
                    doc: now.cloned(),
                }
            }
            (true, true) => Live::Changed {
                id,
                doc: now.cloned().expect("it matches, so it is there"),
            },
        };
        Ok(self.send(live))
    }

    /// Its collection dropped: each document that matched has left.
    fn dropped(&mut self) -> bool {
        let matching = std::mem::take(&mut self.matching);
        matching
            .into_iter()
            .all(|id| self.send(Live::Left { id, doc: None }))
    }
}

/// Checks every watcher against a commit's `changes`, `at` the state the
/// commit left (SPEC §138.2), and drops those that have ended. A commit
/// names each document once; one in a watched collection is read once,
/// however many watch it.
pub(crate) fn check(watchers: &mut Vec<Watcher>, changes: &[Change], at: &Reading<'_>) {
    for change in changes {
        let collection = change.collection();
        if !watchers.iter().any(|w| w.collection == collection) {
            continue;
        }
        let Some(id) = change.id() else {
            watchers.retain_mut(|w| w.collection != collection || w.dropped());
            continue;
        };
        let now = match change {
            Change::Deleted { .. } => None,
            _ => match crate::collection::document_at(at, collection, &id) {
                Ok(doc) => doc,
                Err(e) => {
                    // A document that can't be read leaves each watcher
                    // of its collection unsure what matches: they end.
                    // `Error` isn't `Clone`: each gets an I/O error of
                    // the same kind and message.
                    let kind = match &e {
                        crate::Error::Io(io) => io.kind(),
                        _ => std::io::ErrorKind::InvalidData,
                    };
                    let message = e.to_string();
                    watchers.retain(|w| {
                        if w.collection != collection {
                            return true;
                        }
                        let error = std::io::Error::new(kind, message.clone());
                        let _ = w.sender.send(Err(crate::Error::Io(error)));
                        false
                    });
                    continue;
                }
            },
        };
        watchers.retain_mut(|w| {
            if w.collection != collection {
                return true;
            }
            match w.changed(id, now.as_ref()) {
                Ok(going_on) => going_on,
                Err(panicked) => {
                    // Its set can't be trusted from here: it ends, the
                    // commit stands, and the others go on (§136.2).
                    let _ = w.sender.send(Err(panicked.into()));
                    false
                }
            }
        });
    }
}
