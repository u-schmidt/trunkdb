# 102. Paths resolved once per query (`path.rs`, `query.rs`, `collection.rs`, `cursor.rs`)

A filter names its fields by path: `status`, `address.city`,
`tags[*]` (§31, §42). Each time a condition was checked against a
document, or a sort compared two documents, the path was taken apart
again:
- **`is_multi`:** `path.contains("[*]")`, a substring search set up on
  every call;
- **`field_value`:** `path.split('.')`, a character search;
- **`walk`** (a path with `[*]`): split and trimmed every step, with a
  `Vec` per step;
- **each step's lookup:** `IndexMap::get`, which hashes the name with
  SipHash: 8–12 ns on an object of 1–16 fields, where scanning up to 7
  fields for the name takes about 5 ns (measured for this section, short
  names, the name last);
- **a sort in memory:** `compare_by` looked each key's path up in both
  documents on every comparison, about 2 · n · log₂ n lookups where n
  would do.

The same applied where §100's entries decide. `Covering::fill` put each
named value into a scratch document through `slot`, which split the
path and hashed the name two or three times. The filter then took the
path apart again to check that document.

Measured with a probe built like the benchmark (§48), with 100,000 tasks
and its three indexes, each query in a loop for 5 s under `sample`.
Inclusive shares of the time:

| | time | path search (`contains`, `split`) | lookups (hashing, `slot`) | sort |
|---|---:|---:|---:|---:|
| `count` of created in a range (1,000) | 141 µs | 34% | ~20% | — |
| tenant == x by id, first 20 | 574 µs | 2% | ~20% | 22% |
| tenant == x by title, first 20 | 1,059 µs | 6% | ~20% | 38% |
| scan: `count` of tries > 7, unindexed | 55.7 ms | 3% | 3% | — |

The range count was where §101 left trunkdb furthest behind: 0.14 ms,
against 0.02–0.04 ms for SQLite and redb. §102 parses each path once
per query, finds a field in a small object without hashing, and looks a
sort's keys up once per document. It was the ROADMAP's "a path looked
up once per filter, not per document" (§101.8).

## 102.1 A path parsed once
`Path` (`path.rs`, crate-internal) is a path taken apart. The common
case, one field name with no `.` or `[*]`, is held as it is, with no
allocation (`Path::Field`). Anything else is held as steps
(`Path::Steps`). Each step keeps:
- its text as written, which `field_value` looks up;
- its name without the `[*]`s at its end, which `values` looks up;
- how many `[*]` follow it.

The path also records whether it holds a `[*]` anywhere, as `is_multi`
says.

Its methods follow the old rules exactly:
- `field_value` and `value_or_null`;
- `values`, the old `values_at`;
- `present`, the old `present_at`;
- `slot`, the value at the path made if missing, for §100's entry
  documents.

A path of one field never builds a `Vec` for `value_or_null`.

The free functions on a string path (`field_value`, `value_or_null`,
`values_at`) stay for code that looks a path up once: index maintenance,
`check`, updates, the unordered tail of a walk. They parse and delegate,
so the rules have one implementation.

## 102.2 A filter compiled once per read
`Matcher` (`query.rs`, crate-internal) mirrors the conditions, with every
path parsed. It borrows the filter's values instead of cloning them, so
building one costs only the parsed paths: nothing for a filter on plain
fields beyond the `Vec` of its conditions. A `Contains` keeps its
string case-folded once (`needle`), instead of folding it again for
every document.

`Filter::matcher()` builds one, and `Matcher::matches` answers what
`Filter::matches` answered. Every read builds one and checks every
document with it:
- `count`, `count_in`, `upsert`, `find_in`;
- the walk in index order and the scan after it;
- the streaming cursor, once per step;
- `apply_to`, once per call;
- the `Decider` (§101.2).

`Filter::matches` and `Condition::matches` stay public and keep their
answers. They now compile for the one document, which costs about what
taking the paths apart cost.

