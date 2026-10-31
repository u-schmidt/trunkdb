//! Live queries (SPEC §138): `Collection::watch` keeps a `Filter` as a
//! subscription, and reports the documents that come to match it, those
//! that no longer do, and those written that still do. `Watcher` is the
//! writer's side, checked inside each commit against the documents the
//! commit changed; `LiveQuery` the application's. Live aggregates
//! (SPEC §139) are kept beside them: `AggregateWatcher`, run again whole
//! after each commit to their collection, and `LiveAggregate`.

use crate::aggregate::Aggregate;
use crate::changes::Change;
use crate::collection::aggregate_in;
use crate::database::Reading;
use crate::document::{DocId, Document, encode_document};
use crate::explain::Trace;
use crate::query::{Filter, Panicked, canonical};
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

/// What a live aggregate reports (SPEC §139): a row that came to be in
/// its result, one that no longer is, or one whose values changed. A row
/// is known by its group values, as `group_by` names them, which each
/// event holds as `group`; without `group_by` there is one row, always
/// there, and its `group` is `{}`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum LiveRow {
    /// The row is in the result, and wasn't before this commit; or it
    /// was when `watch_aggregate` was called, reported first.
    #[non_exhaustive]
    Entered {
        /// Its group values, at their paths as in the row.
        group: Document,
        /// The row as it is now.
        row: Document,
    },
    /// The row was in the result before this commit, and no longer is.
    #[non_exhaustive]
    Left {
        /// Its group values, at their paths as in the row.
        group: Document,
        /// The row as it is now, where its group still has documents but
        /// the row fails the `having`, or the row sort's `skip` or `limit`
        /// leave it out; `None` where no document of the group is left.
        row: Option<Document>,
    },
    /// The row was in the result before this commit and still is, with
    /// other values.
    #[non_exhaustive]
    Changed {
        /// Its group values, at their paths as in the row.
        group: Document,
        /// The row as it is now.
        row: Document,
    },
}

impl LiveRow {
    /// The group values of the row reported on: `{}` without
    /// `group_by`.
    pub fn group(&self) -> &Document {
        match self {
            LiveRow::Entered { group, .. }
            | LiveRow::Left { group, .. }
            | LiveRow::Changed { group, .. } => group,
        }
    }
}

/// What an aggregate watcher sends: an event, or the error that ended it.
type SentRow = Result<LiveRow, crate::Error>;

/// A live aggregate, from `Collection::watch_aggregate` (SPEC §139):
/// first an `Entered` for every row of the aggregate's result, then, for
/// each commit that changes the collection, the rows that came into the
/// result, those that left it, and those whose values changed.
///
/// The aggregate runs again, whole, as each such commit commits, on the
/// state it left: no crossing of a `having` is lost between two reads.
/// Reports wait in memory, without limit, until they are read; a writer
/// never waits for them. Dropping the `LiveAggregate` ends it.
///
/// An item is `Err` once, as it ends: a `Condition::custom` that
/// panicked as the aggregate ran (`Error::ClosurePanicked`), or a
/// document that couldn't be read.
///
/// ```
/// # let dir = tempfile::tempdir().unwrap();
/// # let db = trunkdb::Database::open(dir.path().join("app.trunkdb"))?;
/// use trunkdb::{Document, LiveRow, query::{Aggregate, Condition, Filter}};
///
/// let readings = db.collection::<Document>("readings");
/// let busy = Aggregate::new()
///     .group_by("server")
///     .avg("load").named("mean")
///     .having(Condition::gt("mean", 0.8));
/// let alerts = readings.watch_aggregate(Filter::new(), &busy)?;
///
/// readings.insert(r#"{"server": "a", "load": 0.9}"#.parse()?)?;
/// assert!(matches!(alerts.recv(), Some(Ok(LiveRow::Entered { .. }))));
/// readings.insert(r#"{"server": "a", "load": 0.1}"#.parse()?)?;
/// assert!(matches!(alerts.recv(), Some(Ok(LiveRow::Left { row: Some(_), .. }))));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct LiveAggregate {
    receiver: mpsc::Receiver<SentRow>,
}

impl LiveAggregate {
    /// The next report, waiting for one as long as it takes. `None` once
    /// it has ended, by an error or by the database being dropped, and
    /// every report before has been read.
    pub fn recv(&self) -> Option<crate::Result<LiveRow>> {
        self.receiver.recv().ok()
    }

    /// The next report if one is waiting, without waiting.
    pub fn try_recv(&self) -> Option<crate::Result<LiveRow>> {
        self.receiver.try_recv().ok()
    }

    /// The next report, waiting at most `timeout`. `None` if none came,
    /// or it has ended.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<crate::Result<LiveRow>> {
        self.receiver.recv_timeout(timeout).ok()
    }
}

/// Each report in turn, as `recv` hands them out.
impl Iterator for LiveAggregate {
    type Item = crate::Result<LiveRow>;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv()
    }
}

/// A row's group values, at their paths: `LiveRow`'s `group`.
fn group_of(aggregate: &Aggregate, row: &Document) -> Document {
    Selection::new(aggregate.group_by.iter().map(String::as_str)).project(row.clone())
}

