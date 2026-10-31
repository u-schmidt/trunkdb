# 136. A condition of the application's own (`query.rs`, `document.rs`, `collection.rs`)

The time-series workload (§5.1) asks whether a position is within a
fence. No operator says so, and geometry runs deep: sphere or plane,
the antimeridian, polygons with holes. SQLite and PostgreSQL keep it in
extensions. Other tests no operator covers come up as well: two fields
compared (`end > start`), a rule of the application's (an IBAN, a
version string), a regex until there is a pattern language (ROADMAP.md).
So the application gets a condition of its own, a closure, and brings
its own crates. trunkdb knows nothing of places.

```rust
let fence = fences.get("depot");   // the application's own geometry
let inside = Filter::new()
    .eq("device", "van-7")
    .and(Condition::custom(["pos"], move |doc| {
        match (doc.get("pos.lat").and_then(Document::as_f64),
               doc.get("pos.lon").and_then(Document::as_f64)) {
            (Some(lat), Some(lon)) => fence.contains(lat, lon),
            _ => false,
        }
    }));
```

It serves `find`, the hook's reader (§135) and the live queries
ROADMAP.md plans alike.

## 136.1 The API
- **`Condition::custom(fields, test)`**: `fields` the paths it reads,
  `test` an `Fn(&Document) -> bool + Send + Sync + 'static`. Anywhere a
  condition goes: ANDed, ORed, under a `not`, inside an `elem_match`,
  where paths start at the element (§46), as a column's `filter` (§126).
- **`Condition::Custom(Custom)`**, the variant. `Custom` is opaque:
  `fields()` reads its paths, and how it keeps the closure can change
  without a break.
- **The closure gets the fields it names, never more:** a document of
  the top-level fields its paths start in, `pos` for `pos.lat`. A path
  that starts at the document itself (`""`, or `[*]` first) gets the
  whole document; none named, an empty one. That holds wherever it
  runs: on a document read, or checked in memory.

Naming the fields does two things. A read decodes only the fields its
filter names (§99), so a closure that took the whole document would
decode every field of every document read. And a read decodes the
fields *every* condition names, which is more than the closure's when
it is ANDed with others. Cut down to its own, a field the closure
forgot to name is missing in its first test, not in the one read where
no other condition happened to name it. The cut costs a copy of the
closure's fields for each document checked, and none where the
document read holds no others.

## 136.2 A panic leaves trunkdb as it was
The closure is code of the application's, run inside trunkdb. It is
kept apart from trunkdb's own, as a web server keeps a handler that
panics, and `thread::spawn` hands a thread's panic to `join` as an
`Err`:

- **Each call is caught** (`catch_unwind`), and the read or write that
  made it fails with **`Error::ClosurePanicked`**, carrying the
  panic's message (a `&str` or `String` payload; "a panic without a
  message" for another). What the call found so far is dropped.
- **A write writes nothing.** `update_many`, `update_fields`,
  `delete_many` and `upsert` check their filter inside their batch,
  under the writer's lock. A panic let through would poison that lock,
  and every later write would fail with `Error::Poisoned` until the
  database was opened again. Caught, the batch fails as any batch
  does: its catalog is dropped, the lock let go.
- **A check in memory panics on:** `Filter::matches`, `Filter::apply`,
  `Condition::matches`, `Aggregate::apply`. They return no `Result`,
  and no state of trunkdb's is at stake.

The internal matcher returns `Result<bool, Panicked>`, not `bool`.
Every place that checks a document then has to say what a panic does,
or it doesn't compile: the reads, the writes, `count`'s walks, an
aggregate's columns, the cursor.

**A panic is never taken as `false`.** That would hide the bug, and do
harm: under a `not`, `delete_many` would delete every document the
closure panicked on.

## 136.3 Reading a document, and the traits kept
`Document` had no readers, only a `match`. Added, as
`serde_json::Value` has them:
- `get(path)`: the value at a path of field names (§31), `""` for the
  document itself; `None` where a step is missing or isn't an object.
  A path with `[*]` reaches any number of values, not one, and gives
  `None`.
- `is_null`, `as_bool`, `as_i64`, `as_f64`, `as_str`, `as_bytes`,
  `as_datetime`, `as_id`, `as_array`, `as_object`: `None` for a value
  of another type. `as_f64` reads an `Int` too, as filters compare the
  two (§34.1); `as_i64` doesn't read a `Float`, even a whole one.

They are of use wherever a program reads an untyped document.

`Condition` keeps its derives:
- **`Clone`** clones the `Arc` the closure is kept in: cheap, and the
  clone shares it.
- **`Debug`** shows `Custom(custom(["start", "end"]))`. A closure
  can't be printed.
- **`PartialEq`**: equal only to itself and its clones
  (`Arc::ptr_eq`), with the same paths. Two closures written the same
  are unequal: closures can't be compared.

`Filter` stays `Send`, `Sync` and `UnwindSafe`. `dyn Fn` isn't
`UnwindSafe`, so `Custom` declares it, as a marker that only warns may
be declared: trunkdb looks at nothing a panicking closure left behind.
Without it, code that ran `catch_unwind` around a filter would no
longer compile.

## 136.4 Plans
- **It never bounds a read.** Nothing tells what a closure lets
  through, so `bounds_for` takes nothing from it, and a filter of it
  alone scans. ANDed with conditions that bound an index, it is called
  only on the documents those leave.
