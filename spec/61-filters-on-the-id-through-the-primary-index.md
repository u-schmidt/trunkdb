# 61. Filters on the id through the primary index (`query.rs`, `collection.rs`)

Since §59, a struct's id is a field like any other, and filters on ids
match (§59.5). But `eq("_id", id)` read every document of the
collection and compared its `_id`, while `get(&id)` found the same
document in the primary index. On 100,000 documents that was 67 ms
against 3.4 µs. Now the planner sends the one to the other.

```rust
let car = cars.find_one(Filter::new().eq("_id", id))?;           // as get(&id)
let some = cars.find(Filter::new().any_of([
    Condition::eq("_id", a),
    Condition::eq("_id", b),
]))?;                                                             // two lookups
```

## 61.1 Which filters name documents by id
`Filter::ids` finds the ids a filter can only match documents with
(`query::ids_for`):
- **`eq("_id", id)`**, with a `DocId` as the value, names that one id.
- **Conditions that must all hold** (the filter's own, or an `All`)
  name what the one naming the fewest names: `_id == a AND (_id == b OR
  _id == c)` looks up `a` only.
- **An OR** names its branches' ids together, if every branch names
  some. `_id == a OR name == "x"` names none: the second branch could
  match any document.
- **Anything else names none:** `ne`, `lt` and the other comparisons,
  a `not`, an `elem_match`, a path that only ends in `_id`
  (`owner._id`), and an id's string form, which isn't equal to the id
  anyway (§59.5).

The ids are sorted and each kept once, so an OR naming an id twice reads
it once, and the documents come in id order, as a scan would give them.

## 61.2 How they're read
`candidate_entries`, where every read path (`find`, `find_one`,
`count`, `cursor`, `update_many`, `delete_many`, `upsert`) gets the
entries it reads documents from, looks each named id up in the primary
index before trying a secondary one. An id that isn't there is skipped.
As with an index range, each document found is then checked against the
whole filter (§28.3): the lookup only decides what is read. So an id
whose document fails another condition is no match, and so is a
document that isn't an object: it has no `_id` field to compare.

**Ids beat every index.** A filter naming ids reads at most that many
documents, and nothing an index can do reads fewer. That includes an
index that could serve the sort (`IndexOrder`, §34.2): the named
documents are sorted in memory instead, and `index_order` declines a
filter that names ids.

`explain` shows it as a new plan, `QueryPlan::ById`. `QueryPlan` became
`#[non_exhaustive]` in §60 with this change in mind, so adding it breaks
no one.

## 61.3 What it costs
Nothing on the file: the primary index already holds every id, so no
format change. The planner looks at the conditions once more per query.
Measured on 100,000 documents, in a release build:

| | per call |
|---|---|
| `find(eq("seats", n))`, a scan, no index | 67 ms |
| `find(eq("_id", id))` | 3.9 µs |
| `get(&id)` | 3.4 µs |

The half microsecond over `get` goes, most likely, to building the
filter, planning, and checking the document against it.

Rejected:
- **Recognizing only a top-level `eq("_id", ...)`.** Fetching a set of
  known ids, as a sync client does, is an OR of them; handling `Any`
  cost one more match arm.
- **Intersecting the ids of several conditions** instead of taking the
  fewest: it would save a lookup now and then, but two id conditions
  joined by AND are rare, and the check against the whole filter keeps
  the answer right without it.
- **A dedicated `Filter::ids` builder** (an `in` on `_id`): an OR of
  `eq`s says the same. A shortcut that spares callers spelling `"_id"`
  is a separate open item (ROADMAP.md).

## 61.4 Limits
- **An OR that mixes ids and other conditions** isn't served by lookups
  and index ranges together: one branch without ids, and the planner
  falls back to the indexes, or a scan.
- **A `DocId` in any other field**, a reference to another document,
  still has no index key and is scanned for (§59.6); that's the other
  open item.
- **A string `_id`** (an untyped document with `"_id": "abc"` is stored
  with a new id, §59.2) is never looked up: only a `DocId` names a
  document.

## 61.5 Tests
- `query.rs`: which filters name ids, and which don't: `eq` at the top
  and in an `All`, the fewest of several, an OR with every branch
  naming some or not, each id once and in order; `ne`, `lt`, `not`, a
  string, `id` and `ref._id` name none.
- `collection.rs`:
  - `eq("_id", id)` reads one document, with `find`, `find_one` and
    `count`; another condition still decides; an unknown id reads
    nothing; an OR of ids, one of them twice, reads each once, sorted
    in memory ahead of the index on the sort field, and through `cursor`
    too; `update_many` and `delete_many` find their documents the same
    way; an id's string is a scan and finds nothing;
  - the random filters that compare every plan against a scan now name
    documents by id a quarter of the time, sometimes one that doesn't
    exist, alone or next to other conditions, sorted or not, and after
    a reopen; `ById` joins the plans they must all hit.
- Checked by breaking it on purpose, eleven ways. Each of these fails a
  test:
  - no id ever named; ids not sorted, or not kept once;
  - an OR with a branch that names none, named anyway;
  - the most ids of several taken, not the fewest;
  - any comparison, or any path ending in `_id`, naming ids;
  - the index order walk ahead of the ids;
  - the lookups finding nothing, or not made; `explain` not showing it.