/// A row's key: its group values, canonical, so `5` and `5.0`, one
/// group, are one row (§107.2).
fn row_key(aggregate: &Aggregate, row: &Document) -> Vec<u8> {
    let values = (aggregate.group_by.iter())
        .map(|path| canonical(row.get(path).unwrap_or(&Document::Null)))
        .collect();
    encode_document(&Document::Array(values))
}

/// Equal, a float to the bit: a mean that moved by its last bit has
/// changed, and NaN is NaN. An object's fields in any order, as objects
/// compare by their contents (§124): a group's object shown with its
/// fields in another order, as another document came first, is the
/// same.
fn same(a: &Document, b: &Document) -> bool {
    match (a, b) {
        (Document::Object(x), Document::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        (Document::Array(x), Document::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same(v, w))
        }
        (Document::Float(x), Document::Float(y)) => x.to_bits() == y.to_bits(),
        (x, y) => x == y,
    }
}

/// The aggregate's rows as of `at`: every group's, before `having`, the
/// row sort, skip and limit, and those they leave, the result.
fn rows_at(
    at: &Reading<'_>,
    collection: &str,
    filter: &Filter,
    aggregate: &Aggregate,
) -> crate::Result<(Vec<Document>, Vec<Document>)> {
    let every = Aggregate {
        rows: Filter::new(),
        ..aggregate.clone()
    };
    let trace = Trace::default();
    let (all, _plan) = aggregate_in(at.catalog, &at.store, collection, filter, &every, &trace)?;
    let result = aggregate.rows.apply(all.clone());
    Ok((all, result))
}

/// A live aggregate, as the writer keeps it.
pub(crate) struct AggregateWatcher {
    collection: String,
    filter: Filter,
    aggregate: Aggregate,
    /// The result as of the last commit, each row with its key.
    rows: Vec<(Vec<u8>, Document)>,
    sender: mpsc::Sender<SentRow>,
}

impl AggregateWatcher {
    /// A live aggregate of `filter` on `collection`, its first result as
    /// of `at` sent already, as `Entered`.
    pub(crate) fn start(
        at: &Reading<'_>,
        collection: &str,
        filter: Filter,
        aggregate: Aggregate,
    ) -> crate::Result<(Self, LiveAggregate)> {
        aggregate.check()?;
        let (_all, result) = rows_at(at, collection, &filter, &aggregate)?;
        let (sender, receiver) = mpsc::channel();
        let mut rows = Vec::with_capacity(result.len());
        for row in result {
            rows.push((row_key(&aggregate, &row), row.clone()));
            // The receiver is ours still: the send can't fail.
            let group = group_of(&aggregate, &row);
            let _ = sender.send(Ok(LiveRow::Entered { group, row }));
        }
        let watcher = AggregateWatcher {
            collection: collection.to_string(),
            filter,
            aggregate,
            rows,
            sender,
        };
        Ok((watcher, LiveAggregate { receiver }))
    }

    /// Runs the aggregate again on `at` and reports how its result
    /// changed: the rows that left it first, in their order before, then
    /// those that came in or changed, in their order now. `false` once it
    /// has ended.
    fn rerun(&mut self, at: &Reading<'_>) -> bool {
        let (all, result) = match rows_at(at, &self.collection, &self.filter, &self.aggregate) {
            Ok(rows) => rows,
            Err(e) => {
                let _ = self.sender.send(Err(e));
                return false;
            }
        };
        let keyed = |rows: Vec<Document>| -> Vec<(Vec<u8>, Document)> {
            (rows.into_iter())
                .map(|row| (row_key(&self.aggregate, &row), row))
                .collect()
        };
        let now = keyed(result);
        let mut all = keyed(all);
        let mut events = Vec::new();
        let group = |row: &Document| group_of(&self.aggregate, row);
        for (key, was) in &self.rows {
            if !now.iter().any(|(k, _)| k == key) {
                let row = (all.iter().position(|(k, _)| k == key)).map(|i| all.swap_remove(i).1);
                // Its group as the row before showed it: there may be
                // none now.
                let group = group(was);
                events.push(LiveRow::Left { group, row });
            }
        }
        for (key, row) in &now {
            let event = match self.rows.iter().find(|(k, _)| k == key) {
                None => LiveRow::Entered {
                    group: group(row),
                    row: row.clone(),
                },
                Some((_, was)) if !same(was, row) => LiveRow::Changed {
                    group: group(row),
                    row: row.clone(),
                },
                Some(_) => continue,
            };
            events.push(event);
        }
        self.rows = now;
        events
            .into_iter()
            .all(|event| self.sender.send(Ok(event)).is_ok())
    }
}

/// Runs every live aggregate on a collection `changes` changed again, on
/// `at`, the state the commit left (SPEC §139.1), and drops those that
/// have ended.
pub(crate) fn check_aggregates(
    watchers: &mut Vec<AggregateWatcher>,
    changes: &[Change],
    at: &Reading<'_>,
) {
    watchers.retain_mut(|w| !changes.iter().any(|c| c.collection() == w.collection) || w.rerun(at));
}
