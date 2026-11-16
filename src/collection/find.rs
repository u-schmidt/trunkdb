//! Finding documents: by id, through an index or by a scan, and the
//! candidates a read checks against its filter.

use super::Scope;
use super::from_document;
use super::plan::follow_hint;
use super::walk::walk_merged_by_id;
use crate::catalog::{Catalog, IndexMeta};
use crate::data::{self, Stored};
use crate::database::Reading;
use crate::document::{DocId, Document};
use crate::explain::Trace;
use crate::index::{BTreeIndex, Index, KeyRange, key};
use crate::query::{Condition, Covering, Filter, Rank, Ranking};
use crate::select::{Answer, Selection};
use crate::storage::RecordLocation;
use serde::de::DeserializeOwned;

/// Document `id` of `collection` as it is stored in `at`; `None` if
/// there's none.
pub(super) fn stored_at(
    at: &Reading<'_>,
    collection: &str,
    id: &DocId,
) -> crate::Result<Option<Stored>> {
    let Some(meta) = at.catalog.get(collection) else {
        return Ok(None); // collection doesn't exist yet, so neither does the document
    };
    let index = BTreeIndex::new(meta.index_root);
    let Some(loc) = index.lookup(&at.store, &key::primary(*id))? else {
        return Ok(None);
    };
    Ok(Some(
        data::Records::new(&at.store, at.catalog.names()).stored(loc)?,
    ))
}

/// Document `id` of `collection` as of `at`, whole: what a live query
/// checks (SPEC §138.2).
pub(crate) fn document_at(
    at: &Reading<'_>,
    collection: &str,
    id: &DocId,
) -> crate::Result<Option<Document>> {
    match stored_at(at, collection, id)? {
        Some(stored) => Ok(Some(stored.decode()?)),
        None => Ok(None),
    }
}

/// Every match of `filter` on `collection` as of `at`, with its id,
/// whole: what a sorted live query reads again (SPEC §145.2).
pub(crate) fn find_at(
    at: &Reading<'_>,
    collection: &str,
    filter: &Filter,
) -> crate::Result<Vec<(DocId, Document)>> {
    let trace = Trace::default();
    at.scope(collection)
        .find_in(filter, true, None, &trace)?
        .into_iter()
        .map(|found| Ok((found.id(), found.decode(None)?)))
        .collect()
}

impl<'a> Scope<'a> {
    /// Every match with its id, against a catalog and store the caller
    /// has locked — for a read, or inside a write batch (`delete_many`).
    /// What it read and matched goes to `trace` (SPEC §94.3).
    pub(super) fn find_in(
        self,
        filter: &Filter,
        whole_from_index: bool,
        select: Option<&Selection>,
        trace: &Trace,
    ) -> crate::Result<Vec<Found>> {
        let indexes = self.indexes();
        follow_hint(self.collection, filter, indexes)?;
        let mut records = data::Records::new(self.store, self.catalog.names());
        if let Some(order) = filter.index_order(indexes) {
            // The skipped matches are passed over by the walk (SPEC §72),
            // unread where their entries decided (§100), and not kept (§101).
            let found = self.walk_in_index_order(filter, order, select, trace)?;
            return found
                .into_iter()
                .map(|(source, _fields)| Ok(source.found(&mut records, None, trace)?))
                .collect();
        }
        if let Some(ranges) = filter.merged_by_id(indexes) {
            // An OR's ranges, each in id order, merged by id (SPEC §167).
            let found = walk_merged_by_id(
                self.catalog.names(),
                self.store,
                filter,
                ranges,
                select,
                trace,
            )?;
            return found
                .into_iter()
                .map(|(source, _fields)| Ok(source.found(&mut records, None, trace)?))
                .collect();
        }
        // A key sort (SPEC §154), or a sort by fields with a limit (§176):
        // only the best `skip + limit` kept as they are read.
        if let Some(ranking) = filter.ranking().or_else(|| filter.kept_sort()) {
            return self.find_by_key(filter, &ranking, whole_from_index, select, trace);
        }
        // Candidates from an index range or by id nearly all match: for a
        // caller that wants whole documents, each is decoded whole once and
        // checked on that, rather than on its fields and then again whole
        // (SPEC §99.2). A scan's are mostly rejected, and checked on fields.
        // A selection is decoded from the stored document after the check.
        let whole = whole_from_index
            && select.is_none()
            && (filter.ids().is_some() || filter.index_reads(indexes).is_some());
        let names = match whole {
            true => None,
            false => filter.fields(),
        };
        let sort = !filter.sort.is_empty();
        let matcher = filter.matcher();
        let (candidates, answer) =
            self.read_candidates(filter, names.as_deref(), Wants { sort, select }, trace)?;
        let mut matches: Vec<Candidate> = Vec::new();
        for (source, fields) in candidates {
            if source.decided() || matcher.matches(&fields)? {
                matches.push((source, fields));
            }
        }
        trace.matched(matches.len());
        if sort {
            trace.held(matches.len());
            // So equal sort values end up in id order, as they do when read
            // from an index (the sort is stable).
            matches.sort_unstable_by_key(|(source, _fields)| source.id());
        }
        let arranged = filter.arrange(matches, |(_source, fields)| fields)?;
        arranged
            .into_iter()
            .map(|(source, fields)| {
                // Only a document read was decoded whole, not an entry's.
                let whole = whole && matches!(source, Source::Read(_));
                let mut found = source.found(&mut records, answer.as_ref(), trace)?;
                if whole {
                    found.decoded = Some(fields);
                }
                Ok(found)
            })
            .collect()
    }

