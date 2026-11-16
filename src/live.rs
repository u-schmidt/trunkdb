//! Live queries (SPEC §138): `Collection::watch` keeps a `Filter` as a
//! subscription, and reports the documents that come to match it, those
//! that no longer do, and those written that still do. `Watcher` is the
//! writer's side, checked inside each commit against the documents the
//! commit changed; `LiveQuery` the application's. With a sort, and a
//! limit or none, a watcher keeps a window of the first matches in
//! order (SPEC §145). Live aggregates
//! (SPEC §139) are kept beside them: `AggregateWatcher`, run again whole
//! after each commit to their collection, and `LiveAggregate`.

use crate::aggregate::Aggregate;
use crate::changes::{Change, Inbox};

use crate::database::Reading;
use crate::document::{DocId, Document, encode_document};
use crate::explain::Trace;
use crate::query::{Filter, Panicked, SortOrder, value_or_null};
use crate::select::Selection;
use crate::values::{canonical, sort_order};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::time::Duration;

/// What a live query reports (SPEC §138): a document that came to match
/// its filter, one that no longer does, or one written that still does.
/// Each holds the document as the commit that caused it left it, cut to
/// the filter's `select`.
///
/// With a sort (SPEC §145) the live query is a list in the sort's order,
/// at most `limit` long, and each event says where in it: `Left`'s
/// `from`, `Entered`'s `at`, `Changed`'s both. They are places in the
/// list as the events before left it, so a list kept by applying each
/// event in turn (take out at `from`, put in at `at`) is the query's.
/// Without a sort they are `None`.
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
        /// With a sort, its place in the list: put in there.
        at: Option<usize>,
    },
    /// The document matched before this commit, and no longer does; or,
    /// with a limit, it still does, and others came before it.
    #[non_exhaustive]
    Left {
        /// Its id.
        id: DocId,
        /// The document as it is now; `None` if it was deleted, or its
        /// collection dropped.
        doc: Option<T>,
        /// With a sort, the place in the list it had: taken out there.
        from: Option<usize>,
    },
    /// The document matched before this commit and still does, and the
    /// commit wrote it. Its contents may be the same as before.
    #[non_exhaustive]
    Changed {
        /// Its id.
        id: DocId,
        /// The document as it is now.
        doc: T,
        /// With a sort, the place in the list it had: taken out there.
        from: Option<usize>,
        /// With a sort, its place now: put in there, once taken out.
        at: Option<usize>,
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
            Live::Entered { id, doc, at } => Live::Entered {
                id,
                doc: convert(doc)?,
                at,
            },
            Live::Left { id, doc, from } => Live::Left {
                id,
                doc: doc.map(convert).transpose()?,
                from,
            },
            Live::Changed { id, doc, from, at } => Live::Changed {
                id,
                doc: convert(doc)?,
                from,
                at,
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
/// the `LiveQuery` ends it. In a read-only open beside a writer in
/// another process, it is read again as that writer commits instead,
/// several commits as one (SPEC §195): see `Collection::watch`.
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
    inbox: Inbox<Sent>,
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
        LiveQuery {
            inbox: Inbox::new(receiver),
            convert,
        }
    }

    fn converted(&self, sent: Sent) -> crate::Result<Live<T>> {
        sent?.try_map(self.convert)
    }

    /// The next report, waiting for one as long as it takes. `None` once
    /// the query has ended, by an error or by the database being dropped,
    /// and every report before has been read.
    pub fn recv(&self) -> Option<crate::Result<Live<T>>> {
        let sent = self.inbox.recv()?;
        Some(self.converted(sent))
    }

    /// The next report if one is waiting, without waiting.
    pub fn try_recv(&self) -> Option<crate::Result<Live<T>>> {
        let sent = self.inbox.try_recv()?;
        Some(self.converted(sent))
    }

    /// The next report, waiting at most `timeout`. `None` if none came,
    /// or the query has ended; [`has_ended`](Self::has_ended) tells
    /// which.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<crate::Result<Live<T>>> {
        let sent = self.inbox.recv_timeout(timeout)?;
        Some(self.converted(sent))
    }

    /// Whether the query has ended, every report read (SPEC §194): true
    /// once a `recv`, `try_recv` or `recv_timeout` returned `None`
    /// because it ended, by an error or by the database being dropped,
    /// not because none had come yet.
    pub fn has_ended(&self) -> bool {
        self.inbox.has_ended()
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

/// `filter`'s refusal, if it can't be live (SPEC §138.4, §145): a skip
/// moves with every write before the list, and a limit without a sort
/// has no order to keep.
pub(crate) fn refused(filter: &Filter) -> Option<&'static str> {
    if filter.skip > 0 {
        Some("a skip")
    } else if filter.limit.is_some() && !filter.sorted() {
        Some("a limit without a sort")
    } else {
        None
    }
}

