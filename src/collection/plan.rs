//! Which plan a read follows: the index it uses, the alternatives
//! `explain` weighs, and the hints it refuses.

use super::find::find_in;
use crate::catalog::{Catalog, IndexMeta};
use crate::explain::{Alternative, EntryCount, Trace};
use crate::index::{BTreeIndex, KeyRange};
use crate::query::{Covering, Filter, QueryPlan};
use crate::storage::{PageId, PageStore};

/// The plan `find` follows for `filter` among a collection's `indexes`
/// — what `explain` says.
pub(super) fn plan_for(filter: &Filter, indexes: &[IndexMeta]) -> QueryPlan {
    if filter.ids().is_some() {
        return QueryPlan::ById;
    }
    if let Some(order) = filter.index_order(indexes) {
        return QueryPlan::IndexOrder {
            field: order.name(),
        };
    }
    if let Some(ranges) = filter.merged_by_id(indexes) {
        return QueryPlan::IndexUnion {
            fields: ranges.iter().map(|range| range.index.name()).collect(),
            in_order: true,
        };
    }
    match filter.index_reads(indexes) {
        None => QueryPlan::Scan,
        Some(reads) => match (&reads.ids[..], &reads.ranges[..]) {
            ([], [range]) => QueryPlan::Index {
                field: range.index.name(),
            },
            // Ids looked up beside the ranges: the primary index first
            // (SPEC §164.4).
            (ids, ranges) => QueryPlan::IndexUnion {
                fields: (!ids.is_empty())
                    .then(|| crate::document::ID_FIELD.to_string())
                    .into_iter()
                    .chain(ranges.iter().map(|range| range.index.name()))
                    .collect(),
                in_order: false,
            },
        },
    }
}

/// How many leaves an alternative's index ranges are counted through
/// (SPEC §133.3): 8 MB of pages, some 100,000 entries or more. A few in
/// the unit tests, which reach it with a few thousand documents.
#[cfg(not(test))]
const ALTERNATIVE_LEAVES: usize = 1_000;
#[cfg(test)]
const ALTERNATIVE_LEAVES: usize = 4;

/// The plans `explain_run` passed over (SPEC §133): `filter` hinted to
/// each index, to `_id` and to a scan, and without its hint if it has
/// one — each the planner's own plan for that hint (§132.2) — less the
/// hints it can't follow and the plans already listed, `plan` first,
/// the one it ran, which read `read` entries.
pub(super) fn alternatives(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
    plan: &QueryPlan,
    read: u64,
) -> crate::Result<Vec<Alternative>> {
    let indexes = catalog.indexes(collection);
    let mut hinted: Vec<Filter> = Vec::new();
    if filter.hint.is_some() {
        hinted.push(Filter {
            hint: None,
            ..filter.clone()
        });
    }
    hinted.extend(
        indexes
            .iter()
            .map(|i| filter.clone().use_index(i.fields.clone())),
    );
    hinted.push(filter.clone().use_index(crate::document::ID_FIELD));
    hinted.push(filter.clone().no_index());
    let mut listed = vec![plan.clone()];
    let mut found = Vec::new();
    for hinted in hinted {
        if follow_hint(collection, &hinted, indexes).is_err() {
            continue;
        }
        let other = plan_for(&hinted, indexes);
        if listed.contains(&other) {
            continue;
        }
        listed.push(other.clone());
        let (entries, reads_documents) =
            would_read(catalog, store, collection, &hinted, &other, read)?;
        found.push(Alternative {
            plan: other,
            fewer: matches!(entries, EntryCount::Exactly(n) if n < read),
            entries,
            reads_documents,
        });
    }
    Ok(found)
}

