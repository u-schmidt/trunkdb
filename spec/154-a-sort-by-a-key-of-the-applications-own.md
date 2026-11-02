# 154. A sort by a key of the application's own (`query.rs`, `collection.rs`, `live.rs`, `query_text.rs`)

A sort orders by what a field holds. Some orders no field holds: "the
ten restaurants nearest to this point", a distance from where the user
stands; the vectors nearest to a query vector; a score the application
computes from several fields. §136 gave conditions a closure of the
application's; this gives sorts one.

```rust
let (lat, lon) = (52.52, 13.40);
let nearest = Filter::new()
    .gte("lat", lat - 0.1) // a band an index on `lat` reads
    .lte("lat", lat + 0.1)
    .sort_by_key(["lat", "lon"], move |doc| {
        let a = doc.get("lat")?.as_f64()?;
        let b = doc.get("lon")?.as_f64()?;
        Some((a - lat).hypot((b - lon) * lat.to_radians().cos()))
    })
    .limit(10);
```

trunkdb computes nothing of places, as §136 left geometry to the
application: the distance is the application's, and so is the choice
of flat or spherical, of degrees or metres. Nor is it a vector store
(ROADMAP.md, Not planned): an exact search by a scan serves thousands
of vectors to tens of thousands, not millions.

## 154.1 The API
- **`Filter::sort_by_key(fields, key)`**, smallest first, and
  **`sort_by_key_desc`**, largest first: `fields` the paths `key`
  reads, `key` an `Fn(&Document) -> Option<f64> + Send + Sync +
  'static`. Either replaces any earlier sort, and `sort_asc` and the
  rest replace it, as one sort replaces another.
- **The closure gets the fields it names, never more,** cut as
  `Condition::custom`'s are (§136.1): a forgotten field is missing in
  its first call, not only where no condition happens to name it.
  `_id` too: named, it is there; not named, it isn't, unlike a
  `select`, which hands it out always (§106). Equal keys go by id
  without it; a closure that looks something up by id names it.
- **`None` leaves the match out,** and so does NaN: no order holds it.
  `find`, `count`, `cursor`, the writes and `aggregate` all act as if
  it didn't match. The key is a condition too, then, the one that
  costs nothing more where it is computed anyway: "the nearest with a
  position".
- **Equal keys** go by the `then_asc` and `then_desc` fields after it,
  then by id, as a sort by fields orders matches equal in every field
  (§47).
- **A number, not a value of any kind:** distances and scores are
  numbers, and a number compares without the order of kinds a field
  sort needs (§34.1). A key of several parts is sorted by its first,
  then by fields; a tuple of numbers isn't offered.

## 154.2 How it runs
- **Every match is read and keyed.** No index holds a key of the
  application's, so `index_order` takes nothing from it, and a sort
  from entries (§102.4) never serves it. Conditions an index serves
  narrow what is read, as the band on `lat` does above; entries that
  decide a candidate is no match still leave it unread (§100).
- **With a limit, the best `skip + limit` are kept** as the matches are
  read (`find_by_key`): each match goes into a list with its key, and
  when the list holds twice that many, a selection
  (`select_nth_unstable_by`) cuts it back to the best. Memory stays at
  about twice `skip + limit` matches, read in full, however many
  match, and the work at about one comparison per match. Without a
  limit, every match is kept, as for any sort.
- **The candidates stream:** `read_candidates`, which gathered every
  candidate before any was checked, is now `each_candidate`, handing
  each one on as it is read; `read_candidates` gathers them through it,
  as before.
- **`count`** with a key reads every match and keys it, rather than
  counting entries (§101), as it must agree with `find`.
- **A panic** in the closure fails the call with
  `Error::ClosurePanicked`, a write with nothing written, the writer's
  lock not poisoned (§136.2); in memory (`Filter::apply`) it panics on.
  Where a sorted live query is checked, it ends that query alone, as a
  condition's does (§138); at its start it fails `watch`.
- **`explain_run`** counts the key as one more sort key in memory.

## 154.3 Settled here
The roadmap left three questions to this section:
- **The key isn't handed back with its document.** `find` returns
  documents, and a pair would need a method of its own beside `find`,
  `find_one`, `cursor` and their typed forms. The application computes
  the key again for the ten it shows, at the cost of ten calls, from
  the fields the document holds. If a program finds that costly (long
  vectors, many found), a `find_keyed` can follow.
- **A live form:** `watch` takes a key sort as it takes a field sort
  (§145). A member's sort values are its key, as a number, then the
  fields after it; a document whose key is `None` isn't a member. "The
  ten vans nearest to the depot", kept as they move.
- **No text form:** `Display` writes `order by key(lat, lon)`, and
  `desc` after it where it is largest first, and the parser refuses
  `key(…)` there, as it refuses `custom(…)` (§136.5). A field named
  `key` still sorts.

## 154.4 Measured

On the machine of §123: 100,000 places, `lat` and `lon` random over
5° by 7°, an index on `lat`; the distance as in the example; each the
mean of 20 runs (5 for those reading every document), two rounds
agreeing to within 4%:

| | time | found |
|---|---:|---:|
| ten nearest, a band of `lat` ±0.1 (3,948 read) | 5.9 ms | 10 |
| the band alone, unsorted | 4.1 ms | 3,948 |
| ten nearest, all 100,000 read | 60 ms | 10 |
| all 100,000 by distance, no limit | 192 ms | 100,000 |
| ten by `lon`, a field without an index, all read | 75 ms | 10 |
| `count` with the key, all 100,000 | 48 ms | 100,000 |
| ten most similar of 5,000 vectors of 128 floats, cosine | 8.8 ms | 10 |

- **The band is most of it:** keying 3,948 matches and keeping ten
  costs 1.8 ms over reading them. Narrowing the read is what makes
  "nearest" fast, as with a spatial index's cells.
- **Ten of all, kept as read, beat a sort by a field** with the same
  limit and no index (60 ms against 75): a field sort in memory still
  gathers every match and sorts them all (§102.5). The same keep would
  serve it; left for a measurement of its own (ROADMAP.md).
- **No limit** keeps and sorts every match, 3.2× the time.
- **Vectors:** 5,000 of 128 dimensions in 8.8 ms, about 1.8 µs each,
  most of it decoding the array. Tens of thousands stay under a tenth
  of a second; millions want a vector store.

## 154.5 Rejected
- **A `BinaryHeap` of the best:** a heap orders by `Ord`, which a
  comparison through the filter's sort fields doesn't give without a
  wrapper holding the filter; the selection takes a closure, and is
  as cheap in all, at twice the memory at most.
- **A key of any `Ord` type,** or a `Document`: generic, it makes the
  `Filter` generic or the key a trait object of its own; a `Document`
  key brings the order of kinds, which a number has no need of. A
  number serves distances, similarities and scores.
- **The key with the document,** `Vec<(f64, Document)>` (154.3).
- **A key from an index's entries** where they hold its fields: a
  closure is never run on an entry (§136.4), so that a damaged key or
  a rounded value (§28.1) never reaches it.
- **`FnMut`,** as for `custom` (§136.6): called from any thread, and
  trunkdb promises neither which documents it sees nor how often.
- **Geometry built in,** a `near(point)` sort: as for conditions on
  places (ROADMAP.md, Deferred).

## 154.6 Tests
- `every_read_orders_as_the_key_says` (`tests/key_sort.rs`): 300
  places on a grid, so many keys are equal, some with no position and
  some whose key is NaN; with and without an index band and a
  condition no index serves, smallest or largest first, with no field
  after the key, one, one in the other direction, or one an index
  holds, skips of 0 and 3, limits of none, 0, 1, 7, 40 and more than
  match. `find`, `count`, `cursor`, `find_one`, a `select`,
  `Filter::apply` and a snapshot each against a model worked out
  apart.
- `the_nearest_ten_as_the_docs_show_it`, the example above.
- `writes_take_the_same_matches`: `delete_many` of a page of the
  nearest, `update_fields` of the three farthest.
- `an_aggregate_takes_the_matches_the_key_picks`.
- `the_closure_gets_the_fields_it_names_and_no_others`, with
  conditions and a field after the key naming others.
- `the_closure_gets_the_id_only_if_it_names_it`, on a scan and on a
  read through an index.
- `every_match_is_keyed_once_and_the_rest_not_at_all`.
- `a_panic_fails_the_call_and_the_database_goes_on`: `find`, `count`,
  `cursor`, `delete_many`, and in memory.
- `it_has_text_but_none_that_reads_back`,
  `a_later_sort_replaces_it_and_it_replaces_an_earlier_one`,
  `explain_counts_it_as_a_sort_in_memory`.
- Live (`tests/live.rs`): three key sorts in
  `the_kept_list_is_find_s_after_every_commit`, the list kept from the
  events against `find` after each of 100 random commits;
  `a_panic_in_a_sort_key_ends_that_query_alone`, and fails `watch` at
  the start; `a_key_of_none_keeps_a_document_out_of_the_list`.
- The full test suite, clippy, the MSRV build (1.89).

## 154.7 Mutations
Each run against the library's tests, `tests/key_sort.rs` and
`tests/live.rs`:

- in the key's order: `desc` not reversed, the fields after the key
  ignored, NaN kept, equal keys not by id: caught;
- the closure given every field, the key's fields not read: caught;
- an index walked for a key sort (with a field after the key that an
  index holds), entries taken to hold the key: caught;
- a field sort leaving the key in place, a key sort keeping the field
  sort, a key sort taken as no sort: caught;
- the skip not applied, in memory or in `find`; the best cut to one
  fewer than kept, or the cut keeping the worst: caught;
- `count` ignoring the key, or not reading what an entry decided:
  caught;
- live: the key's order left out, a key of `None` taken as a member,
  the list read again without the key: caught;
- the text without `desc` after the key: caught.

Not run, as nothing a test sees changes, only the memory: the list
never cut back (it would hold every match), or cut back at once at
`skip + limit` (one selection per match).
