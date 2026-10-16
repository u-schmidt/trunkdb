# 99. Decoding only what a read needs (`document.rs`, `serde_stored.rs`, `data.rs`, `collection.rs`, `cursor.rs`, `query.rs`)

Every read decoded every document it read in full, into a `Document`
tree:
- an `IndexMap` per object;
- a `String` per key;
- a `String` or `Vec` per value.

A typed read then converted that tree into `T` (`from_document`, the
serde bridge, §13). A `DocId` field even went through its text: the
bridge formatted the id, and `DocId` parsed it back.

Measured per document on the benchmark's tasks (§48):

| | cost |
|---|---:|
| decoding it whole | ~1,050 ns |
| decoding one field of it alone | ~300 ns |
| walking all of it with every check, building nothing | ~93 ns |
| `Document` → `Task` | ~150 ns |
| serde_json straight into `Task` | 400–500 ns |

After §97, decoding was most of what a read cost:
- "first 50 by id" took 70 µs, where redb takes 25 µs;
- "tries > 7", unindexed, read all 100,000 documents in 168 ms.

Much of that was for nothing:
- **A rejected candidate** was decoded in full to look at one field.
- **A match passed over** by `skip`, or cut by a `limit` after a sort in
  memory, was decoded and dropped.
- **A typed read** built a tree only to take it apart, and decoded
  fields its `T` doesn't have.

§99 decodes each document only as far as the read needs it, and a typed
read straight into `T`. It is the second of the two causes the probe
behind §97 found.

## 99.1 A document held as it is stored
A read carries a candidate as `Stored` (`data.rs`): its id and its
encoded bytes, undecoded.
- **An inline cell:** the `SlottedPage` it is on, shared with the cache
  (`Arc`), and its slot. No copy.
- **An overflow chain:** the bytes put together, as `Records::get` does.

`Records::stored` reads one. `Records::get` still decodes, for writes,
`check`, `compact` and export.

`Stored::decode` gives the whole document with its `_id`, as
`Records::get` does. `Stored::deserialize` gives a `T` (99.4).
`Stored::fields` gives some fields (99.2).

## 99.2 Matching and sorting on the fields they name
`Filter::fields()` collects the top-level field of every path it looks
at:
- in its conditions, through `All`, `Any` and `Not`;
- for an `elem_match`, its array's path: the paths inside start at the
  element;
- in its sort keys.

`a.b` names `a`, and `tags[*].x` names `tags`. A path that starts at the
document itself (empty, or `[*]` first) gives `None`, which decodes the
whole document.

`document::decode_fields` decodes those fields only, into an object,
with `_id` when it is named. Every other field is skipped (`skip_value`)
with the checks a decode makes, building nothing. A path's value depends
only on its top-level field, and a missing field reads as null
everywhere (§32), so `filter.matches` gives the same answer on this
object as on the whole document, and the sort orders the same. When no
stored field is named, nothing is read at all.

Where:
- **Every candidate** of `find`, `find_one`, `cursor`, `count`,
  `delete_many`, `upsert`, and the matching of `update_many` and
  `update_fields`, on `Collection` and `View`.
- **A walk of the primary index** that serves the whole sort (§96) needs
  no sort key: only the conditions' fields (`fields_with(false)`).
- **`count` and `delete_many`** decode nothing else.

**One exception, for untyped reads, which the draft didn't have.** The candidates of an index range or a lookup by id nearly all
match. Checking each on its fields and then decoding it whole cost more
than decoding it whole once: untyped "tenant == x" (1,000 documents)
took 2.13 ms against 1.73 ms before. So an untyped read (`find`,
`cursor`, `update_many`, and `explain_run`, which mirrors `find`)
decodes those candidates whole, checks that, and hands that document
out (`Found::whole`). A scan's candidates are mostly rejected, and a
walk's are few, so both are checked on their fields. Typed reads always
are: what they decode in the end is the `T`, not a `Document`.

