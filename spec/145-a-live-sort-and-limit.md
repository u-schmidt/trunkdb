# 145. A live sort and limit (`live.rs`, `collection.rs`)

§138 refused a live query with a sort, a skip or a limit: "a window of
the first matches moves with every write" (§138.4), and §138.6 left it
as a step of its own. This is that step: "the ten fastest vans", kept
as they change, for a list on a screen.

```rust
let fastest = vans.watch(Filter::new().sort_desc("speed").limit(10))?;
let mut list: Vec<DocId> = Vec::new();
for event in fastest {
    match event? {
        Live::Entered { id, at: Some(at), .. } => list.insert(at, id),
        Live::Left { from: Some(from), .. } => { list.remove(from); }
        Live::Changed { id, from: Some(from), at: Some(at), .. } => {
            list.remove(from);
            list.insert(at, id);
        }
        _ => {}
    }
    // `list` is what `find` with the same filter answers now.
}
```

## 145.1 The API
- **`watch` takes a sort, with a limit or without.** Its first events
  are `Entered` for the first `limit` matches, in `find`'s order, then
  what each commit changes in that list. Without a limit, the list is
  every match, in order.
- **Each event says where,** in fields the variants gain (they are
  `#[non_exhaustive]`, §117, so a `match` with `..` is unchanged):
  `Entered`'s `at`, `Left`'s `from`, `Changed`'s `from` and `at`, each
  an `Option<usize>`, `None` without a sort.
- **`Left` also means pushed out:** a document still matching, that
  others came before. It holds the document as it is, as for one that
  no longer matches.
- **Refused with `Error::NotLive`:** a skip (`"a skip"`), as the
  documents before the list move it with every write; a limit without a
  sort (`"a limit without a sort"`), which has no order to keep: `find`
  takes the first it reads, in whatever order its plan reads.
- `select` cuts what is handed out, as before; the sort values are
  read from the whole document.

## 145.2 Checked as the commit is
The live query keeps its list, each member with its sort values. As in
§138.2, it is checked inside the commit, under the writer's lock, from
the documents the commit changed, each read once from the snapshot just
published. A sorted one takes note of each, and settles once the whole
commit is seen:

1. **What is known to match now:** the members the commit didn't write,
   with the sort values they had, and the written documents that
   match, with theirs.
2. **Where the list wasn't full** (fewer members than the limit, or no
   limit), every match was in it, so the first `limit` of what is known
   is the new list.
3. **Where it was full,** a match left out of it comes after its last
   member, and wasn't written: what is known up to that last member is
   certain. If that is `limit` or more, the first `limit` of it is the
   new list. If not, a member left or fell behind, and the next ones
   are documents no commit wrote: the filter is **read again**, its
   conditions, sort and limit on the state the commit left, as `find`
   would read it (an index in sort order where one serves, §34).

A document behind a full list and written stays unseen unless it comes
before the last member. Each settled commit reads nothing more where
the known documents fill the list: a ping that moves the third
fastest van to second, or a slow van that stays slow.

**The order is `find`'s:** the sort keys, then the id, as equal sort
values come in id order (§47). Being total, a document's place is
fixed by its sort values alone, so one the commit didn't write keeps
its place relative to the others it didn't write.

## 145.3 The places
The events take the old list to the new one, each a step on the list
as the steps before left it:

1. **`Left` first,** for each member not in the new list, front to back,
   `from` its place then.
2. **Then `Changed` and `Entered`,** in the new list's order: a written
   member is taken out at `from` and put in at `at`, by its new sort
   values among the others; a document coming in is put in at `at`.

A member the commit didn't write is never reported: its place moves
only as others are taken out or put in before it. Firestore's
`DocumentChange` has the same contract, `oldIndex` and `newIndex` "as
if all prior changes had been applied", for the same reason: a screen
applies each event as a step.

A dropped collection is a `Left` for each member, all `from` 0.

## 145.4 Rejected
- **The query run again after every commit,** as a live aggregate is
  (§139.1): simplest, but a scan per commit without an index on the
  sort, even where the commit wrote nothing near the list. Read again
  only where the list loses a member to a document unseen.