/// The filter `watch` reads its first state with: with a sort, whole
/// documents, for their sort values; the `select` cuts them as they are
/// sent.
pub(crate) fn first_read(filter: &Filter) -> Filter {
    match !filter.sorted() {
        true => filter.clone(),
        false => Filter {
            select: None,
            except: Vec::new(),
            ..filter.clone()
        },
    }
}

/// A document's place in a sorted live query (SPEC §145): its sort
/// values, then its id, as `find` orders equal ones. With a key of the
/// application's (§154), the key first, as a number.
#[derive(Debug, Clone)]
struct Member {
    key: Vec<Document>,
    id: DocId,
}

impl Member {
    /// `None` where `filter`'s key leaves `doc` out, as `find` does.
    fn of(filter: &Filter, id: DocId, doc: &Document) -> Result<Option<Self>, Panicked> {
        if let Some(ranking) = filter.ranking() {
            let rank = ranking.rank(doc)?;
            return Ok(rank.map(|rank| Member {
                key: rank.into_values(),
                id,
            }));
        }
        // A decoded document holds its `_id`, as `find` sorts it.
        let key = (filter.sort.iter())
            .map(|key| value_or_null(doc, &key.field).clone())
            .collect();
        Ok(Some(Member { key, id }))
    }
}

/// Which way each of a sorted live query's values runs: the key's
/// first, where there is one (SPEC §154), then the fields'.
fn orders(filter: &Filter) -> Vec<SortOrder> {
    (filter.by_key.iter().map(|by_key| by_key.order()))
        .chain(filter.sort.iter().map(|key| key.order))
        .collect()
}

/// `find`'s order: by the sort values, then by id, so a total one.
fn order(orders: &[SortOrder], a: &Member, b: &Member) -> Ordering {
    (orders.iter().zip(a.key.iter().zip(&b.key)))
        .map(|(order, (a, b))| sort_order(a, b, *order))
        .find(|ord| ord.is_ne())
        .unwrap_or_else(|| a.id.cmp(&b.id))
}

/// What a watcher keeps between commits.
enum Kept {
    /// Without a sort: the ids of the documents that match as of the
    /// last commit (SPEC §138.3).
    Matching(BTreeSet<DocId>),
    /// With a sort (SPEC §145).
    Window(Window),
}

/// A sorted live query's list as of the last commit, and the changes of
/// the commit being checked, settled once it is all seen.
struct Window {
    /// The first `limit` matches, or every one without a limit, in order.
    members: Vec<Member>,
    limit: Option<usize>,
    /// This commit's documents of the collection, as it left them.
    pending: Vec<(DocId, Option<Document>)>,
}

impl Window {
    /// Whether matches may be left out: the list is as long as the
    /// limit. Where it isn't, every match is in it.
    fn full(&self) -> bool {
        self.limit
            .is_some_and(|limit| limit > 0 && self.members.len() >= limit)
    }
}

