//! Counting and aggregating: `count`, `any_match_in` and aggregates,
//! from index entries where they can tell and from documents where not.

use super::Scope;
use super::find::Wants;
use super::plan::{follow_hint, plan_for};
use crate::aggregate::{Aggregate, Groups, Kind, Order, Window};
use crate::catalog::IndexMeta;
use crate::data::{self};
use crate::document::{DocId, Document};
use crate::explain::Trace;
use crate::index::{BTreeIndex, KeyRange, key};
use crate::query::{Condition, Covering, Decider, Filter, QueryPlan};
use crate::select::Selection;
use crate::storage::{PageId, PageStore};
use crate::trunc::Cut;

impl<'a> Scope<'a> {
    /// What `count` gives for `filter`: counted as an index is walked where
    /// it can be (`count_in`), else from the candidates.
    pub(super) fn count_matches(self, filter: &Filter, trace: &Trace) -> crate::Result<usize> {
        follow_hint(self.collection, filter, self.indexes())?;
        // A key of the application's leaves out the matches it gives none
        // (SPEC §154): counted as `find` finds them, each one read.
        let ranking = filter.ranking();
        let counted = match ranking {
            Some(_) => None,
            None => self.count_in(filter)?,
        };
        let count = match counted {
            Some(count) => count,
            None => {
                let names = filter.fields_with(ranking.is_some());
                let matcher = filter.matcher();
                let mut count = 0;
                self.each_candidate(
                    filter,
                    names.as_deref(),
                    // With a key, every match is read: the key needs its
                    // document, which an entry that decided it doesn't give.
                    Wants {
                        sort: ranking.is_some(),
                        select: None,
                    },
                    trace,
                    |(source, fields)| -> crate::Result<()> {
                        let matches = source.decided() || matcher.matches(&fields)?;
                        let ranked = match &ranking {
                            Some(ranking) => matches && ranking.rank(&fields)?.is_some(),
                            None => matches,
                        };
                        count += usize::from(ranked);
                        Ok(())
                    },
                )?;
                count
            }
        };
        let count = count.saturating_sub(filter.skip);
        Ok(filter.limit.map_or(count, |limit| count.min(limit)))
    }

    /// Whether any document of `collection` matches `filter`'s conditions,
    /// against a catalog and store the caller has locked: a batch's
    /// precondition (SPEC §162). Read as `count_matches` reads them, by id or
    /// through an index where one bounds the conditions, an entry that
    /// decides counting without its document; but the walk stops at the
    /// first match. The caller has refused sort, skip, limit and selection.
    pub(crate) fn any_match_in(self, filter: &Filter) -> crate::Result<bool> {
        follow_hint(self.collection, filter, self.indexes())?;
        /// Why the walk ended early: a match, or a failure to report.
        enum Stop {
            Matched,
            Failed(crate::Error),
        }
        impl From<std::io::Error> for Stop {
            fn from(error: std::io::Error) -> Self {
                Stop::Failed(error.into())
            }
        }
        let names = filter.fields_with(false);
        let matcher = filter.matcher();
        let walked = self.each_candidate(
            filter,
            names.as_deref(),
            Wants {
                sort: false,
                select: None,
            },
            &Trace::default(),
            |(source, fields)| -> Result<(), Stop> {
                match source.decided()
                    || matcher
                        .matches(&fields)
                        .map_err(|panicked| Stop::Failed(panicked.into()))?
                {
                    true => Err(Stop::Matched),
                    false => Ok(()),
                }
            },
        );
        match walked {
            Ok(_) => Ok(false),
            Err(Stop::Matched) => Ok(true),
            Err(Stop::Failed(error)) => Err(error),
        }
    }

