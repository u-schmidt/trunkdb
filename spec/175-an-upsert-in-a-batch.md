# 175. An upsert in a batch (`batch.rs`, `collection.rs`, `bin/trunkdb.rs`)

A counter, a page's visits or a day's total, is one document per key
that the first write makes and later ones add to (§144.2). Kept beside
other writes, a page's count and the site's, or a reading and its
device's last-seen time, it wants one commit. A batch had no upsert:
§168.6 left it to a `require_none` and an insert, or a `require` and an
`update_many`, with the program choosing which before the batch, and a
batch failing whenever another write changed the answer in between.
Through the C ABI (ROADMAP.md, Planned), a batch is how a program
writes, so the op belongs there. No file format changed.

```rust
let mut batch = db.batch();
batch.upsert_fields(&visits, Filter::new().eq("page", "/home"), &Update::new().inc("hits", 1))?;
batch.upsert_fields(&totals, Filter::new().eq("site", "main"), &Update::new().inc("hits", 1))?;
let written = batch.commit()?;
let page = written.upserted(0).unwrap().id(); // Inserted(id) the first time, Updated(id) after
```

```sh
trunkdb batch app.trunkdb - <<'END'
{"op": "upsert", "collection": "visits", "filter": "page = \"/home\"", "update": {"$inc": {"hits": 1}}}
{"op": "insert", "collection": "log", "document": {"page": "/home"}}
END
```

## 175.1 `Batch::upsert_fields(&collection, filter, &update)`
`Collection::upsert_fields` (§144.2) as an op of a batch:
- **At its place:** at `commit`, under the writer's lock, on the
  documents as the ops before it leave them (§168.1). An insert before
  it is what it finds; two upserts of one key make one document, the
  first inserting and the second updating it; a document deleted
  before it isn't found, so it inserts.
- **One match** gets the operators; one they leave as it was isn't
  written. **None:** a document made of the filter's `eq`s and then the
  operators, `set_on_insert` included, is inserted. **Several** fail
  the whole batch with `Error::MultipleMatches`.
- **Refused when added,** `ErrorKind::InvalidInput`, the batch left as
  it was: what `upsert_fields` refuses (an update refused as written, a
  filter whose `eq`s aren't one document), and a filter with a sort, a
  skip, a limit or a selection, as `require` refuses one (§162.1): only
  the conditions count. `Collection::upsert_fields` passes over them; a
  batch, written for a script or the C ABI, says so instead, as it does
  for a precondition.
- **The new id is chosen when it's added,** the filter's `_id` if it
  has one, as `insert` chooses its own (§60): a dry run and the commit
  after it report the same id.
- **At `commit`, as `upsert_fields` fails:** an operator that can't be
  made, to the match or to what would be inserted, `Error::Update`; on a
  typed collection, a document written that doesn't convert to `T`; a
  unique index refusing it. Each rolls back the whole batch.
- **One code path:** `Collection::upsert_fields` and the batch's op
  share `upsert_fields_in` and `only_match_in`, which take the
  collection's name.

## 175.2 What it wrote
- **`Written::counts`:** 1 where it inserted or changed the match, 0
  where the match was left as it was, as for `update_fields`.
- **`Written::upserted(op)`:** `Some(Upserted::Inserted(id))` or
  `Some(Upserted::Updated(id))` for an upsert op, counted from 0 in the
  order added as `counts` is; `None` for any other op and past the last.
  A program learns the id of a counter it made, and whether it made it.
  `Written` is `#[non_exhaustive]` (§168.2), so this is an addition.

## 175.3 `trunkdb batch`
`{"op": "upsert", "collection": …, "filter": …, "update": …}`: the
filter's text and the operators as for an `update` line (§168.3).
- **No `all`:** an upsert writes at most one document, so an empty
  filter is the collection's one document, settings kept as one, and
  needs no guard. `"all"` on an upsert line is a field it doesn't take.
- **Its id on standard output,** inserted or updated, as `trunkdb
  update --upsert` prints it (§156): the ids of the inserts and upserts,
  one a line, in the order of their lines, so a script can tell which
  is which. None on a dry run, nor for a batch that fails.
- **On standard error:** `line 1: inserted 1 document` or `line 1:
  updated 1 document`, `updated 0 documents` where the match was left as
  it was; the batch's line counts them with the inserts and updates.
- **Several matches** fail at commit, exit code 1, and the dry runs of
  §168.5 name the line.

## 175.4 Rejected
- **`Batch::upsert` with a whole document,** as `Collection::upsert`
  takes one: not asked for, and the command and the C ABI write
  operators (§156.2); `$set` of each field does it.
- **Returning the id from `upsert_fields` when it's added,** as `insert`
  does: it is the id only if nothing matches, which only `commit`
  knows. A program that needs the id before reads it from `Written`.
- **Passing over a sort, skip or limit,** as `Collection::upsert_fields`
  does: see 175.1.
- **The op's result in `counts` alone,** 1 for both: whether it made the
  document is what a counter's first write needs to know.

## 175.5 Tests
- `tests/batch_upserts.rs`:
  - counters: a page's and the site's, made by the first batch,
    `Inserted` then `Updated` with the same id, `set_on_insert` given
    once, through an index on the key;
  - at its place: an insert before it found, two upserts of a key, a
    precondition after them, a match deleted and inserted anew;
    `upserted` `None` for other ops and past the last;
  - a match left as it was, counted 0 and told to no subscriber;
  - the filter's `_id` and nested `eq`s making the new document;
  - several matches, an operator failing on the match and on the
    insert, a typed document that doesn't convert: each rolling back
    the insert before it, the dry run failing alike; the untyped handle
    taking it;
  - refused when added: a sort, a skip, a limit, a selection, two values,
    a field and one inside it, an update of `_id`;
  - a dry run: the same outcome and id as the commit after it;
  - a model: 300 random batches of inserts, upserts that raise a value
    or leave it, and deletes, with and without an index, each op
    checked for its count, what it did and to which document, the
    state after each batch, a failure on several matches;
  - four threads upserting three pages' counts and logging each visit,
    one document a page, its count the visits logged.
- `tests/cli.rs`: upsert lines inserting, finding the first's
  document, and leaving one as it was; their ids in line order with an
  insert's; the dry run; an empty filter twice making one document;
  several matches named at their line; the line's mistakes (no update,
  a limit, two values, `all`).
- The `queries` fuzz program (§105): half the batches that end with
  operators end with an upsert instead, checked against the model as
  the ops before leave it, a dry run first, one in four by the id of a
  document the batch holds, one in eight with the query's sort, skip
  and limit, refused.

## 175.6 Fuzzed and mutated
- **Fuzzed:** 1,000 random programs of the `queries` target ran 737
  upserts in batches, 216 inserting, 113 updating, 408 failing as the
  model said; then 15 minutes guided, 3,005 programs. Nothing found.
  Nothing in storage changed, so the other targets weren't run.
- **Mutated,** sixteen ways by hand, each run against the library's,
  the batches' and the command's tests: a match left as it was counted
  1, an insert counted 0; `upserted` not recorded, or read one op off;
  a sort, skip or limit let through, the filter's `eq`s or the update
  not checked when added; the first of several matches taken; the typed
  check skipped, in the collection and in the batch; `set_on_insert`
  lost; the filter's `_id` not used; in the command, an upsert's id not
  printed, an update reported as an insert, an empty filter needing
  `all`, upsert lines run as updates. Every one fails a test.