## 99.3 Decoding last
The full decode waits until filter, sort, skip and limit have run
(`find_in` returns `Found`, the stored documents in order):
- **untyped:** `Found::decode`, into a `Document`, unless it was
  decoded whole already;
- **typed:** `Stored::deserialize`, straight into `T` (99.4).

A sort in memory with `limit(20)` over 1,000 matches decodes 20
documents whole, and page 3 of 50 decodes 50, not 150. The `Stored`
documents in between hold their pages, the cache's, not copies.

## 99.4 Typed reads straight into `T`
`serde_stored.rs`: a serde `Deserializer` over the encoding.
- **The document's `_id`** comes first, from the cell, as `with_id` puts
  it. A stored `_id`, as files before §59 have, is passed over, as
  `with_id` replaces it.
- **Then the stored fields.** A field `T` doesn't declare is skipped
  through `ignored_any`, so "reading only some fields" (README) costs
  less too.

**It gives `T` exactly what the bridge gives it** from the decoded
`Document`. For scalars, strings, binary, ids, arrays, objects and
`Option`, it makes the same visitor calls:
- an id as its text through `visit_str`, formatted on the stack;
- binary through `visit_byte_buf`;
- object keys as strings.

The rare shapes are decoded into a `Document` and handed to the bridge
itself, so they can't drift from it:
- an enum (§13);
- a date-time, which is a two-field map to `deserialize_any` and its
  text to `deserialize_str` and `DateTime`'s newtype (§69, §70);
- a document that isn't an object (only damage since §95).

A visitor that stops early in a map or an array leaves the rest, which
is passed over with the same checks, so what follows reads as it should.

**Damage (§55, §56) reads as damage, exactly as before.**
- Every byte of the document is decoded or skipped with the checks a
  full decode makes: tags, lengths, UTF-8, date-times, the nesting
  limit.
- A document that is both damaged and wrong for `T` is reported as
  damaged, as the full decode found damage before any value reached
  `T`.
- Damage is `Error::Io` of kind `InvalidData`; a value that doesn't fit
  `T` is `Error::Document`, with the bridge's message.

The bridge stays: writes use it, and so does the typed `update_many`,
which needs a `Document` to write back.

## 99.5 What stays
- **The counts of `explain_run` (§94):** a document read is examined,
  decoded in part or in full.
- **Every result and its order.** Only how much is decoded changes.
- **The API.** `Cursor`'s conversion takes a `Found`, internally.