/// A live query, as the writer keeps it.
pub(crate) struct Watcher {
    collection: String,
    filter: Filter,
    select: Option<Selection>,
    kept: Kept,
    sender: mpsc::Sender<Sent>,
    /// Beside a writer in another process (SPEC §195): what it read last,
    /// each match's id and its document encoded, to tell from the next
    /// read which changed. `None` in the writer's own process, where each
    /// commit says.
    seen: Option<Seen>,
}

/// A live query's matches beside a writer (SPEC §195), as `whole_read`
/// read them: each id with its document, encoded.
pub(crate) type Seen = BTreeMap<DocId, Vec<u8>>;

/// `found`, as a live query beside a writer keeps it to compare with.
pub(crate) fn seen(found: &[(DocId, Document)]) -> Seen {
    (found.iter())
        .map(|(id, doc)| (*id, encode_document(doc)))
        .collect()
}

/// What a live query beside a writer read again (SPEC §195): its
/// matches, and each document that changed since the last read, as it is
/// now, `None` where it is gone.
pub(crate) struct Reread {
    seen: Seen,
    changed: Vec<(DocId, Option<Document>)>,
    /// A sorted one's new list, as read: what settling it would read
    /// again, read with the rest, so that nothing after the reads can
    /// fail as the snapshot is let go (§189).
    list: Option<Vec<(Member, Option<Document>)>>,
}