    /// `find_in` for a sort by a key of the application's (SPEC §154), or by
    /// fields with a limit (§176): each match ranked as it is read, by its
    /// key's closure or its sort fields' values, and with a limit only the
    /// best `skip + limit` kept, cut back to them by a selection whenever
    /// twice as many are held. Memory stays at about that many matches,
    /// read in full, however many there are.
    fn find_by_key(
        self,
        filter: &Filter,
        ranking: &Ranking<'_>,
        whole_from_index: bool,
        select: Option<&Selection>,
        trace: &Trace,
    ) -> crate::Result<Vec<Found>> {
        let indexes = self.indexes();
        // As `find_in` decides it: a candidate from an index range or by id
        // is decoded whole once, for its check, its key and the caller.
        let whole = whole_from_index
            && select.is_none()
            && (filter.ids().is_some() || filter.index_reads(indexes).is_some());
        let names = match whole {
            true => None,
            false => filter.fields(),
        };
        let matcher = filter.matcher();
        let keep = filter.limit.map(|limit| filter.skip.saturating_add(limit));
        // By the key, the fields after it, then by id: a total order.
        let order = |(a, (source_a, _)): &(Rank, Candidate),
                     (b, (source_b, _)): &(Rank, Candidate)| {
            ranking
                .compare(a, b)
                .then_with(|| source_a.id().cmp(&source_b.id()))
        };
        let mut kept: Vec<(Rank, Candidate)> = Vec::new();
        let mut matched = 0;
        let answer = self.each_candidate(
            filter,
            names.as_deref(),
            Wants { sort: true, select },
            trace,
            |(source, fields)| -> crate::Result<()> {
                if !(source.decided() || matcher.matches(&fields)?) {
                    return Ok(());
                }
                let Some(rank) = ranking.rank(&fields)? else {
                    return Ok(()); // left out by its key
                };
                matched += 1;
                if keep == Some(0) {
                    return Ok(());
                }
                kept.push((rank, (source, fields)));
                trace.held(kept.len());
                if let Some(keep) = keep
                    && kept.len() > keep.saturating_mul(2)
                {
                    kept.select_nth_unstable_by(keep, order);
                    kept.truncate(keep);
                }
                Ok(())
            },
        )?;
        trace.matched(matched);
        kept.sort_unstable_by(order);
        let mut records = data::Records::new(self.store, self.catalog.names());
        kept.into_iter()
            .take(keep.unwrap_or(usize::MAX))
            .skip(filter.skip)
            .map(|(_rank, (source, fields))| {
                // Only a document read was decoded whole, not an entry's.
                let whole = whole && matches!(source, Source::Read(_));
                let mut found = source.found(&mut records, answer.as_ref(), trace)?;
                if whole {
                    found.decoded = Some(fields);
                }
                Ok(found)
            })
            .collect()
    }

    /// The index entries `find` has to look at for `filter`, gathered: what
    /// `each_entry` hands on, with the index they come from. For a cursor,
    /// which reads its documents after the lock is let go.
    pub(super) fn candidate_entries(
        self,
        filter: &Filter,
        trace: &Trace,
    ) -> std::io::Result<(Vec<Entry>, Indexed<'a>)> {
        let mut entries = Vec::new();
        self.each_entry(filter, trace, |entry| {
            entries.push(entry);
            Ok::<(), std::io::Error>(())
        })?;
        Ok((entries, indexed_for(self.catalog, self.collection, filter)))
    }

