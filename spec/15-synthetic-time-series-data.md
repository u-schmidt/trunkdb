# 15. Synthetic time-series data (`tests/timeseries_shape.rs`)

The validation step the old "Open work" item 1 called for: a new
integration test (`tests/`, not `src/`) that only uses trunkdb's public
API — `Database`, `Collection<T>`, `query::Filter` — the way an actual
application would, rather than reaching into crate internals.
`LocationPing` and `Geofence` mirror the time-series workload's data
(§5.1); `Collection<LocationPing>` and
`Collection<Geofence>` are real, independent, typed collections in the
same database file.

It exercises exactly the query patterns §5.1 identified as real and not
yet provable before `Filter::apply` existed (§14):
- a time-range filter (`tst >= ? AND tst <= ?`) combined with an ascending
  sort,
- "most recent row" (`ORDER BY tst DESC LIMIT 1`),
- several optional filters built up dynamically and ANDed together (the
  shape a real query endpoint uses when not every filter param is always
  supplied).

Deliberately not exercised, because §5.1 already scoped them out of v0
rather than leaving them as an oversight: the `zone_key` unique
constraint (v0 has no constraint system at all) and the `LEFT JOIN` +
`GROUP BY` + `COUNT` aggregate (that stays application-level code on top
of `find`, not something `Filter` should grow support for). All 4 new
tests pass against the real on-disk storage stack, no mocking.