impl Watcher {
    /// A live query on `collection`, starting from what matches `now`,
    /// as `first_read` read it, in `find`'s order: each sent as
    /// `Entered`.
    pub(crate) fn start(
        collection: String,
        filter: Filter,
        now: Vec<(DocId, Document)>,
        sender: mpsc::Sender<Sent>,
    ) -> crate::Result<Self> {
        let select = filter.selection();
        let (kept, entered): (Kept, Vec<Live<Document>>) = match !filter.sorted() {
            true => {
                let matching = now.iter().map(|(id, _)| *id).collect();
                let entered = (now.into_iter())
                    .map(|(id, doc)| Live::Entered { id, doc, at: None })
                    .collect();
                (Kept::Matching(matching), entered)
            }
            false => {
                // As `find` found them; a key that now leaves one out,
                // as a closure may, leaves it out of the list too.
                let mut members = Vec::new();
                let mut found = Vec::new();
                for (id, doc) in now {
                    if let Some(member) = Member::of(&filter, id, &doc)? {
                        members.push(member);
                        found.push((id, doc));
                    }
                }
                let entered = (found.into_iter().enumerate())
                    .map(|(at, (id, doc))| Live::Entered {
                        id,
                        doc,
                        at: Some(at),
                    })
                    .collect();
                let window = Window {
                    members,
                    limit: filter.limit,
                    pending: Vec::new(),
                };
                (Kept::Window(window), entered)
            }
        };
        let watcher = Watcher {
            collection,
            filter,
            select,
            kept,
            sender,
            seen: None,
        };
        for live in entered {
            // The receiver is ours still: the send can't fail.
            let _ = watcher.send(live);
        }
        Ok(watcher)
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

    /// Kept by a read-only open beside a writer (SPEC §195), `seen` what
    /// its first read found.
    pub(crate) fn polled(mut self, seen: Seen) -> Self {
        self.seen = Some(seen);
        self
    }

    /// Its matches read again on `at`, beside a writer (SPEC §195), and
    /// those that changed since the last read: a match whose document is
    /// not as it was, or new; and one that matched and doesn't, as it is
    /// now. A document that matches neither then nor now doesn't matter
    /// to it.
    fn reread(&self, at: &Reading<'_>) -> crate::Result<Reread> {
        let before = self
            .seen
            .as_ref()
            .expect("only one kept beside a writer is read again");
        let found = crate::collection::find_at(at, &self.collection, &whole_read(&self.filter))?;
        let list = match &self.kept {
            Kept::Matching(_) => None,
            Kept::Window(_) => {
                let mut list = Vec::new();
                for (id, doc) in &found {
                    if let Some(member) = Member::of(&self.filter, *id, doc)? {
                        list.push((member, Some(doc.clone())));
                    }
                }
                Some(list)
            }
        };
        let mut seen = Seen::new();
        let mut changed = Vec::new();
        for (id, doc) in found {
            let encoded = encode_document(&doc);
            if before.get(&id) != Some(&encoded) {
                changed.push((id, Some(doc)));
            }
            seen.insert(id, encoded);
        }
        for id in before.keys().filter(|id| !seen.contains_key(id)) {
            changed.push((
                *id,
                crate::collection::document_at(at, &self.collection, id)?,
            ));
        }
        Ok(Reread {
            seen,
            changed,
            list,
        })
    }

    /// What `reread` found, reported as if one commit had written each
    /// changed document: the steps a commit's would be. `false` once it
    /// has ended.
    fn reread_into(&mut self, reread: crate::Result<Reread>, at: &Reading<'_>) -> bool {
        let reread = match reread {
            Ok(reread) => reread,
            Err(e) => {
                let _ = self.sender.send(Err(e));
                return false;
            }
        };
        for (id, now) in &reread.changed {
            match self.changed(*id, now.as_ref()) {
                Ok(true) => {}
                Ok(false) => return false,
                Err(panicked) => {
                    let _ = self.sender.send(Err(panicked.into()));
                    return false;
                }
            }
        }
        self.seen = Some(reread.seen);
        self.settle(at, reread.list)
    }

    /// One change of a commit to its collection, `now` the document as
    /// the commit left it. `false` once it has ended. A sorted one only
    /// takes note, and `settle` reports.
    fn changed(&mut self, id: DocId, now: Option<&Document>) -> Result<bool, Panicked> {
        let matching = match &mut self.kept {
            Kept::Matching(matching) => matching,
            Kept::Window(window) => {
                window.pending.push((id, now.cloned()));
                return Ok(true);
            }
        };
        let was = matching.contains(&id);
        let is = match now {
            Some(doc) => self.filter.matcher().matches(doc)?,
            None => false,
        };
        let live = match (was, is) {
            (false, false) => return Ok(true),
            (false, true) => {
                matching.insert(id);
                Live::Entered {
                    id,
                    doc: now.cloned().expect("it matches, so it is there"),
                    at: None,
                }
            }
            (true, false) => {
                matching.remove(&id);
                Live::Left {
                    id,
                    doc: now.cloned(),
                    from: None,
                }
            }
            (true, true) => Live::Changed {
                id,
                doc: now.cloned().expect("it matches, so it is there"),
                from: None,
                at: None,
            },
        };
        Ok(self.send(live))
    }

    /// Its collection dropped: each document that matched has left. A drop
    /// commits alone, so nothing is pending.
    fn dropped(&mut self) -> bool {
        let left: Vec<Live<Document>> = match &mut self.kept {
            Kept::Matching(matching) => (std::mem::take(matching).into_iter())
                .map(|id| Live::Left {
                    id,
                    doc: None,
                    from: None,
                })
                .collect(),
            Kept::Window(window) => (std::mem::take(&mut window.members).into_iter())
                .map(|member| Live::Left {
                    id: member.id,
                    doc: None,
                    from: Some(0),
                })
                .collect(),
        };
        left.into_iter().all(|live| self.send(live))
    }

    /// A sorted one, once the whole commit is seen (SPEC §145.2): its list
    /// made anew, and the steps from the old one sent. `false` once it has
    /// ended.
    ///
    /// `list`, where it is known, is the new list, read as `read_again`
    /// would read it: beside a writer (SPEC §195).
    fn settle(&mut self, at: &Reading<'_>, list: Option<Vec<(Member, Option<Document>)>>) -> bool {
        let Kept::Window(window) = &mut self.kept else {
            return true;
        };
        if window.pending.is_empty() {
            return true;
        }
        let pending: BTreeMap<DocId, Option<Document>> =
            std::mem::take(&mut window.pending).into_iter().collect();
        match self.reorder(at, &pending, list) {
            Ok(events) => events.into_iter().all(|live| self.send(live)),
            Err(e) => {
                // A panic, or a document that couldn't be read: the list
                // can't be trusted from here.
                let _ = self.sender.send(Err(e));
                false
            }
        }
    }

    /// The list after this commit, worked out from the old one and the
    /// documents the commit wrote, `pending`; read again where they can't
    /// tell (SPEC §145.2).
    fn new_list(
        &self,
        at: &Reading<'_>,
        pending: &BTreeMap<DocId, Option<Document>>,
    ) -> crate::Result<Vec<(Member, Option<Document>)>> {
        let Kept::Window(window) = &self.kept else {
            unreachable!("only a sorted live query settles");
        };
        let sort = &orders(&self.filter);
        let matcher = self.filter.matcher();
        // What is known to match now: the list's documents not written,
        // as they were, and the written ones that match.
        let mut known: Vec<(Member, Option<Document>)> = (window.members.iter())
            .filter(|member| !pending.contains_key(&member.id))
            .map(|member| (member.clone(), None))
            .collect();
        for (id, now) in pending {
            if let Some(doc) = now
                && matcher.matches(doc)?
                && let Some(member) = Member::of(&self.filter, *id, doc)?
            {
                known.push((member, Some(doc.clone())));
            }
        }
        known.sort_by(|(a, _), (b, _)| order(sort, a, b));
        Ok(match (window.limit, window.members.last()) {
            (Some(limit), Some(last)) if window.full() => {
                // A match left out of a full list comes after its last,
                // unwritten: what is known up to there is the list's
                // start. Short of the limit, the rest is read again.
                let certain: Vec<_> = (known.into_iter())
                    .take_while(|(member, _)| order(sort, member, last).is_le())
                    .collect();
                match certain.len() >= limit {
                    true => certain.into_iter().take(limit).collect(),
                    false => read_again(at, &self.collection, &self.filter)?,
                }
            }
            (Some(limit), _) => known.into_iter().take(limit).collect(),
            (None, _) => known,
        })
    }

    /// The list after this commit, `pending` the documents it wrote, and
    /// the events that take the old list to it: those that left first,
    /// then those written or come in, in their new order. `list`, where
    /// it is known, is the new list, read already (SPEC §195).
    fn reorder(
        &mut self,
        at: &Reading<'_>,
        pending: &BTreeMap<DocId, Option<Document>>,
        list: Option<Vec<(Member, Option<Document>)>>,
    ) -> crate::Result<Vec<Live<Document>>> {
        let new = match list {
            Some(list) => list,
            None => self.new_list(at, pending)?,
        };
        let sort = &orders(&self.filter);
        let Kept::Window(window) = &mut self.kept else {
            unreachable!("only a sorted live query settles");
        };

        let staying: BTreeSet<DocId> = new.iter().map(|(member, _)| member.id).collect();
        let list = &mut window.members;
        let was: BTreeSet<DocId> = list.iter().map(|member| member.id).collect();
        let mut events = Vec::new();
        let mut i = 0;
        while i < list.len() {
            let id = list[i].id;
            if staying.contains(&id) {
                i += 1;
                continue;
            }
            list.remove(i);
            let doc = match pending.get(&id) {
                Some(now) => now.clone(),
                // Unwritten, and others came before it.
                None => crate::collection::document_at(at, &self.collection, &id)?,
            };
            events.push(Live::Left {
                id,
                doc,
                from: Some(i),
            });
        }
        // `list` stays in order by what each member holds, a written one
        // its old place until it is moved: each step a place in it.
        for (member, doc) in new {
            let id = member.id;
            let place = |list: &[Member]| {
                list.partition_point(|other| order(sort, other, &member) == Ordering::Less)
            };
            if was.contains(&id) {
                if !pending.contains_key(&id) {
                    continue;
                }
                let from = (list.iter())
                    .position(|other| other.id == id)
                    .expect("it stayed");
                list.remove(from);
                let to = place(list);
                list.insert(to, member);
                let doc = doc.expect("written, so read");
                events.push(Live::Changed {
                    id,
                    doc,
                    from: Some(from),
                    at: Some(to),
                });
            } else {
                let to = place(list);
                list.insert(to, member);
                let doc = doc.expect("come in, so read");
                events.push(Live::Entered {
                    id,
                    doc,
                    at: Some(to),
                });
            }
        }
        debug_assert!(
            list.windows(2).all(|w| order(sort, &w[0], &w[1]).is_lt()),
            "the steps leave the list in order"
        );
        Ok(events)
    }
}

/// What a live query reads to know its matches whole: `filter`'s
/// conditions, sort and limit. Neither its `select` nor its hint: the
/// sort values are wanted, and the hint steered the first read only.
pub(crate) fn whole_read(filter: &Filter) -> Filter {
    Filter {
        conditions: filter.conditions.clone(),
        sort: filter.sort.clone(),
        by_key: filter.by_key.clone(),
        limit: filter.limit,
        ..Filter::new()
    }
}

/// The first `limit` matches of `filter` on `collection` as of `at`,
/// read again: where a full list lost one to a document no commit wrote.
fn read_again(
    at: &Reading<'_>,
    collection: &str,
    filter: &Filter,
) -> crate::Result<Vec<(Member, Option<Document>)>> {
    let filter = whole_read(filter);
    // In `find`'s order, which is `order`'s; a key that now leaves one
    // out, as a closure may, leaves it out of the list.
    let found = crate::collection::find_at(at, collection, &filter)?;
    let mut members = Vec::new();
    for (id, doc) in found {
        if let Some(member) = Member::of(&filter, id, &doc)? {
            members.push((member, Some(doc)));
        }
    }
    Ok(members)
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
    // A sorted one reports once it has seen the whole commit (§145.2).
    watchers.retain_mut(|w| w.settle(at, None));
}

/// What a live aggregate reports (SPEC §139): a row that came to be in
/// its result, one that no longer is, or one whose values changed. A row
/// is known by its group values, as `group_by` names them, which each
/// event holds as `group`; without `group_by` there is one row, always
/// there, and its `group` is `{}`.
///
/// The row is a `Document`, or an `R` of the application's from
/// `watch_aggregate_as` (SPEC §148); `group` is a `Document` either way.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum LiveRow<R = Document> {
    /// The row is in the result, and wasn't before this commit; or it
    /// was when `watch_aggregate` was called, reported first.
    #[non_exhaustive]
    Entered {
        /// Its group values, at their paths as in the row.
        group: Document,
        /// The row as it is now.
        row: R,
    },
    /// The row was in the result before this commit, and no longer is.
    #[non_exhaustive]
    Left {
        /// Its group values, at their paths as in the row.
        group: Document,
        /// The row as it is now, where its group still has documents but
        /// the row fails the `having`, or the row sort's `skip` or `limit`
        /// leave it out; `None` where no document of the group is left.
        row: Option<R>,
    },
    /// The row was in the result before this commit and still is, with
    /// other values.
    #[non_exhaustive]
    Changed {
        /// Its group values, at their paths as in the row.
        group: Document,
        /// The row as it is now.
        row: R,
    },
}