    /// The index entries `find` has to look at for `filter`, each handed to
    /// `take` as the walk reaches it (SPEC §176): the primary index's entries
    /// for the ids it names (SPEC §61), else the secondary index ranges its
    /// conditions allow (SPEC §28.4, §36.3), otherwise the whole primary
    /// index. Each entry's key ends with its document's id, and each
    /// document comes once, even if several ranges hold it (an OR whose
    /// branches overlap) or one range holds it several times (a multikey
    /// index, SPEC §42.2): only then are the ids handed on kept, to pass
    /// over the second. Nothing else is held, so a caller that keeps little
    /// holds little however many entries there are; one that stops early,
    /// with an error of its own, reads no further.
    pub(super) fn each_entry<E: From<std::io::Error>>(
        self,
        filter: &Filter,
        trace: &Trace,
        mut take: impl FnMut(Entry) -> Result<(), E>,
    ) -> Result<(), E> {
        let Some(meta) = self.meta() else {
            return Ok(());
        };
        // Whether the entries decide, so each read is marked as it begins.
        let covering = indexed_for(self.catalog, self.collection, filter)
            .covering(filter, false)
            .is_some();
        let begin = |index: String, bounds: Vec<Condition>| {
            let read = trace.read(index, bounds, false);
            if covering {
                trace.covering(read);
            }
            read
        };
        let ids_read = |ids: &[DocId]| {
            let by_id = |id: &DocId| Condition::id(*id);
            let bounds = match ids {
                [id] => vec![by_id(id)],
                _ => vec![Condition::any(ids.iter().map(by_id))],
            };
            let read = begin(crate::document::ID_FIELD.to_string(), bounds);
            trace.entries(read, ids.len());
        };
        let primary = BTreeIndex::new(meta.index_root);
        if let Some(ids) = filter.ids() {
            ids_read(&ids);
            for id in ids {
                let key = key::primary(id);
                if let Some(loc) = primary.lookup(self.store, &key)? {
                    take((key, loc))?;
                }
            }
            return Ok(());
        }
        let Some(crate::query::IndexReads { ids, ranges }) = filter.index_reads(self.indexes())
        else {
            let read = begin(crate::document::ID_FIELD.to_string(), Vec::new());
            for entry in primary.walk(self.store, KeyRange::everything(), false) {
                trace.entries(read, 1);
                take(entry?)?;
            }
            return Ok(());
        };
        // One range of an index with one key per document holds each once;
        // several ranges, or a multikey index, may hold one several times;
        // so may ids beside a range.
        let multikey = ranges
            .first()
            .is_some_and(|r| r.index.fields.iter().any(|f| crate::query::is_multi(f)));
        let once = ranges.len() == 1 && !multikey && ids.is_empty();
        let mut seen = std::collections::HashSet::new();
        // The ids an OR's branches name first, looked up as `ById` looks
        // them up (SPEC §164.2).
        if !ids.is_empty() {
            ids_read(&ids);
            for id in ids {
                let key = key::primary(id);
                if let Some(loc) = primary.lookup(self.store, &key)? {
                    seen.insert(id);
                    take((key, loc))?;
                }
            }
        }
        for range in ranges {
            let read = begin(range.index.name(), range.by);
            for entry in BTreeIndex::new(range.index.root).walk(self.store, range.range, false) {
                trace.entries(read, 1);
                let entry = entry?;
                if once || seen.insert(key::doc_id(&entry.0)) {
                    take(entry)?;
                }
            }
        }
        Ok(())
    }

    /// The candidates behind `candidate_entries`, not yet checked against
    /// the filter — an index range may hold a few that don't match (SPEC
    /// §28.3) — each with the fields `names` names. Where the index read
    /// covers the filter (SPEC §100), an entry decides instead of its
    /// document: one that matches comes unread (`Source::Entry`), already
    /// checked, and one that doesn't isn't read at all. With `sort`, for a
    /// sort in memory, such a match comes with its entry's document to sort
    /// by; where the index lacks a sort key, it is read for that key.
    pub(super) fn read_candidates(
        self,
        filter: &Filter,
        names: Option<&[&str]>,
        wants: Wants,
        trace: &Trace,
    ) -> std::io::Result<(Vec<Candidate>, Option<Answer>)> {
        let mut candidates = Vec::new();
        let answer = self.each_candidate(filter, names, wants, trace, |candidate| {
            candidates.push(candidate);
            Ok::<(), std::io::Error>(())
        })?;
        Ok((candidates, answer))
    }

