//! Walks in index order: handing out matches in the order an index
//! gives them, or merged by id across several ranges.

use super::Scope;
use super::find::{Candidate, Source};
use crate::data::{self};
use crate::document::{DocId, Document};
use crate::explain::Trace;
use crate::index::{BTreeIndex, Index, KeyRange, key};
use crate::names::FieldNames;
use crate::query::{
    Covering, Decider, Filter, IndexRange, LastDecision, Matcher, OrderedRead, SortOrder,
};
use crate::select::{Answer, Selection};
use crate::storage::{PageStore, RecordLocation};
use std::sync::Arc;

impl<'a> Scope<'a> {
    /// `find` for a filter `Filter::index_order` chose (SPEC §34.2): walks the
    /// index in sort order (`BTreeIndex::walk`, SPEC §49), checks each
    /// document against the whole filter, and stops as soon as `limit`
    /// match — the entries and documents after that are never read. Already
    /// filtered, sorted, skipped and limited.
    ///
    /// Entries are read in groups that share the values of the served sort
    /// keys (`OrderedRead::served`). A group comes in id order, which is sort
    /// order when those values are all equal (`key::part_is_exact`) and no
    /// sort key is left for memory; otherwise the group is read whole and
    /// sorted by every key (SPEC §47) — the rare group whose values may
    /// differ (huge numbers, cut strings), or the documents equal in the
    /// served keys, for the keys after them. Values no one-field index holds
    /// (§34.1: arrays, NaN, ...) sort after everything: if the index runs
    /// out before the limit and no range condition on the sort field ruled
    /// them out, a scan finds them.
    ///
    /// A compound index (SPEC §43.3) is read within the values its first
    /// fields are fixed to — by id within a group, which fields after the
    /// served ones would otherwise order. It holds every document, the
    /// unordered values too, under a tag of their own that sorts last:
    /// right for an ascending walk, and a descending one holds those groups
    /// back until it has passed what they must follow (`Held`).
    ///
    /// A walk in id order leaves most of that idle: each key is one id, a
    /// group of its own and exact. That's the primary index, for a sort by
    /// `_id` (SPEC §96), and a range of a secondary one whose `Eq`s fix every
    /// field, whose keys differ only in their ids (§103). Every document in
    /// either has an ordered value: nothing to look for afterwards.
    ///
    /// Each document is checked on the fields the filter and sort name, and
    /// handed out undecoded (SPEC §99). Where the index covers the filter
    /// (SPEC §100), an entry of an exact group decides without its document,
    /// which is read only if handed out.
    ///
    /// Where the walk's order is the order handed out — forward, or by id;
    /// every field fixed or served, or every key its own group, so that
    /// ties are in key order;
    /// nothing sorted in memory or held — an exact group is never collected:
    /// each entry is handed out as it's walked, its key lent from its leaf
    /// (SPEC §101.5). The matches a skip passes over are counted, not kept,
    /// and a leaf whose ends decide it is passed whole (§101.3).
    pub(super) fn walk_in_index_order(
        self,
        filter: &Filter,
        order: OrderedRead,
        select: Option<&Selection>,
        trace: &Trace,
    ) -> crate::Result<Vec<Candidate>> {
        let (Some(first), Some(limit)) = (filter.sort.first(), filter.limit) else {
            unreachable!("index_order needs a sort and a limit");
        };
        let mut page = Page {
            results: Vec::new(),
            skipped: 0,
            skip: filter.skip,
            limit,
        };
        if limit == 0 {
            return Ok(page.results);
        }
        let name = order.name();
        let (root, fields, compound) = match order.index {
            Some(index) => (index.root, index.fields.len(), index.is_compound()),
            None => match self.meta() {
                Some(meta) => (meta.index_root, 1, false),
                None => return Ok(page.results),
            },
        };
        // Each key a group of its own: one id, in id order (SPEC §96, §103).
        let by_id = order.by_id;
        // An index that counts none left out spares the scan (SPEC §181).
        let unordered_can_match = order.range.is_none()
            && !compound
            && !by_id
            && order.index.is_none_or(|index| index.left_out != Some(0));
        let backward = first.order == SortOrder::Desc;
        let walk = trace.read(name, order.by, backward);
        let mut entries = BTreeIndex::new(root).walk(
            self.store,
            order.range.unwrap_or_else(KeyRange::everything),
            backward,
        );
        let (at, end) = (order.fixed, order.fixed + order.served);
        let spans = Spans {
            at,
            end,
            fields,
            by_id,
        };
        let in_memory = filter.sort.len() > order.served;
        // A walk by id gives the whole order: every key exact, nothing
        // unordered (SPEC §96, §103), so only the conditions' fields are
        // looked at.
        let names = filter.fields_with(!by_id || in_memory);
        let mut decider = Covering::new(filter, order.index, false).map(|c| c.decider(filter));
        if decider.is_some() {
            trace.covering(walk);
        }
        // A match its entry decided, and kept for the page, is answered from
        // its entry where it can be (SPEC §106.5).
        let fields_of = order.index.map_or(&[][..], |index| &index.fields[..]);
        let hand = HandOut {
            answer: select
                .filter(|_| decider.is_some())
                .and_then(|select| Answer::new(select, fields_of)),
            matcher: filter.matcher(),
            trace,
        };
        let mut records = data::Records::new(self.store, self.catalog.names());
        let mut read = |loc: &RecordLocation| -> std::io::Result<Candidate> {
            trace.examined();
            let stored = records.stored(*loc)?;
            let fields = stored.fields(names.as_deref())?;
            Ok((Source::Read(stored), fields))
        };
        // Hands out one collected group's matches; `true` once the page is
        // full.
        let take = |group: &mut Group,
                    decider: &mut Option<Decider>,
                    read: &mut dyn FnMut(&RecordLocation) -> std::io::Result<Candidate>,
                    page: &mut Page|
         -> crate::Result<bool> {
            // Ties in id order: fields after the served ones would order them
            // otherwise, and a backward walk hands them out last id first.
            group.sort_by_key(|(key, _)| key::doc_id(key));
            if spans.exact(&group[0].0) && !in_memory {
                for (key, loc) in group.iter() {
                    if hand.one(key, *loc, decider, read, page)? {
                        return Ok(true);
                    }
                }
                return Ok(false);
            }
            let docs = group
                .iter()
                .map(|(_key, loc)| read(loc))
                .collect::<Result<Vec<_>, _>>()?;
            let unlimited = Filter {
                skip: 0,
                limit: None,
                ..filter.clone()
            };
            let found = unlimited.apply_to(docs, |(_id, doc)| doc);
            trace.matched(found.len());
            for found in found {
                if page.push(found) {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        let mut held = Held::new(backward && compound && !by_id, spans, first.order);
        let direct =
            (!backward || by_id) && !in_memory && !held.active && (by_id || spans.end == fields);
        // The group being collected, and the served bytes it shares.
        let mut group: Group = Vec::new();
        let mut shared: Vec<u8> = Vec::new();
        loop {
            if trace.spent() {
                return Ok(page.results); // an alternative's budget (SPEC §133.4)
            }
            if direct && group.is_empty() {
                // A run of a leaf passed at once (`Walk::run`): its entries
                // all rejected, or all matches the skip passes over — where
                // the group its last entry is in, which may go on past it, is
                // exact.
                let mut run = |most: usize| -> std::io::Result<_> {
                    Ok(entries.run(most)?.map(|leaf| {
                        let decided = decider
                            .as_mut()
                            .and_then(|d| d.decide_all(leaf.lowest, leaf.highest));
                        (decided, leaf.entries, spans.exact(leaf.highest))
                    }))
                };
                // How many it passes, and whether they are matches the skip
                // passes over.
                let budget = page.skip.saturating_sub(page.skipped);
                let passed = match run(usize::MAX)? {
                    Some((Some(false), n, _)) => Some((n, false)),
                    Some((Some(true), n, true)) if n <= budget => Some((n, true)),
                    // The skip ends inside it: the part it passes over.
                    Some((Some(true), _, _)) => match run(budget)? {
                        Some((Some(true), n, true)) => Some((n, true)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((n, matched)) = passed {
                    if matched {
                        trace.matched(n);
                        page.skipped += n;
                    }
                    trace.entries(walk, n);
                    entries.pass_leaf();
                    continue;
                }
            }
            let entry = entries.step().transpose()?;
            trace.entries(walk, usize::from(entry.is_some()));
            let ends_group = !group.is_empty()
                && entry.is_none_or(|(key, _)| spans.group(key) != shared.as_slice());
            if ends_group {
                let next = entry.map(|(key, _)| key);
                if held.active {
                    for mut ready in held.pass(std::mem::take(&mut group), next) {
                        if take(&mut ready, &mut decider, &mut read, &mut page)? {
                            return Ok(page.results);
                        }
                    }
                } else {
                    if take(&mut group, &mut decider, &mut read, &mut page)? {
                        return Ok(page.results);
                    }
                    group.clear();
                }
            }
            let Some((key, loc)) = entry else {
                break;
            };
            if direct && group.is_empty() && spans.exact(key) {
                if hand.one(key, loc, &mut decider, &mut read, &mut page)? {
                    return Ok(page.results);
                }
                continue;
            }
            if group.is_empty() {
                shared.clear();
                shared.extend_from_slice(spans.group(key));
            }
            #[cfg(test)]
            crate::index::KEYS_COPIED.with(|n| n.set(n.get() + 1));
            group.push((key.to_vec(), loc));
            // An id is a group of its own (SPEC §96, §103): handed out
            // without reading the next key to see the group end.
            if by_id {
                if take(&mut group, &mut decider, &mut read, &mut page)? {
                    return Ok(page.results);
                }
                group.clear();
            }
        }
        if unordered_can_match {
            let meta = self.meta().expect("an indexed collection exists");
            let mut unordered = Vec::new();
            let primary = BTreeIndex::new(meta.index_root).scan(self.store)?;
            let scan = trace.read(crate::document::ID_FIELD.to_string(), Vec::new(), false);
            trace.entries(scan, primary.len());
            if trace.spent() {
                return Ok(page.results);
            }
            let wanted = page.skip.saturating_add(limit);
            for (_key, loc) in primary {
                let (id, doc) = read(&loc)?;
                let value = crate::query::value_or_null(&doc, &first.field);
                if crate::query::is_unordered(value) && hand.matcher.matches(&doc)? {
                    trace.matched(1);
                    unordered.push((id, doc));
                    // Without keys after the first, id order is their order.
                    if !in_memory && page.passed() + unordered.len() == wanted {
                        break;
                    }
                }
            }
            let unlimited = Filter {
                conditions: Vec::new(),
                skip: 0,
                limit: None,
                ..filter.clone()
            };
            for found in unlimited.apply_to(unordered, |(_id, doc)| doc) {
                if page.push(found) {
                    break;
                }
            }
        }
        Ok(page.results)
    }
}

/// Hands out a walk's entries one by one, in order (SPEC §101.5): each
/// decided by its entry where it can be, else read and checked; a match
/// decided and kept answered from its entry where it can be (§106.5).
struct HandOut<'a> {
    answer: Option<Answer>,
    matcher: Matcher<'a>,
    trace: &'a Trace,
}

impl HandOut<'_> {
    /// One entry of an exact group: `true` once the page is full.
    fn one(
        &self,
        key: &[u8],
        loc: RecordLocation,
        decider: &mut Option<Decider>,
        read: &mut dyn FnMut(&RecordLocation) -> std::io::Result<Candidate>,
        page: &mut Page,
    ) -> crate::Result<bool> {
        let (source, doc) = match decider.as_mut().and_then(|d| d.decide(key)) {
            Some(false) => return Ok(false),
            Some(true) => {
                let id = key::doc_id(key);
                let answered = (self.answer.as_ref())
                    .filter(|_| page.keeps())
                    .and_then(|answer| answer.entry(key));
                match answered {
                    Some(selected) => (Source::Answered(id, selected), Document::Null),
                    None => (Source::Entry(id, loc, None), Document::Null),
                }
            }
            None => read(&loc)?,
        };
        if !source.decided() && !self.matcher.matches(&doc)? {
            return Ok(false);
        }
        self.trace.matched(1);
        Ok(page.push((source, doc)))
    }
}

/// The ranges of an OR, or an `in`, each fixed whole by `Eq`s, walked
/// side by side for a sort by `_id` (SPEC §167): each holds its keys in
/// id order (§103.1), so the next document in the sort is the next
/// entry of one of them. The walk takes the least id of their next
/// entries (the greatest, backward), hands it out as a walk by id does
/// (§103.4), and steps that range on, until `limit` documents match. A
/// document in two ranges comes out of both at once, one after the
/// other: handed out once.
///
/// Entries decide (§100) only where every range is of one index: a
/// decider reads a key as its index lays it out. Each range keeps its
/// own last decision, so that one decision covers a range's entries as
/// it covers a walk's (§101.2). No leaf is passed whole
/// (§101.3): the next entry may be in another range.
pub(super) fn walk_merged_by_id(
    dictionary: &Arc<FieldNames>,
    store: &dyn PageStore,
    filter: &Filter,
    ranges: Vec<IndexRange>,
    select: Option<&Selection>,
    trace: &Trace,
) -> crate::Result<Vec<Candidate>> {
    let (Some(first), Some(limit)) = (filter.sort.first(), filter.limit) else {
        unreachable!("merged_by_id needs a sort and a limit");
    };
    let mut page = Page {
        results: Vec::new(),
        skipped: 0,
        skip: filter.skip,
        limit,
    };
    if limit == 0 {
        return Ok(page.results);
    }
    let backward = first.order == SortOrder::Desc;
    let one_index = ranges
        .iter()
        .all(|read| std::ptr::eq(read.index, ranges[0].index))
        .then(|| ranges[0].index);
    let mut decider = one_index
        .and_then(|index| Covering::new(filter, Some(index), false))
        .map(|c| c.decider(filter));
    let covering = decider.is_some();
    // Each range's last decision: a decider takes it again where the
    // next key's deciding bytes agree with it (§101.2), as they do within
    // a range fixed by `Eq`s, not across two whose entries interleave.
    // One decider for all, as building one compiles the whole filter.
    let mut lasts: Vec<LastDecision> = (0..ranges.len()).map(|_| LastDecision::default()).collect();
    let hand = HandOut {
        answer: one_index
            .zip(select)
            .filter(|_| covering)
            .and_then(|(index, select)| Answer::new(select, &index.fields)),
        matcher: filter.matcher(),
        trace,
    };
    let mut walks = Vec::with_capacity(ranges.len());
    for read in ranges {
        let at = trace.read(read.index.name(), read.by, backward);
        if covering {
            trace.covering(at);
        }
        walks.push((
            BTreeIndex::new(read.index.root).walk(store, read.range, backward),
            at,
        ));
    }
    // A walk by id gives the whole order: only the conditions' fields
    // are looked at (§103.4).
    let names = filter.fields_with(false);
    let mut records = data::Records::new(store, dictionary);
    let mut read = |loc: &RecordLocation| -> std::io::Result<Candidate> {
        trace.examined();
        let stored = records.stored(*loc)?;
        let fields = stored.fields(names.as_deref())?;
        Ok((Source::Read(stored), fields))
    };
    // Each range's next entry, and the ranges by it, the next in the sort
    // on top.
    let mut next: Vec<Option<(Vec<u8>, RecordLocation)>> = vec![None; walks.len()];
    let mut heads = std::collections::BinaryHeap::with_capacity(walks.len());
    let mut step = |source: usize,
                    next: &mut [Option<(Vec<u8>, RecordLocation)>],
                    heads: &mut std::collections::BinaryHeap<Head>|
     -> std::io::Result<()> {
        let (walk, at) = &mut walks[source];
        if let Some((key, loc)) = walk.step().transpose()? {
            trace.entries(*at, 1);
            #[cfg(test)]
            crate::index::KEYS_COPIED.with(|n| n.set(n.get() + 1));
            heads.push(Head {
                id: key::doc_id(key),
                source,
                backward,
            });
            next[source] = Some((key.to_vec(), loc));
        }
        Ok(())
    };
    for source in 0..next.len() {
        step(source, &mut next, &mut heads)?;
    }
    let mut last: Option<DocId> = None;
    while let Some(head) = heads.pop() {
        if trace.spent() {
            return Ok(page.results); // an alternative's budget (SPEC §133.4)
        }
        let (key, loc) = next[head.source].take().expect("a head has its entry");
        if last != Some(head.id) {
            last = Some(head.id);
            if let Some(decider) = decider.as_mut() {
                decider.swap_last(&mut lasts[head.source]);
            }
            let full = hand.one(&key, loc, &mut decider, &mut read, &mut page)?;
            if let Some(decider) = decider.as_mut() {
                decider.swap_last(&mut lasts[head.source]);
            }
            if full {
                return Ok(page.results);
            }
        }
        step(head.source, &mut next, &mut heads)?;
    }
    Ok(page.results)
}

/// A range's next entry in a merge by id (`walk_merged_by_id`): ordered
/// so that the heap's top is the next in the walk's direction, the least
/// id forward and the greatest backward.
#[derive(PartialEq, Eq)]
struct Head {
    id: DocId,
    source: usize,
    backward: bool,
}

impl Ord for Head {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let order = self.id.cmp(&other.id).then(other.source.cmp(&self.source));
        match self.backward {
            true => order,
            false => order.reverse(),
        }
    }
}

impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The page `walk_in_index_order` hands out (SPEC §101.5): matches in
/// order, the first `skip` only counted.
struct Page {
    results: Vec<Candidate>,
    skipped: usize,
    skip: usize,
    limit: usize,
}

impl Page {
    /// The next match in order: passed over while the skip lasts, kept
    /// after. `true` once `limit` are kept.
    fn push(&mut self, found: Candidate) -> bool {
        if self.skipped < self.skip {
            self.skipped += 1;
            return false;
        }
        self.results.push(found);
        self.results.len() == self.limit
    }

    /// Whether the next match is kept, not passed over.
    fn keeps(&self) -> bool {
        self.skipped >= self.skip
    }

    /// Matches so far, skipped or kept.
    fn passed(&self) -> usize {
        self.skipped + self.results.len()
    }
}

/// Index entries sharing the values of the served sort keys.
type Group = Vec<(Vec<u8>, RecordLocation)>;

/// Where the served sort keys' values are in an index key.
#[derive(Clone, Copy)]
pub(super) struct Spans {
    /// Fixed fields before them.
    pub(super) at: usize,
    /// Fixed fields and served keys.
    pub(super) end: usize,
    pub(super) fields: usize,
    /// A walk by id (SPEC §96, §103): each key a group of its own, its
    /// id unique in the walk.
    pub(super) by_id: bool,
}

impl Spans {
    /// The bytes a group shares: the fixed values and the served ones —
    /// up to the first that may stand for several values (a cut string,
    /// a huge number): the keys after it don't order entries it holds,
    /// whose true values may differ; they're sorted in memory instead.
    /// A damaged key with fewer values than the index has fields groups
    /// by the ones it has (SPEC §55).
    pub(super) fn group<'k>(&self, key: &'k [u8]) -> &'k [u8] {
        if self.by_id {
            return key; // one document, whose id is unique
        }
        let mut len = 0;
        let parts = key::each_part(key::value_part(key)).take(self.end);
        for (i, part) in parts.enumerate() {
            len += part.len();
            if i >= self.at && !key::part_is_exact(part, self.fields) {
                break;
            }
        }
        &key[..len]
    }

    /// Whether the served values of `key`'s group are each exact: one
    /// value each, so the group's entries are equal in them.
    fn exact(&self, key: &[u8]) -> bool {
        self.by_id
            || key::each_part(self.group(key))
                .skip(self.at)
                .all(|part| key::part_is_exact(part, self.fields))
    }

    /// The served values a group shares, each on its own.
    pub(super) fn values<'k>(&self, key: &'k [u8]) -> Vec<&'k [u8]> {
        if self.by_id {
            return Vec::new(); // an id, exact
        }
        let shared = self.group(key);
        key::parts(shared).into_iter().skip(self.at).collect()
    }
}