- **It is never decided on an entry.** `covered_by` refuses it, so a
  covering read (§100), `count`'s walks (§101) and an aggregate from
  entries (§107.5) read the documents instead.

An index on a coarse key the application writes beside a position (a
geohash cell, say), `is_in` the cells a fence covers, then the closure:
that is what a spatial index does inside, with nothing new.

## 136.5 No text form
`Display` writes `custom(start, end)`, for a person to read in a log or
an `explain`. The parser refuses `custom(…)`, saying a closure in code
has no text. It is the one condition whose text doesn't read back
(§111.6).

## 136.6 Rejected
- **The whole document for the closure:** every field of every
  document read decoded, where a filter decodes only the fields it
  names.
- **The fields the read decoded,** more than the closure named: a
  forgotten field found only where no other condition names it.
- **A panic raised again** in the program: for a live query, the
  writer whose commit it checks would see another's bug.
- **A panic taken as `false`** (136.2).
- **`FnMut`:** called from several threads at once, it would need a
  lock around every call. And trunkdb promises neither which documents
  it sees nor how often: state it changes would count the plan, not the
  data. A closure that wants to count uses an atomic.
- **A trait to implement** instead of a closure: the same, with more
  to write.
- **A closure on a typed `T`,** deserialized for every document
  checked. A typed collection may want it later.
- **Conditions on places built in,** with an index on points: a page
  type of its own in the file, and geometry to keep right. Only if
  `custom`, with a coarse key indexed, is too slow for a program that
  uses it.

## 136.7 Limits
- No index narrows a closure's read; a filter of one alone scans the
  collection.
- The cut copies the closure's fields for each document it checks
  where the read decoded more.
- `std`'s panic hook prints a caught panic as it prints any.
- Built with `panic = "abort"`, a program ends at a panic: there is
  nothing to catch.
- A `Cursor` that met a panic hands out the error and goes on with the
  next document, as after an I/O error.
- **The closure mustn't use the database.** It may run inside a
  write, or as a live query is checked (§138.2), under the writer's
  lock: a write from it waits for that lock for ever. A read from it
  takes a second read lock on the same thread, which can wait for ever
  behind a `compact` waiting for the first to end. What it needs beside its
  document it captures beforehand. To react to a match by reading or
  writing, a live query hands the document to the application's own
  thread, where it may (§135.5). Nothing enforces it: the closure is
  `'static` and may hold a `Database`.
- `update_many`'s own closure, the one that changes each document,
  still runs under the writer's lock uncaught: a panic there poisons
  it, as before this section. §137 catches it, and names the error
  `Error::ClosurePanicked`, for both.

## 136.8 Tests
- `tests/custom.rs`:
  - every read takes it: `find`, `count`, `find_one` sorted, `cursor`,
    sort, skip and limit, `Filter::apply` and `Condition::matches`, an
    OR, a NOT, an aggregate's filter and a column's;
  - inside an `elem_match`, on an element;
  - the closure gets its fields and no others: paths into an object,
    other conditions' fields, the sort's, in memory, a field missing,
    none named, `""`;
  - it never bounds a read, and is called only on what an index
    leaves: `explain`, and the calls counted;
  - every write takes it: `update_fields`, `update_many`, `upsert`
    updating and inserting, `delete_many`;
  - a panic fails each read and each write with its message, a
    formatted one and one without, nothing written (a `not` under
    `delete_many` among them), and the database goes on: a write, a
    read, a `check`;
  - in memory a panic goes on: `apply`, `matches`, `Aggregate::apply`;
  - `Clone`, `PartialEq`, `Debug`, the text and its refusal;
  - a filter with one is `Send`, `Sync` and `UnwindSafe`, and counts
    the same from four threads;
  - the hook's reader testing what changed.
- `document.rs`: each reader for each type; `get` along a path, `""`,
  a null, a missing step, through a number, an array, `[*]`.
- **The `queries` fuzz target** (§105) makes one condition in six a
  closure that checks it, naming the paths it reads. In every plan it
  must answer as the condition does; the model checks it in memory,
  the cut included. Its text is refused, and the typed read takes the
  filter itself. The smoke run's 1,000 programs made 4,537 of them,
  2,000 of seed 7 made 9,564, all answering as the model says.
  It draws one more value per condition, so a seed no longer gives the
  program it gave.

## 136.9 Mutations
| Mutation | Caught by |
|---|---|
| The closure given the whole read, not cut to its fields | its fields and no others |
| The cut keeping no field | every read takes it, and four more |
| Every closure given the whole document | its fields and no others |
| A read decoding without the closure's fields | every read and every write take it, and three more |
| A panic taken as `false` | a panic fails the call; in memory |
| A panic not caught | a panic fails the call |
| `find`'s read dropping the panic | a panic fails the call |
| A column taking a panic as `true` | a panic fails the call; in memory |
| The cursor dropping the panic | a panic fails the call |
| A `not` going past the panic | a panic fails the call |
| Equal to any closure on the same paths | cloned, compared and shown |
| The parser taking `custom(` for a field | cloned, compared and shown |
| `covered_by` taking a closure | its fields; never bounding a read; every read and write |
| `as_f64` not reading an `Int` | each reader for each type |
| `get` through `[*]` | `get` along a path, made stronger: a field named `d[*]` |

`get` through `[*]` survived at first: without the check, the path
reads `d[*]` as a field's name, and the test's document had none.
