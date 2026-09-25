# 14. Query: sort and limit (`query.rs`)

Real and tested: `Filter` gained `sort: Option<Sort>` and `limit:
Option<usize>`, and a new `Filter::apply(docs)` method — the single place
query semantics now live. `Collection::find` no longer filters
documents itself; it just gathers every candidate (via `index.scan()` +
`data::get_record`) and hands the whole set to `apply`, which filters,
then sorts, then limits, in that order (matching SQL's `WHERE` -> `ORDER
BY` -> `LIMIT` — sorting an already-filtered set is both correct and
cheaper than sorting everything first). This was the concrete gap
the time-series workload (§5.1) was expected to surface: its query
patterns need "most recent row" (`ORDER BY tst DESC LIMIT 1`), which
`Filter` couldn't express before this.

A document missing the sort field, or one whose field type doesn't
compare against the other side's, doesn't make `apply` error — it
compares as `Equal`, so it just doesn't move relative to whatever it's
being compared against (`sort_by`'s stability preserves the rest of the
original order). Matches the project's general stance of trusting the
caller rather than inventing validation for a case a schema-less document
store can't really define as "wrong" anyway. (Since §34 there's a fixed
order across kinds instead: null and missing first, values nothing
orders last.)

Since §23, `apply` delegates to `apply_to(items, doc_of)`, the same
pipeline for items that *carry* a document — `find_with_ids` runs it
over `(DocId, Document)` pairs.