/// A descending walk of a compound index hands out a group whose served
/// value is unordered (`key::is_other`) before the groups with the same
/// values up to it, since that tag sorts last — but in a sort, unordered
/// values go last in both directions (§34.1). So such a group is held
/// until the walk leaves those values, then handed out after them
/// (SPEC §49). Unordered values are rare; so are held groups.
struct Held {
    active: bool,
    spans: Spans,
    order: SortOrder,
    /// Held groups by the bytes before their first unordered value, the
    /// innermost last.
    stack: Vec<(Vec<u8>, Vec<Group>)>,
}

impl Held {
    fn new(active: bool, spans: Spans, order: SortOrder) -> Self {
        Held {
            active,
            spans,
            order,
            stack: Vec::new(),
        }
    }

    /// Takes a finished group, `next` being the walk's next key (`None`
    /// at the end), and returns the groups to hand out now, in order.
    fn pass(&mut self, group: Group, next: Option<&[u8]>) -> Vec<Group> {
        if !self.active {
            return vec![group];
        }
        let mut ready = Vec::new();
        let key = group[0].0.clone();
        let values = self.spans.values(&key);
        match values.iter().position(|part| key::is_other(part)) {
            Some(i) => {
                let before: usize = self.spans.group(&key).len()
                    - values[i..].iter().map(|p| p.len()).sum::<usize>();
                let prefix = key[..before].to_vec();
                match self.stack.last_mut() {
                    Some((top, groups)) if *top == prefix => groups.push(group),
                    _ => self.stack.push((prefix, vec![group])),
                }
            }
            None => ready.push(group),
        }
        while let Some((prefix, _)) = self.stack.last() {
            if next.is_some_and(|next| next.starts_with(prefix)) {
                break;
            }
            let (_, mut groups) = self.stack.pop().expect("just looked");
            let order = self.order;
            let spans = self.spans;
            groups.sort_by_cached_key(|group| {
                spans
                    .values(&group[0].0)
                    .into_iter()
                    .map(|part| (key::is_other(part), Directed(part.to_vec(), order)))
                    .collect::<Vec<_>>()
            });
            ready.extend(groups);
        }
        ready
    }
}

/// An encoded value that sorts ascending or descending as its
/// `SortOrder` says — the key `read_in_index_order` sorts groups by.
struct Directed(Vec<u8>, SortOrder);

impl Ord for Directed {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let ord = self.0.cmp(&other.0);
        match self.1 {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        }
    }
}

impl PartialOrd for Directed {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Directed {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Directed {}