/// What `filter`, run as `plan`, would read (SPEC §133.3, §133.4): its
/// index entries, and whether it reads a document for each. A range or
/// a scan counted a leaf at a time, through `ALTERNATIVE_LEAVES` at
/// most; ids by how many; a walk in sort order walked, as far as `read`.
fn would_read(
    catalog: &Catalog,
    store: &dyn PageStore,
    collection: &str,
    filter: &Filter,
    plan: &QueryPlan,
    read: u64,
) -> crate::Result<(EntryCount, bool)> {
    let indexes = catalog.indexes(collection);
    let checks = |index: Option<&IndexMeta>| Covering::new(filter, index, false).is_none();
    let Some(meta) = catalog.get(collection) else {
        return Ok((EntryCount::Exactly(0), false));
    };
    let counted = |ranges: Vec<(PageId, KeyRange)>| -> std::io::Result<EntryCount> {
        let (mut entries, mut leaves) = (0, 0);
        for (root, range) in ranges {
            let left = ALTERNATIVE_LEAVES.saturating_sub(leaves);
            let counted = BTreeIndex::new(root).count_at_most(store, range, left)?;
            entries += counted.entries as u64;
            leaves += counted.leaves;
            if !counted.whole {
                return Ok(EntryCount::MoreThan(entries));
            }
        }
        Ok(EntryCount::Exactly(entries))
    };
    Ok(match plan {
        QueryPlan::ById => {
            let ids = filter.ids().map_or(0, |ids| ids.len());
            (EntryCount::Exactly(ids as u64), checks(None))
        }
        QueryPlan::Scan => {
            let all = vec![(meta.index_root, KeyRange::everything())];
            (counted(all)?, checks(None))
        }
        QueryPlan::IndexOrder { .. } | QueryPlan::IndexUnion { in_order: true, .. } => {
            let reads = match filter.index_order(indexes) {
                Some(order) => checks(order.index),
                None => {
                    let ranges = filter.merged_by_id(indexes).expect("a merge's plan");
                    let first = ranges[0].index;
                    !ranges.iter().all(|r| std::ptr::eq(r.index, first)) || checks(Some(first))
                }
            };
            let trace = Trace::with_budget(read);
            find_in(catalog, store, collection, filter, false, None, &trace)?;
            let entries = match trace.spent() {
                true => EntryCount::MoreThan(read),
                false => EntryCount::Exactly(trace.entries_read()),
            };
            (entries, reads)
        }
        _ => {
            let crate::query::IndexReads { ids, ranges } =
                filter.index_reads(indexes).expect("a range's plan");
            let reads = match &ranges[..] {
                _ if !ids.is_empty() => true,
                [] => false,
                [first, rest @ ..] if rest.iter().all(|r| std::ptr::eq(r.index, first.index)) => {
                    checks(Some(first.index))
                }
                _ => true,
            };
            let ranges = ranges
                .into_iter()
                .map(|r| (r.index.root, r.range))
                .collect();
            // Ids by how many, as for `ById` (SPEC §164.4).
            let entries = match counted(ranges)? {
                EntryCount::Exactly(n) => EntryCount::Exactly(n + ids.len() as u64),
                EntryCount::MoreThan(n) => EntryCount::MoreThan(n + ids.len() as u64),
            };
            (entries, reads)
        }
    })
}

/// Refuses a hint the plan can't follow (SPEC §132.3), before anything
/// is read: an index the collection lacks, or one the rules, offered it
/// alone, would not read — a scan instead. Every read checks here, so
/// that each refuses what `explain` refuses.
pub(super) fn follow_hint(
    collection: &str,
    filter: &Filter,
    indexes: &[IndexMeta],
) -> crate::Result<()> {
    let Some(index) = filter.hinted_index() else {
        return Ok(());
    };
    let reason = if index != crate::document::ID_FIELD && filter.offered(indexes).is_empty() {
        "the collection has no such index"
    } else if plan_for(filter, indexes) == QueryPlan::Scan {
        "the filter has no condition it can bound, and no sort with a limit it can give the order of"
    } else {
        return Ok(());
    };
    Err(crate::Error::Hint {
        collection: collection.to_string(),
        index,
        reason: reason.to_string(),
    })
}