    /// `Collection::aggregate` against a catalog and store the caller has
    /// locked (SPEC §107): from index entries where they hold the answer
    /// (`aggregate_by_entries`), else from the documents `find_in` finds,
    /// each decoded on the paths the aggregate reads. With the plan it
    /// followed.
    pub(crate) fn aggregate_in(
        self,
        filter: &Filter,
        aggregate: &Aggregate,
        trace: &Trace,
    ) -> crate::Result<(Vec<Document>, QueryPlan)> {
        aggregate.check()?;
        let indexes = self.indexes();
        follow_hint(self.collection, filter, indexes)?;
        // A window is a condition once the newest is known (SPEC §139.2).
        let windowed;
        let picks = filter.sorted() || filter.skip > 0 || filter.limit.is_some();
        if picks && aggregate.per_group.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "an aggregate with a window per group over a filter with a sort, a skip or a \
                 limit: the window keeps each group's newest, the limit of all (SPEC §174)",
            )
            .into());
        }
        let filter = match &aggregate.window {
            Some(_) if picks => {
                // Two windows, by count and by time: which first would be a
                // rule to learn, and `apply`, given the documents the filter
                // found, could keep only one of them.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "an aggregate with a window over a filter with a sort, a skip or a limit: \
                     window by time or by count, not both",
                )
                .into());
            }
            Some(window) => {
                let newest = self.newest_in(filter, window, trace)?;
                windowed = filter.clone().and(window.condition(newest));
                &windowed
            }
            None => filter,
        };
        let mut groups = Groups::new(aggregate);
        // Counts alone, of every match: `count`'s.
        let counts_only = aggregate.columns.iter().all(|c| c.kind == Kind::Count);
        let whole = aggregate.unwind.is_empty()
            && aggregate.per_group.is_none()
            && !aggregate.has_conditions();
        if aggregate.group_by.is_empty() && counts_only && whole {
            let n = self.count_matches(filter, trace)?;
            trace.matched(n);
            if n > 0 {
                let takes = vec![true; aggregate.columns.len()];
                groups.add_counted(Vec::new(), n as u64, &takes, Order::id(DocId::NIL), None);
            }
            trace.groups(1);
            return Ok((
                groups.finish(|_| unreachable!("counts only"))?,
                plan_for(filter, indexes),
            ));
        }
        if let Some(plan) = self.aggregate_by_entries(filter, &mut groups, trace)? {
            trace.groups(groups.len());
            let selection = aggregate_selection(aggregate);
            let mut records = data::Records::new(self.store, self.catalog.names());
            let rows = groups.finish(|loc| {
                trace.examined();
                let stored = records.stored(loc)?;
                selection.decode(stored.encoded(), stored.id(), stored.names())
            })?;
            return Ok((rows, plan));
        }
        let selection = aggregate_selection(aggregate);
        let found = self.find_in(filter, false, Some(&selection), trace)?;
        match aggregate.per_group {
            None => {
                for found in found {
                    let id = found.id();
                    let doc = found.decode(Some(&selection))?;
                    groups.add_document(&doc, Order::id(id), None)?;
                }
            }
            // Each group's window needs its newest, so every match first
            // (SPEC §174).
            Some(_) => {
                let docs = (found.into_iter())
                    .map(|found| {
                        let order = Order::id(found.id());
                        Ok((found.decode(Some(&selection))?, order))
                    })
                    .collect::<crate::Result<Vec<_>>>()?;
                for (doc, order) in crate::aggregate::per_group(aggregate, docs) {
                    groups.add_document(&doc, order, None)?;
                }
            }
        }
        trace.groups(groups.len());
        let rows = groups.finish(|_| unreachable!("every value from its document"))?;
        Ok((rows, plan_for(filter, indexes)))
    }

    /// The latest date-time at a window's path among the documents that
    /// meet `filter`'s conditions (SPEC §139.2): the first of them sorted
    /// newest first, read through an index on the path where there is one.
    /// Its sort, skip and limit play no part; its hint steers the read.
    fn newest_in(
        self,
        filter: &Filter,
        window: &Window,
        trace: &Trace,
    ) -> crate::Result<Option<crate::DateTime>> {
        let mut newest = (filter.conditions.iter().cloned())
            .fold(Filter::new(), Filter::and)
            // Date-times compare with date-times alone (§69.2).
            .and(Condition::gte(&window.path, crate::DateTime::EARLIEST))
            .sort_desc(&window.path)
            .limit(1);
        newest.hint = filter.hint.clone();
        let select = Selection::new([window.path.as_str()]);
        let found = self.find_in(&newest, false, Some(&select), trace)?;
        match found.into_iter().next() {
            Some(found) => Ok(window.newest([&found.decode(Some(&select))?])),
            None => Ok(None),
        }
    }

    /// An aggregate from index entries (SPEC §107.5), into `groups`: where
    /// it has no `sum`, the filter no sort, skip or limit, and one index
    /// holds every path it reads, exactly, and isn't multikey — read in the
    /// range the filter's plan reads, if the entries decide the filter; or,
    /// without conditions, whole, if it holds every document. Each match is
    /// aggregated from its entry where the values it needs are exact, and
    /// from its document otherwise; a leaf whose ends share their group is
    /// counted whole where every column is a `count`. `None` where the
    /// entries can't serve, and nothing was read.
    fn aggregate_by_entries(
        self,
        filter: &Filter,
        groups: &mut Groups,
        trace: &Trace,
    ) -> crate::Result<Option<QueryPlan>> {
        let aggregate = groups.aggregate();
        let picks = filter.skip > 0 || filter.limit.is_some() || filter.sorted();
        if picks || filter.ids().is_some() || aggregate.needs_documents() {
            return Ok(None);
        }
        let paths = aggregate.paths();
        // Each column's condition, decided from the entries where they hold
        // what it reads (SPEC §126.4).
        let conditions = aggregate.column_filters();
        let holds = |index: &IndexMeta| {
            !index.fields.iter().any(|f| crate::query::is_multi(f))
                && paths
                    .iter()
                    .all(|path| index.fields.iter().any(|f| f == path))
                && (conditions.iter().flatten())
                    .all(|condition| Covering::new(condition, Some(index), false).is_some())
        };
        // Among those the hint leaves (SPEC §132.2).
        let indexes = filter.offered(self.indexes());
        // Only an index that holds every path can answer: the planner is
        // offered those, so `tenant == x` averaging `tries` reads `(tenant,
        // tries)` rather than the `tenant` index `find` would take.
        let holding: Vec<IndexMeta> = indexes.iter().filter(|i| holds(i)).cloned().collect();
        let (index, range, bounds, plan) = if filter.conditions.is_empty() {
            // An index that holds every document: a compound one, or one
            // whose entries are as many as the documents (§107.5, rule 4).
            let documents = || self.count_in(filter);
            let mut every: Option<&IndexMeta> = None;
            let group_count = aggregate.group_by.len();
            let leading = |index: &IndexMeta| {
                index.fields.len() >= group_count
                    && (index.fields[..group_count].iter()).all(|f| aggregate.group_by.contains(f))
            };
            let mut candidates: Vec<&IndexMeta> = indexes
                .iter()
                .filter(|index| holds(index) && !index.sparse)
                .collect();
            candidates.sort_by_key(|index| !leading(index));
            for index in candidates {
                let whole = index.is_compound()
                    || Some(index_entries(self.store, index.root)?) == documents()?;
                if whole {
                    every = Some(index);
                    break;
                }
            }
            let Some(index) = every else {
                return Ok(None);
            };
            let plan = QueryPlan::IndexOrder {
                field: index.name(),
            };
            (index, KeyRange::everything(), Vec::new(), plan)
        } else {
            let Some(ranges) = filter.index_ranges(&holding) else {
                return Ok(None);
            };
            let [range] = &ranges[..] else {
                return Ok(None);
            };
            if Covering::new(filter, Some(range.index), false).is_none() {
                return Ok(None);
            }
            let plan = QueryPlan::Index {
                field: range.index.name(),
            };
            (range.index, range.range.clone(), range.by.clone(), plan)
        };
        let fields = index.fields.len();
        let at = |path: &str| index.fields.iter().position(|f| f == path).expect("held");
        let group_at: Vec<usize> = aggregate.group_by.iter().map(|p| at(p)).collect();
        let column_at: Vec<Option<usize>> = (aggregate.columns.iter())
            .map(|c| c.path.as_deref().map(at))
            .collect();
        let counts_only = aggregate.columns.iter().all(|c| c.kind == Kind::Count);
        // The bytes up to the last group field: a leaf whose ends agree on
        // them is in one group (keys are in order, values prefix-free).
        let through = group_at.iter().max().map_or(0, |last| last + 1);
        // A group field cut to a unit (SPEC §209.6): per index field up to
        // the last group field, the cuts its value is grouped by, empty
        // where a group field takes it as it is, whose bytes then decide.
        let cuts_at: Vec<Vec<Cut>> = (0..through)
            .map(|j| {
                let here = || (group_at.iter().enumerate()).filter(move |&(_, &at)| at == j);
                match here().any(|(i, _)| aggregate.cut(i).is_none()) {
                    true => Vec::new(),
                    false => here().filter_map(|(i, _)| aggregate.cut(i)).collect(),
                }
            })
            .collect();
        let cut = |i: usize, value: Document| match aggregate.cut(i) {
            Some(cut) => cut.apply(&value),
            None => value,
        };
        // What tells an entry's group: its key's bytes up to the last
        // group field, a cut field's encoded as cut. `false` for a key
        // with fewer values.
        let group_bytes = |key: &[u8], bytes: &mut Vec<u8>| -> bool {
            bytes.clear();
            let mut parts = key::each_part(key::value_part(key));
            for cuts in &cuts_at {
                let Some(part) = parts.next() else {
                    return false;
                };
                match cut_value(part, fields) {
                    Some(value) if !cuts.is_empty() => {
                        for cut in cuts {
                            match key::encode_for(&cut.apply(&value), fields) {
                                Some(encoded) => bytes.extend_from_slice(&encoded),
                                None => return false,
                            }
                        }
                    }
                    _ => bytes.extend_from_slice(part),
                }
            }
            true
        };
        // Whether every key from `low` to `high` is in one group: their
        // group bytes the same, and where a cut field's value differs, it
        // the last group field and both ends of one type, so every value
        // between is of that type too (a key's type comes first, §34.1) and
        // cuts to the same bucket, a cut keeping order (§209.6).
        let (mut low_bytes, mut high_bytes) = (Vec::new(), Vec::new());
        let mut one_group = |low: &[u8], high: &[u8]| -> bool {
            if !group_bytes(low, &mut low_bytes)
                || !group_bytes(high, &mut high_bytes)
                || low_bytes != high_bytes
            {
                return false;
            }
            let lows = key::each_part(key::value_part(low));
            let highs = key::each_part(key::value_part(high));
            let differs = lows.zip(highs).take(through).position(|(a, b)| a != b);
            match differs {
                None => true,
                Some(j) => {
                    let kind = |key: &[u8]| {
                        let part = key::each_part(key::value_part(key)).nth(j)?;
                        cut_value(part, fields).map(|v| std::mem::discriminant(&v))
                    };
                    j + 1 == through
                        && !cuts_at[j].is_empty()
                        && kind(low).is_some()
                        && kind(low) == kind(high)
                }
            }
        };
        let mut entry_bytes = Vec::new();
        // One cut field, the last group field, the only one at its index
        // field (SPEC §212): a key of its bucket told by its bytes, those
        // before the cut field the bucket's and the cut field's below the
        // bucket's end, as keys come in order. Set where a group begins.
        let single_cut = match cuts_at.iter().position(|cuts| !cuts.is_empty()) {
            Some(j) if j + 1 == through && cuts_at[j].len() == 1 => Some((j, cuts_at[j][0])),
            _ => None,
        };
        let bucket_of = |key: &[u8]| -> Option<Bucket> {
            let (j, cut) = single_cut?;
            let value_part = key::value_part(key);
            let mut parts = key::each_part(value_part);
            let before: usize = (0..j)
                .map(|_| parts.next().map(<[u8]>::len))
                .sum::<Option<_>>()?;
            let end = cut.end(&cut_value(parts.next()?, fields)?)?;
            Some(Bucket {
                before: value_part[..before].to_vec(),
                end: key::encode_for(&end, fields)?,
            })
        };
        let mut bucket: Option<Bucket> = None;
        let mut decider = Covering::new(filter, Some(index), false).map(|c| c.decider(filter));
        let mut column_deciders: Vec<Option<Decider>> = (conditions.iter())
            .map(|condition| {
                condition.as_ref().map(|condition| {
                    let covering = Covering::new(condition, Some(index), false);
                    covering.expect("held").decider(condition)
                })
            })
            .collect();
        // Which columns an entry goes to: every one without conditions, else
        // decided into a list kept from entry to entry.
        let every_column = vec![true; aggregate.columns.len()];
        let conditional = column_deciders.iter().any(Option::is_some);
        let mut takes: Vec<bool> = Vec::with_capacity(aggregate.columns.len());
        let read = trace.read(index.name(), bounds, false);
        if decider.is_some() || filter.conditions.is_empty() {
            trace.covering(read);
        }
        let matcher = filter.matcher();
        let mut records = data::Records::new(self.store, self.catalog.names());
        let mut walk = BTreeIndex::new(index.root).walk(self.store, range, false);
        // The bytes of the last entry's key up to its last group value, when
        // that entry went to a group from its key.
        let mut last_group: Option<Vec<u8>> = None;
        // A value from a key, and whether it is the value as stored.
        let value_of = |part: &[u8]| -> Option<(Document, bool)> {
            if !key::part_is_exact(part, fields) {
                return None;
            }
            let mut value = Document::Null;
            key::decode_part_into(part, fields, &mut value)?;
            let stored = !matches!(value, Document::Float(_));
            Some((value, stored))
        };
        loop {
            if counts_only
                && !group_at.is_empty()
                && let Some(leaf) = walk.whole_leaf()?
            {
                let decided = match &mut decider {
                    Some(decider) => decider.decide_all(leaf.lowest, leaf.highest),
                    None => Some(true),
                };
                let group = match decided {
                    Some(true) if one_group(leaf.lowest, leaf.highest) => {
                        let parts = key::parts(key::value_part(leaf.lowest));
                        (group_at.iter().enumerate())
                            .map(|(i, &at)| {
                                let (value, stored) = value_of(parts[at])?;
                                stored.then(|| cut(i, value))
                            })
                            .collect::<Option<Vec<Document>>>()
                    }
                    _ => None,
                };
                // Each condition for the whole leaf, or the leaf walked.
                let takes = group.as_ref().and_then(|_| {
                    (column_deciders.iter_mut())
                        .map(|decider| match decider {
                            Some(decider) => decider.decide_all(leaf.lowest, leaf.highest),
                            None => Some(true),
                        })
                        .collect::<Option<Vec<bool>>>()
                });
                if let (Some(group), Some(takes)) = (group, takes) {
                    let n = leaf.entries;
                    let order = Order::id(key::doc_id(leaf.lowest));
                    trace.entries(read, n);
                    trace.matched(n);
                    (0..n).for_each(|_| trace.answered_from_entry());
                    groups.add_counted(group, n as u64, &takes, order, None);
                    last_group =
                        group_bytes(leaf.lowest, &mut entry_bytes).then(|| entry_bytes.clone());
                    bucket = bucket_of(leaf.lowest);
                    #[cfg(test)]
                    LEAVES_COUNTED.with(|c| c.set(c.get() + 1));
                    walk.pass_leaf();
                    continue;
                }
            }
            let Some(entry) = walk.step() else {
                break;
            };
            let (key, loc) = entry?;
            trace.entries(read, 1);
            let decided = match &mut decider {
                Some(decider) => decider.decide(key),
                None => Some(true),
            };
            if decided == Some(false) {
                continue;
            }
            let parts = key::parts(key::value_part(key));
            let values = (decided == Some(true) && parts.len() == fields).then(|| {
                (column_at.iter())
                    .map(|at| match at {
                        None => Some(None),
                        Some(i) => value_of(parts[*i]).map(Some),
                    })
                    .collect::<Option<Vec<_>>>()
            });
            let id = key::doc_id(key);
            // Each condition on the key; one it can't tell reads the document.
            let decided: Option<&[bool]> = match &values {
                Some(Some(_)) if !conditional => Some(&every_column),
                Some(Some(_)) => {
                    takes.clear();
                    let told = (column_deciders.iter_mut()).try_for_each(|decider| {
                        takes.push(match decider {
                            Some(decider) => decider.decide(key)?,
                            None => true,
                        });
                        Some(())
                    });
                    told.map(|()| &takes[..])
                }
                _ => None,
            };
            if let Some(Some(values)) = values
                && let Some(takes) = decided
            {
                // Entries come in key order: one in the last one's bucket,
                // or whose bytes up to the last group value, cut where its
                // field is, are the last one's, is in its group.
                if last_group.is_some() && bucket.as_ref().is_some_and(|b| b.holds(key)) {
                    trace.matched(1);
                    trace.answered_from_entry();
                    groups.add_entry_to_last(values, takes, Order::id(id), loc);
                    continue;
                }
                let told = group_bytes(key, &mut entry_bytes);
                if told && last_group.as_deref() == Some(&entry_bytes[..]) {
                    trace.matched(1);
                    trace.answered_from_entry();
                    groups.add_entry_to_last(values, takes, Order::id(id), loc);
                    continue;
                }
                let group = (group_at.iter().enumerate())
                    .map(|(i, &at)| {
                        value_of(parts[at]).map(|(value, stored)| (cut(i, value), stored))
                    })
                    .collect::<Option<Vec<_>>>();
                if let (Some(group), true) = (group, told) {
                    trace.matched(1);
                    trace.answered_from_entry();
                    groups.add_entry(group, values, takes, Order::id(id), loc);
                    let last = last_group.get_or_insert_with(Vec::new);
                    last.clear();
                    last.extend_from_slice(&entry_bytes);
                    bucket = bucket_of(key);
                    continue;
                }
            }
            // No group from a key: the next entry finds its own, its bucket
            // too (a bucket holds only while `last_group` does).
            last_group = None;
            // An entry that can't tell: its document, checked unless the
            // entry decided.
            trace.examined();
            let doc = records.stored(loc)?.decode()?;
            if decided.is_none() && !matcher.matches(&doc)? {
                continue;
            }
            trace.matched(1);
            groups.add_document(&doc, Order::id(id), None)?;
        }
        Ok(Some(plan))
    }

    /// `count`'s conditions counted entry by entry as an index is walked,
    /// nothing collected (SPEC §101.4): with no conditions, the primary
    /// index's entries, leaf by leaf; with conditions one range of one index
    /// covers (§100), each entry decided by its run of equal values and each
    /// leaf inside the range whole where its ends decide it, and only an
    /// entry that can't decide read. `None` for every other filter, which
    /// `read_candidates` counts: a lookup by id, a union of ranges, a
    /// multikey index, a filter the entries don't cover.
    fn count_in(self, filter: &Filter) -> crate::Result<Option<usize>> {
        let Some(meta) = self.meta() else {
            return Ok(Some(0));
        };
        if filter.conditions.is_empty() {
            let primary = BTreeIndex::new(meta.index_root);
            return Ok(primary
                .count(self.store, KeyRange::everything())
                .map(Some)?);
        }
        if filter.ids().is_some() {
            return Ok(None);
        }
        let (root, range, index) = match filter.index_ranges(self.indexes()) {
            None => (meta.index_root, KeyRange::everything(), None),
            Some(mut ranges) if ranges.len() == 1 => {
                let range = ranges.pop().expect("one");
                if range.index.fields.iter().any(|f| crate::query::is_multi(f)) {
                    return Ok(None);
                }
                (range.index.root, range.range, Some(range.index))
            }
            Some(_) => return Ok(None),
        };
        let Some(covering) = Covering::new(filter, index, false) else {
            return Ok(None);
        };
        let mut decider = covering.decider(filter);
        let matcher = filter.matcher();
        let names = filter.fields_with(false);
        let mut records = data::Records::new(self.store, self.catalog.names());
        let mut walk = BTreeIndex::new(root).walk(self.store, range, false);
        let mut count = 0;
        loop {
            let whole = walk
                .run(usize::MAX)?
                .map(|leaf| (decider.decide_all(leaf.lowest, leaf.highest), leaf.entries));
            if let Some((Some(matches), entries)) = whole {
                count += if matches { entries } else { 0 };
                walk.pass_leaf();
                continue;
            }
            let Some(entry) = walk.step() else {
                return Ok(Some(count));
            };
            let (key, loc) = entry?;
            let matches = match decider.decide(key) {
                Some(decided) => decided,
                None => {
                    let fields = records.stored(loc)?.fields(names.as_deref())?;
                    matcher.matches(&fields)?
                }
            };
            count += usize::from(matches);
        }
    }
}