    /// `read_candidates`, each candidate handed to `take` as it is read
    /// rather than all kept: for a sort by a key of the application's
    /// (SPEC §154), which keeps only the best of them.
    pub(super) fn each_candidate<E: From<std::io::Error>>(
        self,
        filter: &Filter,
        names: Option<&[&str]>,
        wants: Wants,
        trace: &Trace,
        mut take: impl FnMut(Candidate) -> Result<(), E>,
    ) -> Result<Option<Answer>, E> {
        let Wants { sort, select } = wants;
        let indexed = indexed_for(self.catalog, self.collection, filter);
        let mut decides = indexed.covering(filter, false).map(|c| c.decider(filter));
        let orders = match sort {
            true => indexed.covering(filter, true).map(|c| c.for_sort(filter)),
            false => None,
        };
        // Where the entries may answer the selection (SPEC §106.5), a match
        // keeps its key, to be answered from if it's handed out.
        let answer = answer_for(select, indexed, filter);
        let mut records = data::Records::new(self.store, self.catalog.names());
        self.each_entry(filter, trace, |(key, loc)| {
            let decided = decides.as_mut().and_then(|d| d.decide(&key));
            if decided == Some(false) {
                return Ok(()); // rejected by its entry, never read
            }
            // A match whose entry also holds the sort keys, or with no sort
            // to need them, stays unread: its entry's document is all the
            // sort looks at.
            let entry = match (decided, &orders) {
                (Some(true), _) if !sort => Some(Document::Null),
                (Some(true), Some(orders)) => orders.entry(&key),
                _ => None,
            };
            if let Some(fields) = entry {
                let id = key::doc_id(&key);
                let key = answer.is_some().then_some(key);
                return take((Source::Entry(id, loc, key), fields));
            }
            trace.examined();
            // The id stored in the data cell itself (SPEC §11).
            let stored = records.stored(loc)?;
            let fields = stored.fields(names)?;
            take((Source::Read(stored), fields))
        })?;
        Ok(answer)
    }
}

/// How a read answers `select` from the entries of the index it read
/// (SPEC §106.5) — `None` unless those entries decide its filter and hold
/// every selected path.
pub(super) fn answer_for(
    select: Option<&Selection>,
    indexed: Indexed,
    filter: &Filter,
) -> Option<Answer> {
    let select = select?;
    indexed.covering(filter, false)?;
    match indexed {
        Indexed::Primary => Answer::new(select, &[]),
        Indexed::One(index) => Answer::new(select, &index.fields),
        Indexed::Several => None,
    }
}

/// A match of `find_in`, not decoded yet — or, where it was decoded whole
/// to be checked, with that document; or, where its index entry answered
/// the selection (SPEC §106.5), with that result and no document at all.
pub(crate) struct Found {
    id: DocId,
    /// `None` where the entry answered.
    stored: Option<Stored>,
    /// The whole document, decoded to be checked; or, without `stored`,
    /// the selection built from the entry.
    decoded: Option<Document>,
}

impl From<Stored> for Found {
    fn from(stored: Stored) -> Self {
        Found::stored(stored)
    }
}

impl Found {
    fn stored(stored: Stored) -> Self {
        Found {
            id: stored.id(),
            stored: Some(stored),
            decoded: None,
        }
    }

    /// A result its index entry gave (SPEC §106.5): already the selection.
    pub(crate) fn answered(id: DocId, selected: Document) -> Self {
        Found {
            id,
            stored: None,
            decoded: Some(selected),
        }
    }

    pub(crate) fn id(&self) -> DocId {
        self.id
    }

    /// The document as handed out: whole, or cut down to `select` (SPEC
    /// §106), decoded now unless it already was.
    pub(crate) fn decode(self, select: Option<&Selection>) -> std::io::Result<Document> {
        match (self.stored, self.decoded, select) {
            (None, Some(selected), _) => Ok(selected),
            (_, Some(whole), None) => Ok(whole),
            (_, Some(whole), Some(select)) => Ok(select.project(whole)),
            (Some(stored), None, None) => stored.decode(),
            (Some(stored), None, Some(select)) => {
                select.decode(stored.encoded(), stored.id(), stored.names())
            }
            (None, None, _) => unreachable!("a match is stored or answered"),
        }
    }

