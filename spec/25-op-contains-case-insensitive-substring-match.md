# 25. `Op::Contains`: case-insensitive substring match (`query.rs`)

Real and tested: `Op::Contains` matches when the field is a string that
contains the condition's string value, ignoring case — what the sync
workload's search does with MongoDB's `{"$regex": ..., "$options": "i"}`
(§5.3). It composes with the other conditions (flat AND), sort and
limit like any `Op`.

## 25.1 Case-insensitive by definition, not by a flag
There is one `Contains`, and it ignores case — no case-sensitive variant
and no options field on `Condition`. Every known use (search fields) is
case-insensitive; LiteDB's string comparisons ignore case by default
too. A case-sensitive variant can be added as its own `Op` if something
needs it, without changing this one. The other ops (`Eq`, `Lt`, …) stay
case-sensitive, as before.

## 25.2 What "ignoring case" means
Both sides go through `fold_case`: Rust's Unicode `to_lowercase`, then
`ß` → `ss`. Lowercasing alone handles umlauts (`MÜLLER` ~ `müller`) but
leaves `ß` alone, so "Straße" wouldn't contain "STRASSE" — a likely
search in German text. Full Unicode case folding (the `CaseFolding.txt`
table) would cover the remaining rare cases, but needs a dependency;
not worth it yet. Folding isn't accent-stripping: `muller` doesn't match
`Müller`, which is what a user typing the umlaut expects.

## 25.3 Edge cases
- Anything but a string on either side (a number field, a numeric
  needle) doesn't match — no implicit conversion, consistent with how
  `compare` treats mismatched types.
- A missing field doesn't match, as for every other op.
- An empty needle matches every string field (standard `contains`
  semantics) — so an empty search box can be passed through unchanged,
  though skipping the condition is cheaper.

## 25.4 Deliberately not regex
A pattern language (regex, `LIKE` wildcards) is still open (ROADMAP.md). A
plain substring covers the search boxes, has no syntax to escape user
input for, and can't be made pathologically slow by a pattern.
Performance is a scan anyway (§4.3): each candidate's field is folded
per query — an allocation per document, negligible at the sync workload's size.
