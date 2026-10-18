# 106. Projection: a select of fields (`select.rs`, `query.rs`, `collection.rs`, `cursor.rs`, `explain.rs`)

A read handed out whole documents. There were two ways to get fewer
fields:
- **A typed collection** could read into a smaller struct (README,
  "Reading only some fields"). Since §99 it decodes straight into it and
  skips the fields it doesn't declare.
- **`Collection<Document>`** had no smaller type. It decoded and handed
  out every field, even when the caller showed two of them. An admin
  list, an export of one column and a lookup of names all paid for the
  rest.

§106 adds `Filter::select`, the fields a read hands out. SQL calls it a
column list, MongoDB a projection, LINQ `Select`. It was the ROADMAP's
"Projection, a `select` of fields", and it is the first step toward
aggregates, which §6 had left out and §106.7 takes up again.

It saves two things:
- **Decoding and memory, always.** The fields not selected are skipped,
  not built. It doesn't save reads: a document is stored in one piece,
  overflow pages included (§26), and the encoding records no byte
  sizes, so reaching a field still walks the fields before it (§11.1),
  without allocating them.
- **The document itself, sometimes.** Where an index answers the filter
  (§100) and holds every selected field, the result is built from the
  index entry, and the document isn't read at all (106.5). That is
  §100's `count` carried over to `find`.

## 106.1 The API
```rust
let rows = users.find(
    Filter::new()
        .eq("team", "core")
        .sort_asc("name")
        .select(["name", "address.city"]),
)?;
// [{"_id": ..., "name": "Ada", "address": {"city": "London"}}, ...]
```

- **`Filter::select(paths)`**, taking anything `IntoIterator` of
  `Into<String>`, as `Condition::all` does. A second call adds to the
  first.
- **On `Filter`**, beside sort, skip and limit: one value still says
  everything about a read, and code that builds a filter (the admin's
  page filter, the fuzz target) gains the selection in the same place.
  An argument to `find` would have been two arguments to every read on
  `Collection` and `View`, and a break for every caller.
- **`Filter::apply` projects too:** it is the meaning in memory, and
  §105 uses it as its oracle. `apply_to` hands back the items it was
  given, which carry a document rather than being one, so it can't.
- **No `select` on `get`:** `find_one(Filter::new().id(id).select(..))`
  reads one document by id with a selection, through the same lookup
  (§61).

## 106.2 What a selection hands out
A document with `_id` and the selected paths:

- **`_id` always, first.** Every result keeps its identity: the admin
  opens a row by it, and a typed `T` with `#[serde(rename = "_id")]`
  reads it. Selecting `_id` changes nothing. There is no way to leave it
  out (106.8).
- **Then the paths, in the order the selection first names them.**
  The draft had the stored order, the stored document with fields taken
  out, as MongoDB's projection is. A result built from an index entry
  (106.5) doesn't know the stored order, and two orders for one read
  would show in the text the admin displays. The selection's order is
  known on every route, and it is a SQL column list's order too.
- **A dotted path keeps its nesting.** `address.city` gives
  `{"address": {"city": ...}}`. Two paths into one object share it:
  `address.city` and `address.zip` give one `address` with both, in
  that order.
- **A path stops where its value does.** It doesn't walk into an array,
  as conditions don't (§31). If `address` is an array, a string or
  missing, `address.city` selects nothing, and an object with nothing
  selected under it is left out.
- **A missing field is left out**, not made `null`. A stored `null` is
  handed out as `null`. That keeps "missing" and "null" apart, which
  the admin's table shows differently.
- **A path inside a selected one adds nothing.** `address` and
  `address.city` together give all of `address`, in either order.
- **An empty selection,** `select([] as [&str; 0])`, gives `_id` alone:
  the cheapest list of ids there is.
- **`[*]`:** a step ending in `[*]` selects its array whole:
  `items[*].sku` selects `items`. Trimming each element to `sku`, as
  MongoDB does, is left for later (106.8). The empty path, the document
  itself (§46.1), or one starting at `[*]`, selects the whole document.

