# 100. Index entries that answer the filter (`query.rs`, `collection.rs`, `cursor.rs`, `index/key.rs`, `explain.rs`)

Every candidate an index gave `find` was read from the data pages and
checked against the filter, even when its index entry already said
whether it matched (§28.3). An entry's key holds the indexed values and
the document's id. Where the filter looks at nothing else, and those
values are exact, the document adds nothing to the decision.

Three reads paid for that:
- **`count` with conditions** read every candidate document: `count` of
  `tenant == x` read 1,000 documents to count 1,000 entries.
- **A deep page** (§72.3) read every document it skipped. Page 50 of
  20, sorted by `created` on `(status, created)`, read 1,000 documents
  to hand out 20. The default list sorted by `_id` (§96) did the same.
- **A sort in memory by `_id`** after an index range (`tenant == x`,
  sorted by `_id`, `limit(20)`) read every candidate, to sort by an id
  its entry holds.

§100 lets an index entry decide its document's match where it can. A
document is read only where its entry can't decide, where the sort
needs a field the key doesn't hold, or to be handed out.

SQL calls this a covering index or an index-only scan, MongoDB a
covered query. Here it isn't a plan of its own: it is decided per read,
and per entry. It was the ROADMAP's "index-only reads", and §72.3's
rejected "skipping index entries without reading their documents".

## 100.1 When an index answers the filter
A read of an index **covers** a filter (`Filter::covered_by`,
`Covering::new`) when every path its conditions name is one of the
index's fields or `_id`:
- **Conditions:** `Compare`, every `Op` (`Ne` and `Contains` too),
  through `All`, `Any` and `Not`.
- **Not `Exists`:** the index keys a missing field as null (§32), and
  `Exists` tells them apart.
- **Not `Size` or `ElemMatch`:** they look into arrays, which a key
  doesn't hold.
- **Not a multikey index** (a path with `[*]`, §42): an entry is one
  element, not the array. MongoDB has the same rule.
- **The primary index** holds `_id` only: it covers a filter that names
  only `_id`, or nothing.
- **Exactly the index's paths:** an index on `o.p` covers a condition
  on `o.p`, not on `o` or `o.p.q`.
- **Not two fields where one is inside the other**, `a` and `a.b` of
  one compound index, both named: one document can't hold both values
  as the key has them (§31). Naming either alone is covered.

`_id` is in every key: the last 16 bytes of a secondary one, the whole
of a primary one. So `status == x AND _id > last` is covered by an index
on `status`. A filter without conditions is covered by any index that
isn't multikey.

## 100.2 What an entry decides
A covering read decodes each entry's key back into the values it holds
(`key::decode_part`), into a small document of the named fields at
their paths and `_id` (`Covering::entry`), and checks the filter on
that. Every value decodes exactly except those that stand for several
(`key::part_is_exact`, §28.1):
- a string cut to the key's budget;
- a number of 2^53 or more in magnitude;
- in a compound index, a value no comparison orders (arrays, objects,
  binary, NaN, §43.2).

An entry with such a value, in a field the filter names, doesn't
decide. Its document is read and checked as before. A field the filter
doesn't name may hold anything. Every other entry decides alone: a match
without its document, and a rejection is never read. A damaged key
doesn't decide either.

The entry's document holds `_id` only where the filter (or the sort)
names it, so a filter that names nothing decides every entry without
decoding anything. Entry after entry is decided in one document whose
slots are refilled (`Covering::decide`, `key::decode_part_into` reusing
a string's buffer), and a match decided so isn't checked again. A
document of its own is made only for a match kept for a sort in memory.

A number decodes as a `Float`. Filters compare `Int` and `Float` by
exact value (§34.1), and below 2^53 the float is the exact value, so
every comparison gives the same answer. `-0.0` is keyed as `0.0`, which
it equals.

A document without an entry is never a candidate of a covering read:
the candidates are the same as before, only how they are decided
changes. A single-field index leaves out what no comparison can match
(§28.1), and a walk without a range finds those by a scan that reads
them (§34.2).

## 100.3 Where
- **`count`:** entries that decide are counted. No document is read
  unless an entry can't decide.
- **`find`, `find_one`, `cursor`, `explain_run`:**
  - entries decide matches;
  - **skip:** the matches a skip passes over are decided by their
    entries, and never read;
  - **a sort in memory** whose keys the key holds (the index's fields,
    or `_id`) sorts the entries' documents. Only the page after skip and
    limit is read. A sort key the key doesn't hold needs the documents'
    fields, as before, but only the matches are read for it;
  - **a walk in index order** (§34.2, §96) decides each entry of an
    exact group; documents are read only for the matches handed out.
    A group read whole (§47: an inexact value, or sort keys left to
    memory) and the scan for values the index lacks read as before.
- **`delete_many`, `update_many`, `update_fields`:** they read the
  documents they change, but not the rejected or the skipped.
- **`upsert`:** decides its matches by entries, reading none of them.
- **`View`:** the same, on its snapshot.

By plan:
- **`Index`, and `IndexUnion` over one index** (an OR on one field): as
  above. A union over several indexes doesn't cover: no one index holds
  every field such a filter names.
- **`ById`:** the lookup decides when the filter names only `_id`.
- **`Scan`:** it reads the primary index's entries, which cover a filter
  on `_id` only. `count(Filter::new().gt("_id", last))` reads no
  document.

One range of an index with one key per document holds each document
once, so its entries are no longer put through the set that drops a
document a union or a multikey index finds twice: that set cost about
as much as reading the range.
- **`IndexOrder`:** as above.

The planner doesn't change. It picks the index by its rules (§28.4,
§36.3, §43.3), and whether that index covers decides how much is read,
not which index. `tenant == x AND tries > 7` with an index on `tenant`
alone is bounded by it, not covered. An index on `(tenant, tries)`
covers it.