impl<R> LiveRow<R> {
    /// The group values of the row reported on: `{}` without
    /// `group_by`.
    pub fn group(&self) -> &Document {
        match self {
            LiveRow::Entered { group, .. }
            | LiveRow::Left { group, .. }
            | LiveRow::Changed { group, .. } => group,
        }
    }

    fn try_map<U, E>(self, convert: impl Fn(R) -> Result<U, E>) -> Result<LiveRow<U>, E> {
        Ok(match self {
            LiveRow::Entered { group, row } => LiveRow::Entered {
                group,
                row: convert(row)?,
            },
            LiveRow::Left { group, row } => LiveRow::Left {
                group,
                row: row.map(convert).transpose()?,
            },
            LiveRow::Changed { group, row } => LiveRow::Changed {
                group,
                row: convert(row)?,
            },
        })
    }
}

/// What an aggregate watcher sends: an event, or the error that ended it.
pub(crate) type SentRow = Result<LiveRow, crate::Error>;

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
/// document that couldn't be read. With rows of the application's own,
/// `R` from `watch_aggregate_as` (SPEC §148), each is converted as it is
/// received, on the reader's thread; one that doesn't convert is an
/// `Err` for it, and the live aggregate goes on.
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
pub struct LiveAggregate<R = Document> {
    inbox: Inbox<SentRow>,
    /// `Ok` for a `Document`, `from_document` for an `R`: on the
    /// reader's thread, not under the writer's lock.
    convert: fn(Document) -> crate::Result<R>,
}