## 102.3 A field found without hashing
`Document::Object` is an `IndexMap<String, Document>` with the default
hasher, and that type is public. The map stays. A step finds its name by
comparing names in order on an object of up to 8 fields, and with
`IndexMap::get` beyond that (`path::get`, `path::get_or_insert`). Both
find the same entry, as names in a map are unique.

8 comes from the measurement above: the scan won at 7 fields (5.1
against 8.4 ns) and lost at 10 (17.4 against 9.3). Most objects a
filter checks are small:
- a candidate is decoded to the fields its filter names (§99.2);
- §100's scratch document holds only the named ones;
- the benchmark's tasks have 7 fields with `_id`.

## 102.4 Entries decided through parsed paths
`Covering` parses its named fields' paths when it's made. `fill` writes
each value through `Path::slot`, finding each step by 102.3, and sets
`_id` the same way. The scratch document is reused from entry to entry,
so after the first one every step finds its object already there. The
`Decider` checks the scratch document with its matcher.

An entry kept for a sort in memory (§100.3) holds only the sort keys'
values (`Covering::for_sort`), and `_id` only where the sort names it.
The conditions' values are left out: the entry has decided them
already, and only the sort reads that document afterwards. "tenant == x
by id" builds a document with `_id` alone for each of its 1,000 entries,
instead of `_id` and a decoded `tenant` string.

## 102.5 A sort that looks its keys up once
`arrange` (so `apply_to`, `apply`, and every sort in memory) looks up
each item's sort-key values once, through the keys' parsed paths, into
one flat `Vec` of n · k references. It then sorts the items' positions
by them (`compare_values`, the comparison `compare_by` made: per key,
`sort_order`). `sort_by` is stable, so items equal in every key keep the
order they came in.

Only the items the skip and limit leave are then moved out, in order.
A sort of 1,000 matches for a page of 20 moves 20.

## 102.6 What stays
- **Every result, count and order.** The matcher answers as `Filter`
  did, and the sort compares as `compare_by` did, which the test module
  keeps as the reference (102.8).
- **The API, the file format and every write.**
- **`Filter` and `Condition` as values.** Nothing is cached in them:
  `Filter` is built field by field in places (`Filter { skip: 0,
  ..filter.clone() }`), and a cache there could go stale.

## 102.7 Rejected
- **Caching parsed paths in `Condition`:** its variants have public
  fields, so adding one breaks every caller who builds or matches a
  `Condition::Compare`.
- **Another hasher for `Document::Object`** (FxHash, aHash): it changes
  a public type, and code naming that type would break.
- **Matching on the encoded key bytes** (an `Eq` as `memcmp`, a range as
  two comparisons), without decoding an entry's values. It would be
  faster still for a range count, but it is a second implementation of
  every comparison (type order, `Int` against `Float`, `Contains`)
  beside `value_matches`. Worth it only if the range count still lags
  (102.10).
- **A matcher over an entry's values without a document**, each path
  resolved to an index field: it saves the scratch document, which
  102.4 makes cheap, at the cost of a second kind of matcher.
- **`sort_by_cached_key`:** it needs owned keys, so it would clone every
  sort value.
- **An owned `Matcher`** that a cursor could keep: it would clone the
  filter's values. A cursor builds a borrowed one on each step instead,
  which costs nothing for plain fields.