## 100.4 A cursor
An unsorted `cursor` keeps where its candidates are and reads them as
it goes (§82). A covering read decides at creation. A rejected entry
isn't kept; a match is kept with a flag that says so. A skip passes over
a decided match without reading it, and a match handed out is read then.
Memory per candidate is still a location, and a flag.

## 100.5 What explain_run says
- **`IndexRead::covering`**, new: the read's entries decide the filter.
  The scan for what an index lacks after a walk (§34.2) never covers.
- **`documents_examined`:** documents read, to be checked or, where an
  entry decided, to be handed out.
- **`documents_matched`:** the matches, decided by their documents or
  their entries, the skipped included. It can now exceed
  `documents_examined`.

`explain` and `QueryPlan` stay: covering is not another plan, and it
can differ by entry.

## 100.6 What stays, and what changes in kind
- **Every result, count and order.** Only which documents are read
  changes.
- **The file format and every write.**
- **The API**, apart from `IndexRead::covering` (`#[non_exhaustive]`).

Where an entry decides, the result follows the index, not the document.
A damaged index whose entry disagrees with its document (§55) used to
cost extra reads; now it can change a count or a page. `check` finds
every such disagreement (§39: each index holds exactly one entry per
document with an indexed value). SQLite, PostgreSQL and MongoDB trust a
covering index the same way.

## 100.7 Rejected
- **A plan of its own** (`QueryPlan::IndexOnly`): covering is a property
  of a read and of each entry, not another path, and a plan would
  promise what an inexact entry breaks.
- **`Exists` covered by keying a missing field apart from null:** a new
  key encoding and file format.
- **Multikey indexes covering `[*]` comparisons:** an entry says that
  one element matches. It can't say that none does (`Ne`), or which
  element meets which condition (§42.3).
- **Trusting inexact entries**, a cut string compared on its prefix:
  wrong answers near the cut.
- **Choosing a covering index over a better-bounded one:** a cost-based
  decision the planner doesn't make (§28.4).
- **Projection, an index-only `find` returning fields:** needs `select`
  (ROADMAP), which trunkdb doesn't have.
- **Checking a sample of documents against their entries** while
  reading: `check` is where damage is looked for (§39).

## 100.8 Tests
- **The filter's side** (`a_read_covers_only_what_its_entries_hold`):
  every op, nested in `All`, `Any` and `Not`, against one-field, nested,
  compound, sparse, multikey and primary indexes; `Exists`, `missing`,
  `Size` and `ElemMatch` never cover, alone or nested; neither do a
  field the index lacks, a part of an indexed path, or a path inside
  one; sort keys only when asked.
- **The entry's side:**
  - `exact_parts_decode_back_to_their_values`: every kind of value,
    alone and in a two-field key, decodes to a value with the same
    encoding and the same answer to every comparison; inexact parts
    and seven kinds of damage decode to nothing;
  - `an_entry_gives_the_named_fields_and_its_id`: nested paths, `_id`
    only when named, an inexact value in a field nobody names, a field
    inside another (not covered), the primary index, and entries decided
    one after another in one scratch document.
- **Agreement** (`entries_decide_as_the_documents_would`), with plain
  and with sparse indexes: 300 documents with `a`, `b` (every kind of
  value `random_value` makes: cut strings, numbers past 2^53, NaN,
  arrays, ids, date-times), `o.p` (sometimes `o` a number) and `c`, on
  indexes `b`, `(a, b)` and `o.p`. 600 random filters on those and
  `_id`, every op, nested, now and then on `c` or with `exists`; seven
  sorts, `_id` among them; skips 0 to 1,000 and limits. `find`, typed
  `find` and `cursor`, untyped `cursor` and `count` agree with the
  filter applied to every document, and `explain_run`'s invariants hold
  (`explained`). Then 40 random `delete_many` and `update_fields`
  change exactly what `find` returns, and `check` passes.
- **The randomized corpora before** (§47, §72, §96, compound, sparse,
  multikey, `elem_match`) pass unchanged in their results. Their
  `explain_run` checks now also require `covering` exactly where
  `covered_by` says, and allow more matched than examined only where a
  read covers.