    /// The document as a `T`: straight from its encoding (SPEC §99.4),
    /// or with a selection, from the selection through the bridge.
    pub(crate) fn deserialize<T: DeserializeOwned>(
        self,
        select: Option<&Selection>,
    ) -> crate::Result<T> {
        match (&self.stored, &self.decoded, select) {
            (Some(stored), None, None) => stored.deserialize(),
            _ => from_document(self.decode(select)?),
        }
    }
}

/// A candidate as `find` checks it (SPEC §99.2): the document as it is
/// stored, and the fields its filter and sort name, decoded
/// (`Filter::fields`) — or, where its index entry decided (SPEC §100),
/// where it is and its entry's document. The whole document is decoded
/// only for what is handed out (§99.3).
pub(super) type Candidate = (Source, Document);

/// Where a candidate's document is: read, or not yet (SPEC §100).
pub(super) enum Source {
    Read(Stored),
    /// Its index entry decided: its id and where it is, read only if
    /// handed out — and its key, where the read's selection may be
    /// answered from it (SPEC §106.5).
    Entry(DocId, RecordLocation, Option<Vec<u8>>),
    /// Its index entry decided, and answered the selection.
    Answered(DocId, Document),
}

impl Source {
    /// Its entry decided it matches: no need to check it again.
    pub(super) fn decided(&self) -> bool {
        !matches!(self, Source::Read(_))
    }

    pub(super) fn id(&self) -> DocId {
        match self {
            Source::Read(stored) => stored.id(),
            Source::Entry(id, ..) | Source::Answered(id, _) => *id,
        }
    }

    /// The match as handed out: its document read now if it wasn't, or
    /// its result from its key where `answer` can build it there.
    fn found(
        self,
        records: &mut data::Records,
        answer: Option<&Answer>,
        trace: &Trace,
    ) -> std::io::Result<Found> {
        match self {
            Source::Read(stored) => Ok(Found::stored(stored)),
            Source::Answered(id, selected) => {
                trace.answered_from_entry();
                Ok(Found::answered(id, selected))
            }
            Source::Entry(id, loc, key) => {
                let answered = key.zip(answer).and_then(|(key, answer)| answer.entry(&key));
                if let Some(selected) = answered {
                    trace.answered_from_entry();
                    return Ok(Found::answered(id, selected));
                }
                trace.examined();
                Ok(Found::stored(records.stored(loc)?))
            }
        }
    }
}

/// Which index `each_entry` reads for `filter`, before it reads it.
fn indexed_for<'a>(catalog: &'a Catalog, collection: &str, filter: &Filter) -> Indexed<'a> {
    if catalog.get(collection).is_none() || filter.ids().is_some() {
        return Indexed::Primary;
    }
    match filter.index_reads(catalog.indexes(collection)) {
        None => Indexed::Primary,
        Some(crate::query::IndexReads { ids, ranges }) => {
            let first = ranges.first().map(|range| range.index);
            match first {
                Some(first)
                    if ids.is_empty() && ranges.iter().all(|r| std::ptr::eq(r.index, first)) =>
                {
                    Indexed::One(first)
                }
                _ => Indexed::Several,
            }
        }
    }
}

/// An index entry: its key, and where its document is.
pub(super) type Entry = (Vec<u8>, RecordLocation);

/// Which index `candidate_entries` read.
#[derive(Clone, Copy)]
pub(super) enum Indexed<'a> {
    Primary,
    One(&'a IndexMeta),
    /// Ranges of several, for an OR (SPEC §36.3): no one index holds
    /// every field the filter names, so their entries decide nothing.
    Several,
}

impl<'a> Indexed<'a> {
    /// `Covering::new` for the index read.
    pub(super) fn covering(self, filter: &Filter, sort: bool) -> Option<Covering<'a>> {
        match self {
            Indexed::Primary => Covering::new(filter, None, sort),
            Indexed::One(index) => Covering::new(filter, Some(index), sort),
            Indexed::Several => None,
        }
    }
}

/// `filter` stopping at its first match — what `find_one` asks for, so
/// a sorted one can stop early too (SPEC §34.2).
pub(super) fn first_only(mut filter: Filter) -> Filter {
    filter.limit = Some(filter.limit.map_or(1, |limit| limit.min(1)));
    filter
}

/// What a caller of `read_candidates` wants of the matches besides
/// their decision: their sort keys, for a sort in memory, and their keys
/// where the selection may be answered from them (SPEC §106.5).
#[derive(Clone, Copy, Default)]
pub(super) struct Wants<'s> {
    pub(super) sort: bool,
    pub(super) select: Option<&'s Selection>,
}