impl<R> std::fmt::Debug for LiveAggregate<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveAggregate").finish_non_exhaustive()
    }
}

impl<R> LiveAggregate<R> {
    pub(crate) fn new(
        receiver: mpsc::Receiver<SentRow>,
        convert: fn(Document) -> crate::Result<R>,
    ) -> Self {
        LiveAggregate {
            inbox: Inbox::new(receiver),
            convert,
        }
    }

    fn converted(&self, sent: SentRow) -> crate::Result<LiveRow<R>> {
        sent?.try_map(self.convert)
    }

    /// The next report, waiting for one as long as it takes. `None` once
    /// it has ended, by an error or by the database being dropped, and
    /// every report before has been read.
    pub fn recv(&self) -> Option<crate::Result<LiveRow<R>>> {
        let sent = self.inbox.recv()?;
        Some(self.converted(sent))
    }

    /// The next report if one is waiting, without waiting.
    pub fn try_recv(&self) -> Option<crate::Result<LiveRow<R>>> {
        let sent = self.inbox.try_recv()?;
        Some(self.converted(sent))
    }

    /// The next report, waiting at most `timeout`. `None` if none came,
    /// or it has ended; [`has_ended`](Self::has_ended) tells which.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<crate::Result<LiveRow<R>>> {
        let sent = self.inbox.recv_timeout(timeout)?;
        Some(self.converted(sent))
    }

    /// Whether the live aggregate has ended, every report read (SPEC
    /// §194): true once a `recv`, `try_recv` or `recv_timeout` returned
    /// `None` because it ended, not because none had come yet.
    pub fn has_ended(&self) -> bool {
        self.inbox.has_ended()
    }
}