## 99.6 Rejected
- **Matching on the encoded bytes directly**, a byte view with its own
  `matches`: a second implementation of every condition (§36, §42, §45,
  §46, `Contains`'s case folding) to keep equal to the first. The
  partial decode reuses the one there is.
- **Lengths before values in the encoding**, to skip a field in one
  step: a new file format, for a walk that costs 93 ns a document.
- **A cheaper `Document`** (shared keys, short strings inline): it helps
  untyped reads, but it is still a tree, and a separate decision.
- **Borrowing from the page** (`&'de str` fields in `T`): `Collection<T>`
  takes `DeserializeOwned`, and a borrow would tie results to the read.
- **Decoding on match without deferring:** it decodes the skipped
  matches and everything a limit cuts after a sort in memory.
- **Checking untyped index-range candidates on their fields** (the
  draft's leaning): measured 25–35% slower than before (99.2).

## 99.7 Tests
- **The deserializer and the bridge agree**
  (`the_deserializer_agrees_with_the_bridge`):
  - 20,000 random documents: objects of up to five fields, three levels
    deep, with every kind of value;
  - each read as seven targets, from a struct of every field kind
    (scalars, `char`, binary, `Vec`, `HashMap`, `DateTime`,
    `SystemTime`, enums of all four shapes, an internally tagged enum, a
    newtype, a tuple, an untagged "anything", `DocId`) to a strict one
    (`deny_unknown_fields`), a summary, a flattened map, `BTreeMap`,
    `Option`;
  - the same value or the same error message every time; every target
    fits some, the strict ones fail many.
- **Chosen shapes:**
  - every enum shape and its errors;
  - date-times as each kind of field;
  - ids and binary;
  - documents that aren't objects;
  - a visitor that stops after a map's first entry, and a tuple
    shorter than its array;
  - a stored `_id` from before §59.
- **Damage:** for 300 random documents, every truncation and three
  altered bytes at every position fail a typed read exactly where, and
  with the message, the full decode fails. That covers what the type
  takes, what it skips (`IgnoredAny`), and a wrong type before the
  damage. The nesting limit holds in what is read and skipped.
  `some_fields_are_damaged_when_the_whole_is` does the same for
  `decode_fields`.
- **Partial matching:** the randomized corpora (§47, §72, §96, compound,
  sparse, multikey) compare `find` with the filter applied in memory to
  whole documents. They now also read typed (`IdOnly`, every field but
  `_id` skipped), through `find` and `cursor`, and must agree.
  `a_filter_names_the_fields_it_looks_at` covers every kind of
  condition, nested.
- **Decoded last** (`only_what_is_handed_out_is_decoded_whole`, a test
  counter):
  - a sort in memory with `limit(20)` over 1,000 decodes 20, untyped and
    typed;
  - a cursor decodes what it hands out;
  - page 3 of 50 decodes 50 of 150 read;
  - `count` and `delete_many` decode none;
  - an untyped index range decodes each candidate once, a typed one
    only its matches.

Fourteen hand-made mutations, all caught:
- `Filter::fields` missing an OR branch, a `Not`, an `elem_match`, or
  the sort keys;
- `_id` left out when named;
- a skip that doesn't check UTF-8, or the nesting limit;
- untyped candidates decoded whole before skip and limit;
- `_id` not handed to `T`;
- an id as its bytes;
- a date-time as text in `deserialize_any`;
- a stored `_id` not passed over;
- damage not preferred over a wrong type;
- entries a visitor left not skipped.

Two survived the first run: a stored `_id`, which `encode_stored` never
writes, and a visitor that stops early, which no derived type is. The
tests above add both.

Benchmark (§48), trunkdb alone, before (`aa93466`) and after, back to
back twice on 100,000 documents:

| | before | after |
|---|---:|---:|
| get by id | 3.9–4.0 µs | 3.4–3.8 µs |
| tenant == x (1,000 docs) | 1.87–2.00 ms | 1.44–1.52 ms |
| created in a range (1,000 docs) | 1.99–2.19 ms | 1.49–1.60 ms |
| status == x, oldest 20 | 39 µs | 35–39 µs |
| status == x, newest 20 | 43–44 µs | 39–42 µs |
| first 50 by id | 68–70 µs | 35.5–37 µs |
| scan: tries > 7, unindexed | 168 ms | 62 ms |

Untyped (the probe), before and after:

| | before | after |
|---|---:|---:|
| first 50 by id | 59 µs | 61 µs |
| tenant == x | 1.79 ms | 1.69 ms |
| created in a range | 1.74 ms | 1.66 ms |
| scan: tries > 7 | 165 ms | 88 ms |
| `count`, tries > 7 | 157 ms | 58 ms |

The other stores, same run: first 50 by id SQLite 34 µs, redb 24 µs,
sled 30 µs; the scan SQLite 44 ms, redb 51 ms, sled 70 ms. The draft
expected about 40 µs and got it. It also expected the scan under the
other stores, and it isn't: each of its 100,000 candidates still builds
a small object for the one field its filter names (99.8).

Full suite, clippy `-D warnings` with and without all features, fmt,
rustdoc, the fuzz and bench crates.

## 99.8 Limits
- **A scan still allocates for each candidate:** the small object of
  the fields its filter names, about 200 ns of the 300.
- **An untyped read** still builds a `Document` for each document it
  hands out.
- **Skipping walks the encoding:** a large array is skipped item by
  item, without allocating.
- **A filter that names most fields** gains little from the partial
  decode.
- **The typed `update_many`** still converts through the bridge.