- **Every match's sort values kept,** so the next one comes from memory:
  no read again, but memory for every match, a million readings for
  the newest ten.
- **No places,** the application sorting what it holds itself: it would
  need trunkdb's order, kinds of value and ties included, and a
  `select` may cut the sort fields out.
- **A `Moved` event** for a document whose place changes without being
  written: its place follows from the others' steps, as in 145.3.
- **The whole list at each commit,** as `(Vec<T>, …)`: a list of ten
  sent ten times for each ping, and the application diffing it.
- **A limit without a sort in id order:** the order `find` reads isn't
  that, and a live query should be what `find` answers.
- **A skip:** the documents before the list would have to be kept too;
  page two of a list that moves with every write is rarely what a
  screen shows.

## 145.5 Limits
- **Reading again runs under the writer's lock,** a `find` with the
  sort and limit: a walk of `limit` entries where an index serves the
  sort, a scan of the collection where none does. A list whose members
  fall behind its end at every commit, the ten fastest where the
  fastest slows down at each ping, reads at every commit.
- **Memory:** the members' ids and sort values, `limit` of them; every
  match without a limit.
- The rest is §138.7's: unbounded, checked under the writer's lock,
  events lost if the process stops.

## 145.6 Tests
- `tests/live.rs`:
  - the first matches first, in order, with their places;
  - a document coming in pushes the last out, which is handed out as it
    is; a member written and moved, one written in place, one behind
    the list written and not reported;
  - a full list losing one reads the next again, a document no commit
    wrote; a member deleted; fewer matches than the limit, every one in
    the list;
  - equal sort values in `find`'s order, by id; one written as it was
    staying in place;
  - a sort without a limit; renamed past the others, moved to the end;
  - a dropped collection, the list emptied from its front, a document
    after it `Entered`;
  - `select` cutting what is handed out, the order kept;
  - a panic in a `custom` condition ending that query alone;
  - a limit of 0, which reports nothing;
  - **the kept list is `find`'s after every commit:** six filters, a
    sort descending and ascending, two keys, `_id`, a condition, a
    `select`, with an index and without; random inserts, updates,
    deletes, batches and `update_fields` of many, the list kept from
    the places compared with `find` after each of 100 commits;
  - a skip, and a limit without a sort, refused.
- **The `queries` fuzz target** (§105): where the program's query sorts,
  one live query in two takes its sort and limit. After every step each
  event names a document as the model has it, at a place in the list
  as the events before left it, and after them the list is the
  filter applied to the model. The smoke run's 1,000 programs made 1,999
  sorted live queries, which reported 46,384 events; 2,000 of seed 7
  4,004, with 84,928. It draws more of each program, so a seed no
  longer gives the program it gave.
- **Found on the way,** by a program seed 7 now makes: the check of
  `upsert_fields` (§144) was too strict. A filter whose `_id` names a
  document that doesn't match its other conditions inserts under that
  id, which the insert refuses as a duplicate; operators that fail on
  the new document fail first, `Error::Update` for an id the model
  has. Both refusals write nothing; the check now takes either.

## 145.7 Mutations
| Mutation | Caught by |
|---|---|
| No bound on what is known: a full list taken from it whole | reads the next again, the kept list |
| Never read again | reads the next again, the kept list |
| A full list never full | reads the next again, the kept list |
| One pushed out without its document | pushes the last out |
| `Left` from the front | pushes the last out, the kept list |
| `Changed` staying in its place | pushes the last out, without a limit, the kept list |
| `Entered` at the end | pushes the last out, without a limit, a panic, the kept list |
| Ties by id reversed | equal sort values, the kept list |
| Never settled | every sorted test but two |
| Members not written reported `Changed` | every sorted test but three |
| The first state without places | the first matches, equal values, without a limit, `select`, the kept list |
| A limit without a sort taken | refused |

Three survived at first, each code that changed nothing, and taken out:
`_id` taken from the id rather than the document (a decoded document
holds its `_id`, as `find` sorts it); what was read again sorted once
more (it comes in `find`'s order, which is the list's); and what a
commit had noted cleared at a drop (a drop commits alone).