/// Each report in turn, as `recv` hands them out.
impl<R> Iterator for LiveAggregate<R> {
    type Item = crate::Result<LiveRow<R>>;

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
    let (all, _plan) = at.scope(collection).aggregate_in(filter, &every, &trace)?;
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
    ) -> crate::Result<(Self, mpsc::Receiver<SentRow>)> {
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
        Ok((watcher, receiver))
    }

    /// Runs the aggregate again on `at` and reports how its result
    /// changed: the rows that left it first, in their order before, then
    /// those that came in or changed, in their order now. `false` once it
    /// has ended.
    fn rerun(&mut self, at: &Reading<'_>) -> bool {
        let rows = self.rows(at);
        self.report(rows)
    }

    /// Its rows on `at`: every row, and the result.
    fn rows(&self, at: &Reading<'_>) -> crate::Result<(Vec<Document>, Vec<Document>)> {
        rows_at(at, &self.collection, &self.filter, &self.aggregate)
    }

    /// `rerun`'s report, of `rows` read.
    fn report(&mut self, rows: crate::Result<(Vec<Document>, Vec<Document>)>) -> bool {
        let (all, result) = match rows {
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

/// A read-only open beside a writer in another process (SPEC §195):
/// every live query and aggregate read again on `at`, what is new since
/// it last read reported, and those that have ended dropped. `false`,
/// having reported nothing, where `at` was ended under the reads
/// (`Error::SnapshotTooOld`, §189): the next look tries again.
pub(crate) fn poll(
    watchers: &mut Vec<Watcher>,
    aggregates: &mut Vec<AggregateWatcher>,
    at: &Reading<'_>,
) -> bool {
    let rereads: Vec<_> = watchers.iter().map(|w| w.reread(at)).collect();
    let rows: Vec<_> = aggregates.iter().map(|a| a.rows(at)).collect();
    let too_old =
        |read: Result<(), &crate::Error>| matches!(read, Err(crate::Error::SnapshotTooOld));
    if rereads.iter().any(|r| too_old(r.as_ref().map(|_| ())))
        || rows.iter().any(|r| too_old(r.as_ref().map(|_| ())))
    {
        return false;
    }
    let mut rereads = rereads.into_iter();
    watchers.retain_mut(|w| w.reread_into(rereads.next().expect("one each"), at));
    let mut rows = rows.into_iter();
    aggregates.retain_mut(|a| a.report(rows.next().expect("one each")));
    true
}

/// Ends every live query and aggregate with `e`: a read-only open beside
/// a writer that can no longer read the database (SPEC §195). `Error`
/// isn't `Clone`: each gets an I/O error of the same kind and message, as
/// in `check`.
pub(crate) fn end_all(
    watchers: &mut Vec<Watcher>,
    aggregates: &mut Vec<AggregateWatcher>,
    e: &crate::Error,
) {
    let kind = match e {
        crate::Error::Io(io) => io.kind(),
        _ => std::io::ErrorKind::InvalidData,
    };
    let error = || crate::Error::Io(std::io::Error::new(kind, e.to_string()));
    for w in watchers.drain(..) {
        let _ = w.sender.send(Err(error()));
    }
    for a in aggregates.drain(..) {
        let _ = a.sender.send(Err(error()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    /// A sorted list beside a writer settles from what its look read
    /// (SPEC §195): nothing after the reads can fail as the snapshot is
    /// let go. Settled here against an empty database's reading, it
    /// still reports what the look found; one that read again there
    /// would find nothing.
    #[test]
    fn a_sorted_list_beside_a_writer_settles_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("vans.trunkdb")).unwrap();
        let empty = Database::open(dir.path().join("empty.trunkdb")).unwrap();
        let vans = db.collection::<Document>("vans");
        let van = |speed: i64| -> Document { format!(r#"{{"speed": {speed}}}"#).parse().unwrap() };
        let slow = vans.insert(van(1)).unwrap();
        vans.insert(van(2)).unwrap();
        let fast = vans.insert(van(3)).unwrap();
        let filter = Filter::new().sort_desc("speed").limit(2);

        let state = db.read().unwrap();
        let found =
            crate::collection::find_at(&state.snapshot(), "vans", &whole_read(&filter)).unwrap();
        drop(state);
        let (sender, receiver) = mpsc::channel();
        let mut watcher = Watcher::start("vans".into(), filter, found.clone(), sender)
            .unwrap()
            .polled(seen(&found));
        assert_eq!(receiver.try_iter().count(), 2);

        // The fastest gone: a full list short of one, which the
        // writer's own process would read again for.
        vans.delete(&fast).unwrap();
        let state = db.read().unwrap();
        let reread = watcher.reread(&state.snapshot());
        let nowhere = empty.read().unwrap();
        assert!(watcher.reread_into(reread, &nowhere.snapshot()));
        let events: Vec<_> = receiver.try_iter().map(Result::unwrap).collect();
        assert_eq!(
            events,
            [
                Live::Left {
                    id: fast,
                    doc: None,
                    from: Some(0)
                },
                Live::Entered {
                    id: slow,
                    doc: vans.get(&slow).unwrap().unwrap(),
                    at: Some(1)
                },
            ]
        );
    }
}