/// What an aggregate decodes of a document (SPEC §107.3): the group
/// and column paths, and the fields each column's condition reads
/// (§126.3) — the whole document where one starts at the document
/// itself.
fn aggregate_selection(aggregate: &Aggregate) -> Selection {
    let filters = aggregate.column_filters();
    let mut paths = aggregate.paths();
    for filter in filters.iter().flatten() {
        match filter.fields() {
            Some(fields) => paths.extend(fields),
            None => paths.push(""),
        }
    }
    Selection::new(paths)
}

#[cfg(test)]
thread_local! {
    /// Leaves an aggregate counted whole (SPEC §107.5), on this thread.
    pub(super) static LEAVES_COUNTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A cut field's bucket in an index walked in order (SPEC §212): the
/// bytes of the key's values before the cut field, and the cut field's
/// end encoded as a key encodes it.
struct Bucket {
    before: Vec<u8>,
    end: Vec<u8>,
}

impl Bucket {
    /// Whether `key`, at or after a key of the bucket, is in it: the same
    /// values before the cut field (values are prefix-free) and the cut
    /// field's below the end, which keeps out every later kind too, as a
    /// key's kind comes first.
    fn holds(&self, key: &[u8]) -> bool {
        let value_part = key::value_part(key);
        value_part.starts_with(&self.before)
            && key::each_part(&value_part[self.before.len()..])
                .next()
                .is_some_and(|part| part < &self.end[..])
    }
}

/// The value a key's `part` holds where a cut changes it, a date-time
/// or a date, both exact in a key (SPEC §209.6); `None` for any other,
/// which a cut leaves as it is, so its bytes tell its group.
fn cut_value(part: &[u8], fields: usize) -> Option<Document> {
    if !key::part_is_exact(part, fields) {
        return None;
    }
    let mut value = Document::Null;
    key::decode_part_into(part, fields, &mut value)?;
    matches!(value, Document::DateTime(_) | Document::Date(_)).then_some(value)
}

/// How many entries the index at `root` has, counted leaf by leaf
/// (SPEC §101.4).
fn index_entries(store: &dyn PageStore, root: PageId) -> std::io::Result<usize> {
    let mut walk = BTreeIndex::new(root).walk(store, KeyRange::everything(), false);
    let mut n = 0;
    loop {
        if let Some(leaf) = walk.run(usize::MAX)? {
            n += leaf.entries;
            walk.pass_leaf();
            continue;
        }
        match walk.step() {
            Some(entry) => {
                entry?;
                n += 1;
            }
            None => return Ok(n),
        }
    }
}