A typed read with a selection deserializes `T` from that document. A
field of `T` outside the selection is missing to `T`: an `Option` is
`None`, `#[serde(default)]` takes its default, and a required field is
`Error::Document` ("missing field `team`"), as for any stored document
that lacks it.

## 106.3 Where it acts
A selection shapes what a read **hands out**, and nothing else. It is
applied last: the filter, sort, skip and limit run on whole documents
(on the fields they name, §99.2), so a field the filter or the sort
needs doesn't have to be selected.

- **`find`, `find_one`, `cursor`**, on `Collection` and `View`, typed
  and untyped: they hand out the selection.
- **`explain_run`** runs as `find` does, selection included (106.6).
  `explain` names no fields: the plan doesn't change with a selection.
- **`count`:** nothing is handed out, so a selection changes nothing.
- **Writes work on whole documents.** `delete_many`, `update_fields`
  and `upsert` hand nothing out and ignore the selection. `update_many`
  hands its closure the whole document, because it writes back what the
  closure returns: a selected document written back would drop every
  field outside the selection. `Filter::select`'s documentation says
  so.
- **`export`:** no filter, no change.

The planner doesn't change. It picks the index by its rules (§28.4,
§36.3, §43.3), as §100 left it: a selection decides how much is read,
never which index (106.8).

## 106.4 Decoding only what is selected
`select.rs`. The selection is parsed once per read into a tree
(`Selection`): each node is a value selected whole, or some fields of
an object, in the order first named.

- **`Selection::decode`** walks the stored object once:
  - a top-level field not in the tree is skipped (`skip_value`, with
    every check a decode makes, §99.2);
  - a field selected whole is decoded;
  - a field selected in part is walked one level down by the same rule
    when it is an object, and skipped otherwise;
  - each found value goes into its slot, and the slots are put
    together in the selection's order at the end.
  A stored `_id` (files before §59) is passed over, as `with_id`
  replaces it.
- **`Selection::project`** does the same to a decoded document, moving
  its values out: what `Filter::apply` hands out, and what a document
  already decoded whole becomes.
- **An untyped index-range candidate** was decoded whole, checked, and
  handed out as it was (`Found::whole`, §99.2). With a selection it is
  checked on its fields, as a scan's candidate is, and the selection is
  decoded after the sort, skip and limit. The draft's alternative,
  decoding the union of the filter's fields and the selection once, is
  one walk less where both are wanted. Measured below, the simple way
  is already faster than the whole decode.
- **Typed with a selection:** the selected document, then the bridge
  (§13). Teaching `serde_stored`'s deserializer the tree would be
  faster, but a typed caller has the smaller struct already.
- **Damage reads as damage**, exactly as a whole decode finds it
  (§99.4): every byte a selection skips is skipped with the checks, and
  the empty selection walks the document too, though it hands out
  nothing of it.

## 106.5 Answering from index entries
§100 let an entry decide its document's match. §106 lets an entry
**be** the result, where it holds everything the result needs
(`select::Answer`):

1. **The read's entries decide the filter** (`Covering`, §100.1), and
2. **every selected path is one of the index's fields**, exactly as
   written (`o.p`, not `o` or `o.p.q`, as §100.1), and the index isn't
   multikey, and
3. **every selected value in this entry can be handed out as stored.**

A key doesn't hold every value as it was stored, so rule 3 asks more
than §100's "exact", which only has to decide comparisons the same way:

| A value in the key | Decides a filter (§100) | Can be handed out |
|---|:-:|:-:|
| bool, `DocId`, `DateTime` | yes | yes |
| a string that fits the key | yes | yes |
| a string cut to the key's budget | no | no |
| a number below 2^53 | yes | **no**: `5` and `5.0` share a key, and `-0.0` is keyed as `0.0` |
| `null` | yes | **no**: a missing field is keyed as `null` too (§32), and 106.2 leaves it out |
| arrays, objects, binary, NaN (compound) | no | no |