## 102.8 Tests
The test module of `query.rs` keeps the code from before §102
(`before_102`) as the reference: paths taken apart on every call, every
step hashed, `Condition::matches` as it was, and `compare_by`. Random
documents for these tests are nested objects and arrays, with field
names a path can't reach (`a.b`) or reads as a step's end (`[*]`,
`a[*]`), and strings that only match ignoring case (`Straße`,
`STRASSE`).
- **Paths** (`a_parsed_path_finds_what_the_string_did`): 4,000 random
  paths, and fixed odd ones (`""`, `"."`, `".."`, `"[*]"`, `"a."`,
  `"a[*]b"`), on random documents. `field_value`, `values` and
  `present` give the same values of the document, compared by where
  they are, not by equality (NaN isn't equal to itself); `is_multi`
  agrees with `contains("[*]")`.
- **Matching** (`a_matcher_answers_as_the_conditions_did`): 4,000
  random conditions, nested three deep, with every op, `exists`,
  `size` and `elem_match`, each on five documents. `Matcher` and the
  public `Condition::matches` answer as the reference does, and both
  answers come up often.
- **Lookups** (`a_field_is_found_by_scan_or_by_hash_alike`):
  - objects of 0 to 20 fields, names found and not: `get` gives what
    `IndexMap::get` gives;
  - objects whose names are each other's prefixes (`k`, `kk`, ...), up
    to 12: `get_or_insert` changes and adds as `IndexMap`'s entry does,
    in the same order.
- **Sorting** (`a_sort_on_values_looked_up_once_orders_as_before`): 600
  random lists of up to 60 documents, one to three keys among `a`, `b`,
  `a.b`, `c[*]` and `""`, both orders, skips and limits. `arrange`
  gives what a stable `sort_by` with the reference `compare_by` gives.
- **Work done** (`paths_are_parsed_and_looked_up_once`): `apply` of a
  filter with two conditions and two sort keys over 1,000 documents
  parses four paths. It looks up one field per condition checked (the
  second only where the first held), and two per match sorted.
- **The corpora** (§36's nested filters, §42, §45, §46, §47, §72, §96,
  §100, §101, compound, sparse, multikey) pass unchanged.

Fourteen hand-made mutations, all caught:
- a path with `[*]` held as one plain field;
- a missing field not read as null;
- `present` reading a missing field as null;
- a scan matching a name's prefix, in `get` and in `get_or_insert` (the
  second needed the prefix names, added after it first survived);
- a nested slot written at the top;
- sort values read with the wrong stride;
- the sort made unstable;
- only the first sort key compared;
- an entry for a sort lacking its sort keys, or lacking `_id`;
- a `Contains` string not case-folded;
- `Ne` on `[*]` taken as "any value unequal";
- a cursor not checking its undecided candidates.

## 102.9 Measurements
Benchmark (§48), no new rows. trunkdb before (`47a5d33`) and after, back
to back, three runs each, on 100,000 documents:

| | before | after |
|---|---:|---:|
| `count` created in a range (1,000) | 0.14–0.15 ms | 0.07 ms |
| tenant == x by id, first 20 | 587–745 µs | 378–401 µs |
| find tenant == x (1,000) | 0.97–0.98 ms | 0.91–0.98 ms |
| find created in a range (1,000) | 1.03–1.09 ms | 0.99–1.06 ms |
| scan: tries > 7, unindexed | 60.4–64.1 ms | 58.7–64.2 ms |

Every other row stays within its noise. SQLite and redb, in the same
runs, count a range in 0.02 and 0.04 ms, so the range count is now
2–3.5× theirs instead of 5–7×. Most of what is left is decoding each
entry's values into the scratch document, which matching on the
encoded bytes would avoid (102.7).

The probe agrees: the range count went from 141 to 67 µs, "tenant == x
by id" from 574 to 377 µs, find tenant == x from 1,018 to 770 µs, and
"tenant == x by title" from 1,059 to 818 µs. The scan moved within its
noise (54.6–67.7 ms over runs): decoding is most of it (§99).

## 102.10 Limits
- **Objects over 8 fields** still hash every step.
- **A range count still decodes each entry's values** into a scratch
  document and checks it (102.7).
- **Updates and index maintenance** take their paths apart as before:
  once per document per path, where parsing is not what costs.
- **`Filter::matches` called by hand in a loop** compiles every time.
  `apply` and `apply_to` don't.
- **"tenant == x by id"** still sorts the tenant's matches in memory.
  Getting near redb's 19 µs needs the walk of an `Eq`-fixed index
  (ROADMAP).