- **What is read** (`where_entries_decide_only_what_is_handed_out_is_read`,
  2,000 tasks):
  - `count` of `tenant == x` and of `status == x, created >= 100` read
    none;
  - page 50 of 20 on `(status, created)`, and by `_id`, read 20;
  - `tenant == x`, newest 20 by `_id`: 20;
  - sorted by a key the index lacks: the 200 matches read, the 200 the
    entries rejected not;
  - a condition on a field the index lacks: all 200 candidates, as
    before;
  - a cursor with `skip(150)`: the 50 it hands out;
  - a cut string, a number past 2^53: that document alone.
- **Counts that changed, in older tests:** the bound's own entry (age
  10 for `age < 10`) and the ids before a reference no longer read;
  the skipped pages of §72's and §96's tests read only what they hand
  out; `explain_run`'s union, compound and skip cases examine only the
  matches.

Nineteen hand-made mutations, all caught:
- `Exists`, `Size` or `ElemMatch` covered;
- a multikey index covering;
- a field the index lacks covered;
- a cut string, a big number or a compound key's unordered value
  decoded as exact;
- the id taken from the wrong end of the key, or left out when named;
- a sort in memory on entries lacking its key;
- a union over several indexes covering;
- an entry's document handed out as the whole one;
- every read said to cover;
- a cursor keeping a decided rejection, or reading the decided matches
  it skips;
- a walk ignoring its entries;
- a field inside another covered;
- a multikey range not deduplicated.

Benchmark (§48), five rows new, for all four stores: `count` of an
index range, a deep page by an index, a deep page by id, and a sort by
id after an index range. trunkdb before (`8e7a36f`, the new rows
copied in) and after, back to back, three runs each on 100,000
documents:

| | before | after |
|---|---:|---:|
| `count` tenant == x (1,000) | 0.91–0.93 ms | 0.17–0.19 ms |
| `count` created in a range (1,000) | 0.98–1.69 ms | 0.20–0.22 ms |
| status == x by created, page 50 of 20 | 1.17–1.23 ms | 0.33–0.35 ms |
| 50 by id after skipping 5,000 | 0.75–0.76 ms | 0.55–0.58 ms |
| tenant == x by id, first 20 | 1.07–1.12 ms | 0.62–0.69 ms |
| find tenant == x (1,000) | 1.46–1.92 ms | 0.82–1.00 ms |
| find created in a range (1,000) | 1.55–1.58 ms | 1.03–1.18 ms |
| status == x, oldest 20 | 35 µs | 23.5–24.6 µs |
| status == x, newest 20 | 40–41 µs | 27 µs |

Unchanged: first 50 by id, get, the scan, writes, compaction, file
size. The finds and walks gained too: their filters are covered, so
each match is decided on its entry and decoded once, and a single
range skips the set that removes duplicates.

The other stores, same run:

| | SQLite | redb | sled |
|---|---:|---:|---:|
| `count` tenant == x | 0.03 ms | 0.04 ms | 0.26 ms |
| page 50 of 20 | 53 µs | 42 µs | 249 µs |
| 50 by id after skipping 5,000 | 123 µs | 152 µs | 868 µs |
| tenant == x by id, first 20 | 1,178 µs | 19 µs | 24 µs |

The draft expected `count` near 0.1 ms; it is 0.17. The first build was
much slower than that (0.66 ms, and "50 by id after skipping 5,000"
slower than before, 1.6 ms): each entry built a document of its own,
with `_id` always, its strings allocated, and the key's parts in a
vector. Deciding in one reused document, `_id` only when named, and
iterating the parts brought it here. The walk's grouping of entries
(`Spans::group`) iterates the parts too now, instead of collecting
them twice an entry: page 50 of 20 from 0.46 ms to 0.35. What a covered entry still costs,
measured: about 70 ns to copy it out of the range, 35 ns to fill the
document, 75 ns for the filter. SQLite and redb count index entries
without copying them out.

Two things stay far behind (100.9): a walk costs about 300 ns an
entry, so a deep page is 7× SQLite's; and "tenant == x by id" sorts
the tenant's 1,000 entries in memory, where redb walks its index, whose
keys within one tenant are in id order already.

Full suite, clippy `-D warnings` with and without all features, fmt,
rustdoc, the fuzz and bench crates.

## 100.9 Limits
- **A filter naming a field the index lacks** reads every candidate, as
  before. A compound index with that field covers it.
- **`Exists`, `Size`, `ElemMatch`**, and multikey indexes, never cover.
- **A union over several indexes** doesn't cover.
- **A damaged index** can change results where its entries decide,
  until `check` finds it (100.6).
- **A deep page still walks every entry it skips**, at about 300 ns
  each: the walk copies each key and groups entries one by one. Only
  paging by the last value (§72.3) skips without walking.
- **A sort by `_id` after an `Eq` on an index** sorts the entries in
  memory, though within one value they are in id order already. The
  planner could walk such an index for an `_id` sort, as it walks a
  compound index whose first fields an `Eq` fixes (§43.3): not done
  here.
- **Each covered entry is still copied out of its range** before it is
  decided.