A match whose selected values all pass rule 3 is handed out without
reading its document. Any other match's document is read, as before.
This is decided per entry, as in §100: a list of names with one long
name reads one document.

So `(status, created)`, with `created` a date-time, answers "page 3 of
the queued, newest first, `select(["status", "created"])`" without
reading a document, and `(tenant, status)` answers "every status of
tenant x". A selection with a number field reads the documents: the key
doesn't know whether a number was an `Int` or a `Float`.

Where:
- **A read of index ranges** (`Index`, and `IndexUnion` over one
  index): a match its entry decided keeps its key (moved, not copied),
  and after the sort, skip and limit, the matches handed out are built
  from their keys where rule 3 holds (`Source::found`).
- **A walk in index order** (`IndexOrder`, §34.2, §96, §103): a match
  its entry decided is built from its entry as the walk passes it, if
  the page keeps it. The matches a skip passes over aren't built, and
  the keys stay lent (§101.5).
- **`ById` and `Scan`** with a selection of `_id` alone: the primary
  index's entries are the results. `find(Filter::new().select([] as
  [&str; 0]))` lists every id without reading a document.
- **An unsorted cursor** (§100.4) keeps a decided match's key where the
  selection may be answered from it, and builds the result when it gets
  there. Memory per candidate grows by the key, at most 1 KiB (§28.2).
- **Typed:** the document built from the entry, then the bridge.
- **Writes** read whole documents (106.3), and answer nothing.

## 106.6 What explain_run says
- **`ExecutionStats::from_entries`**, new: results built from their
  index entries, their documents never read.
- **`documents_examined`** counts the documents read, as before. With a
  selection it is the unselected filter's, less `from_entries`: a test
  and the fuzz target hold every read to that.
- **`Explained::select`**, new: the paths selected, as given; `None`
  without a selection.

`QueryPlan` doesn't change: a selection changes no plan.

## 106.7 Toward aggregates
Aggregates (`GROUP BY`, `COUNT`, `SUM`) were left to application code
in v0 (§5.1, §6), when `count` read every document and an index decided
nothing alone. Since then, `count` reads no document where an index
answers (§100, §101), filters are decided on index bytes (§104), and
`explain_run` shows how a read went. Application code can't use any of
that, and doing it there costs more than it did in v0. So aggregates
are worth looking at again, as a section of their own, without joins.
§6's line now says so, and the ROADMAP lists them under the API.

§106 builds what they would stand on, and decides nothing more:
- **Paths that name the values to read,** parsed once and walked in one
  pass over the stored object (106.4). `group_by("team")` and
  `sum("hours")` name fields as `select` does.
- **Results built from index entries** (106.5). In an index on `(team,
  hours)` the entries of one team are side by side, so a `count`, `min`
  or `max` per team needs no document. `sum` and `avg` over numbers
  need the entries' values: as floats, enough for a sum but not for
  handing out.
- **A result that is not a stored document:** an id and some fields,
  typed or not. An aggregate's row (a group's key and its counts) is one
  step further: a document with no `_id`.

What §106 must not block: `select` stays a list of paths, not an
expression. Computed fields, renames and per-group functions belong to
an aggregate API of their own (`Filter::group_by(..)` or a separate
builder), decided in that section.

## 106.8 Rejected
- **Leaving `_id` out**, MongoDB's `_id: 0`: every reader of a result
  here keys it by id, and leaving it out gains 16 bytes a document.
- **Excluding fields** (`omit(["body"])`): useful against one large
  field, but a second meaning beside the first, and nothing asks for it
  yet. It can come later as its own method.
- **Trimming arrays** (`items[*].sku` giving each element's `sku`):
  MongoDB's rules for it (non-objects dropped, objects kept empty) are a
  design of their own. `[*]` selects the array whole for now (106.2).
- **Computed fields and renames:** an aggregate's work (106.7).
- **A selection that picks the index:** choosing an index because it
  holds the selected fields, when another bounds the filter better,
  would take a cost model, which the planner hasn't got (§6).
- **Numbers and nulls handed out from keys:** an `Int`/`Float` bit and a
  missing mark in every key change the key format: file format 13, and
  every index rebuilt. Not for a projection. It can be weighed again
  with aggregates, where `sum` over an index would want it.
- **The stored order of fields** (the draft's): unknown to a result
  built from an entry (106.2).
- **A `select` argument to `find`** instead of a `Filter` method: two
  arguments to every read method, and a break for every caller.
- **`trunkdb find` on the command line:** the command has no query yet
  (§39). A query on the command line needs a filter syntax in text, a
  decision of its own. The ROADMAP's line about projection for the
  command is now its own item.

## 106.9 Tests
- **The meaning** (`select.rs`): the tree of paths, and every rule of
  106.2 on a document in memory (`a_projection_hands_out_what_it_names`):
  `_id` first; the selection's order; nesting; through an array, a
  string and a missing field; missing left out, `null` kept; a path
  inside a selected one in either order; `[*]`; the empty selection;
  the empty path.
- **Decoding is projecting** (`decoding_a_selection_is_projecting_the_whole`):
  every selection of three from 18 paths over a nested document with a
  stored `_id`, decoded and projected, give the same text.
- **Damage** (`a_selection_is_damaged_when_the_whole_is`): every
  truncation and four altered bytes at every position of a nested
  document fail six selections exactly where, and with the message, the
  whole decode fails: nested, missing, empty, whole objects.
- **Every route** (`a_selection_hands_out_what_it_names_on_every_read`):
  `find`, `cursor`, `find_one`, a `View`, sorted and unsorted; a second
  `select`; conditions and sort keys outside the selection; typed, a
  field outside the selection `None`, and a required one
  `Error::Document`.
- **From entries, exactly** (`results_from_entries_hand_out_values_as_stored`):
  an index on each kind of value of 106.5's table — a bool, a short
  string, a cut one, an `Int`, a `Float`, `-0.0`, 2^60, an id, a
  date-time, `null`, and a missing field — read four ways (an `eq`, a
  walk by id after it, a walk of the index, a skip), through `find` and
  `cursor`. Each must hand out what the whole document projected gives,
  `Int` as `Int`, `-0.0` as `-0.0`, missing as missing; `from_entries`
  counts every result for the four kinds of rule 3 and none for the
  rest.
- **A page from a compound index**
  (`a_page_of_a_compound_index_is_answered_without_a_document`): page 3
  of 20 on `(status, created)` with date-times, 20 from entries and 0
  documents read; a number selected too, 0 and 20; every id of 500,
  500 and 0.
- **Writes ignore it** (`writes_ignore_the_selection`): `update_many`
  with a selection is given, and keeps, the whole document;
  `update_fields`, `upsert`, `delete_many` with sort and limit, and
  `count` give what they give without it.
- **The randomized corpora** (sorted finds through every index, §47,
  §72, §96; compound and sparse compound through every kind of write,
  §43, §44; §100's, §101's and §103's) run each of their filters again
  with a random selection (`selection_agrees`), half the time one
  index's fields:
  `find`, a `View`, `cursor`, `find_one`, typed (`Keys`, the names of
  the fields `T` is given; `IdOnly`) and `explain_run` must hand out
  the documents the filter finds, each projected, in order where order
  is defined, with `documents_examined` the unselected filter's less
  `from_entries`. §100's corpus must answer more than 100 from entries:
  it answered 143.
- **The `queries` target (§105)** selects on half its reads, a focused
  query's index fields half of those, and the model projects through
  `apply`. Each selected read is held to the `documents_examined` rule
  too, and writes are given filters with selections. 20,000 smoke
  programs (seed 106106, 1,120 s) ran 168,638 reads and answered 113,921
  results from entries, and found nothing.

**Mutations,** each put in alone, against the library's tests and 300
smoke programs:

| Mutation | Tests | Smoke run |
|---|:-:|:-:|
| a number handed out from its key | caught | caught |
| a null handed out from its key, for a missing field too | caught | caught |
| a selected path the index doesn't hold left out of the answer | caught | caught |
| `_id` dropped from a decoded selection | caught | caught |
| a path selected inside an array taking the array whole | caught | caught |
| a field selected in part decoded whole | caught | caught |
| fields in their stored order | caught | survived |
| an answer built from the wrong value of a compound key | caught | caught |
| `update_many` given the selection | caught | survived |
| a cursor's skip passing one decided match too few | caught | caught |
| a multikey index allowed to answer | survived | survived |

- **Fields in their stored order** survive the smoke run by design: the
  target compares documents with their fields in any order (§105.1).
  The corpora compare text.
- **`update_many`** isn't one of the target's steps.
- **A multikey index allowed** changes nothing: `Answer` is only made
  where the entries decide the filter, which they never do for a
  multikey index (`Covering`), and a selected path has no `[*]` to
  match one. The check was taken out.

## 106.10 Measurements
`bench/src/bin/select.rs`: the benchmark's 100,000 tasks (§48), read
untyped, as `Document`s. Beside them, SQLite: `SELECT doc` parsed into a
JSON value for the whole document, a column list for a selection.
`paired` holds the same documents with an index on `(tenant, status)`.
Two runs, release build:

| Query | trunkdb | Documents read, answered | SQLite |
|---|---:|---|---:|
| tenant == x (1,000), whole | 1.54–1.74 ms | 1,000, 0 | 2.67–3.03 ms |
| the same, `select(["tenant", "status"])` | 1.04–1.06 ms | 1,000, 0 | 1.55–1.63 ms |
| the same, on `(tenant, status)` | 0.67–0.72 ms | 0, 1,000 | — |
| page 50 of 20 by `created`, whole | 34 µs | 20, 0 | 56–63 µs |
| the same, `select(["status", "created"])` | 30 µs | 20, 0 | 36 µs |
| the same, `select(["status"])` | 22 µs | 0, 20 | — |
| every document, whole | 153–228 ms | 100,000, 0 | 135–136 ms |
| every id, `select([])` | 41–42 ms | 0, 100,000 | 20 ms |
| tries > 7, a scan, whole | 87–89 ms | 100,000, 0 | 74 ms |
| the same, `select(["tenant", "status"])` | 76–77 ms | 100,000, 0 | 50–53 ms |

- **Decoding only the selection** saves a third where most of a
  document isn't selected (1.6 ms → 1.05), and beats SQLite's column
  list, which reads its columns from the row.
- **Answering from entries** halves that again (0.7 ms), not the tenth
  the draft expected. A profile of it is more than half `malloc` and
  `free`: each result is a `Document`, an `IndexMap` with its table and
  a `String` per key and per string value, eight allocations or so, and
  each key is copied out of the range read, as §100 reads ranges. The
  reading the draft counted on saving is nearly gone; building the
  results is what's left. A cheaper `Document` is §99.6's separate
  decision, and the count of the same range, which builds nothing,
  takes 12–17 µs.
- **`created` is a number** in the benchmark's tasks, so the page with
  it selected is read from its documents (106.5); with `status` alone
  it is answered.
- **A scan** still reads every document: the selection saves only the
  decoding of the matches.
- **Every id** from the primary index alone takes 41 ms, against
  SQLite's 20: the results are 100,000 objects of one field each.

`Answer::entry` decodes only the selected values of a key, into the
selection's order: a first version filled a slot for every field of the
index, zeroed on every entry, and was 5–15% slower.

## 106.11 Limits
- **No read is saved without an index** that holds the selection: a
  document is still read in full from its pages (106, intro).
- **Numbers and nulls read their documents** even where the index holds
  them (106.5).
- **Typed selections go through the bridge** (106.4).
- **No array trimming, no exclusions, no computed fields** (106.8).
