# 105. A differential fuzz target for queries (`fuzz/`)

§96–§104 gave the read path shortcuts, each a way to answer without
doing the plain thing:
- walks that stop at the limit, by id (§96, §103);
- keys lent from their leaf instead of copied (§97, §101);
- documents decoded only as far as a read needs (§99);
- index entries deciding a filter without their documents (§100), on
  their bytes (§104);
- whole leaves counted or skipped where their ends decide them (§101,
  §104);
- paths parsed once and sort keys looked up once (§102).

Each section tested its own shortcut: random corpora against `apply_to`,
and hand-made mutations the tests caught. Those corpora are fixed. They
use one seed, one shape of document, one set of indexes, a few hundred
queries, and no writes, reopens or compactions between queries. Three
times a mutation survived a section's first tests, and a test was added
for it (§101, §102, §104): corpora narrower than the code.

The four fuzz targets (§55) all feed damaged files and input. None asked
whether a valid database answers a valid query rightly.

§105 adds a fifth target, `queries`. It turns any bytes into a program
of writes and reads on a valid database, and checks every read against
the same filter applied in memory to the documents the database holds.
It was the ROADMAP's "a differential fuzz target for queries".

## 105.1 The oracle
The target keeps a **model**: the documents it believes are stored, by
id. It holds the database to three things.

1. **The model is what is stored.** A plain read of every document,
   `find(Filter::default())`, which walks the primary index and nothing
   else, gives the model's documents. They are compared field by field
   in any order, floats by their bits, since a NaN isn't equal to
   itself (`same`). This catches writes lost, doubled or misapplied.
   - In full: on one write step in four, after every compaction and
     reopen, and at the end.
   - On the other write steps: only the count and four documents read by
     id. Reading every document on every step took most of a run's time
     (105.4).
2. **Every read answers as `apply_to` on the model does.** A query is
   run through every way a read can go:
   - `find`, untyped;
   - `find` typed, into a struct of only `_id`, decoded straight into
     `T` (§99);
   - `cursor`, collected;
   - `find_one`;
   - `count`;
   - `explain_run`, whose `documents_returned` must be `find`'s length.

   Each kept snapshot is asked the same, through its `View`, against
   the model as it was when the snapshot was taken (§82). Each answer is
   held against `filter.apply_to(model)`:
   - **With a sort:** the same documents in the same order. Ties come in
     id order on both sides: `apply_to` sorts stably over a model kept in
     id order, and the walks hand ties out by id (§47.3).
   - **Without a sort, skip or limit:** the same set.
   - **Without a sort but with a skip or limit:** which matches come
     back isn't defined. They must be distinct, each must be a match as
     the model has it, and there must be as many as `apply_to` gives.
   - **`count`:** `apply_to`'s length.
3. **The file checks out.** `check` (§39) finds nothing wrong. It runs
   after each compaction and reopen, at the end, and on one step in
   sixteen.

**The meaning is `apply_to`'s.** That uses the same `Matcher` and the
same comparison as the database. So the target tests every route to an
answer against the one meaning of a filter, not the meaning itself.
That has its own reference in the unit tests, the code from before §102
kept in `query.rs`'s test module.

**Writes the model can't predict in full** take their result from the
database:
- **`update_fields`.** The target doesn't reimplement update operators.
  Which documents it touches is `apply_to(filter)` on the model, and the
  model takes those back with `get`. Rule 1 catches a change outside that
  set. An `Error::Update`, or an update refused as `InvalidInput`, leaves
  the model as it was.
- **`upsert`** replacing a match keeps the matched id: the model takes
  that document back with `get`.

`delete_many` and `update_fields` act on what `find` returns. Without a
total order, a skip or limit picks among matches in a way that isn't
defined. So the target gives them `_id` as a last sort key
(`Query::total`). It also limits them to 10 and 20 documents
(`Query::few`): one of everything emptied the collection the steps after
it should read.

Every other error from a write fails the run, unless the model predicts
it:
- `DuplicateId` for an insert of an id it holds;
- `NotFound` in a batch for an update or delete of an id it lacks;
- `MultipleMatches` for an `upsert` whose conditions match more than
  one;
- `SnapshotOpen` for a `compact` beside a kept snapshot.

A read never fails.

## 105.2 The program
The input is read as a program, a byte at a time (`Program`). Past its
end every byte reads 0, so any input is a valid program. It stops where
its bytes do, or after 64 steps, on one collection of at most 400
documents.

**A prelude** starts every program with data, so that the steps meet
indexes of several leaves from the first:
- the program's choice of how its values look (105.3);
- up to three indexes;
- 100 to 300 documents in one batch.

Without it, a program spent its steps on an almost empty collection.

**The steps**, the next byte choosing; reads and inserts come up more
often:
- **An index made or dropped**, from a menu of nine:
  - `a`, `b`, `o.p`;
  - multikey `tags[*]` and `tags[*].k`;
  - compound `(a, b)`, `(b, a)`, `(a, o.p, c)` and `(aa, a)`;
  - each plain or sparse (§44).

  `(aa, a)` puts a name and its prefix in one entry's document (§102.3).
  An index is also made between writes, over documents already stored.
- **Writes:**
  - an insert;
  - a batch of up to 32 inserts, updates and deletes, half of them
    inserts;
  - a replace by id;
  - a delete by id;
  - `delete_many`;
  - `update_fields` with up to three of `set`, `unset`, `inc`, `min`,
    `max`, `push`, `pull`, `add_to_set` and `rename`;
  - `upsert`.
- **Upkeep:**
  - `checkpoint`;
  - `compact`;
  - a reopen, dropping every handle and opening the file again;
  - a snapshot kept, at most two at once.
- **A read** of a query (105.3).

**Ids come from the input:** 16 bytes, a counter in the low bytes and an
input byte at the top, so that id order and insertion order differ.
`id_gen` takes the clock, and a run couldn't be repeated.

## 105.3 Documents and queries
**Values** come from four sources:
- **Repeating ones:** small integers and short strings, so that an
  `Eq` finds runs.
- **Long strings** of 400, 990 or 1,100 bytes that differ only at their
  end, from 8 or from 256 endings:
  - 400 bytes are exact in a key of one or two fields and cut in one of
    three;
  - 990 bytes are exact in one field;
  - 1,100 bytes are cut everywhere.

  They make indexes of leaves of a few entries, so a few hundred
  documents give many leaves. Cut ones share their keys.
- **A pool built to collide**, as in §104.7:
  - numbers past 2^53 a few apart, the extreme `i64`s, `Int` beside
    `Float`, `-0.0`, both infinities, NaN;
  - strings cut alike, `"a\0"`, `"Straße"` and `"STRASSE"`;
  - ids, date-times a nanosecond apart, null, both bools, an array, an
    object and binary.
- **What a stored document holds**, two times in three, for a condition,
  so that conditions match.

**Per program**, the prelude chooses:
- how often a value is long: one in eight, half, or seven in eight;
- and, one time in three, **runs**: long strings from four values, two
  common and two rare. That gives runs of one value over several leaves
  with a short run between, which is where a range's bounds meet whole
  leaves.

**Documents** have:
- `a`, `aa`, `b` and `c`, each missing now and then;
- `o`, mostly an object `{p: …}`, sometimes a plain value;
- `tags`, an array of values and objects `{k: …}`, or now and then not
  an array;
- `blob`, now and then a string of 5 KB, so that some documents live in
  overflow pages (§26).

**Queries** are of two kinds, half each where an index exists.
- **Random:**
  - up to three conditions, nested three deep, through `All`, `Any` and
    `Not`: comparisons with every op on every path, `_id` and one
    nothing has; `exists`, `missing`, `size`, `elem_match`, and an OR of
    `eq`s on two paths, which a union of index ranges reads (§36.3);
  - up to three sort keys;
  - a skip and a limit of every size.
- **Focused on one existing index** (`Run::focused`):
  - `eq`s fixing its first fields, then ranges on the next one or two;
  - now and then strictly between two stored values, ordered with
    `Condition::matches`, since the target can't compare values itself;
  - now and then a `ne`, `contains`, an OR or a NOT on its fields, and
    an `_id` bound;
  - sorted by `_id`, by its fields, or anyhow.

  Focused queries are the ones whose entries decide (§100, §104), whose
  leaves are passed whole (§101), and which walk an `Eq`-fixed range by
  id (§103). Random ones seldom put every field they name in one index.

Every plan comes up: `Scan`, `ById`, `Index` and `IndexOrder` on each
kind of index, `IndexOrder` on `_id`, and `IndexUnion` across them.

## 105.4 Where it runs
- **The target:** `fuzz/fuzz_targets/queries.rs`. Its logic is
  `trunkdb_fuzz::queries::run`, in `fuzz/src/queries.rs`. To fuzz it, by
  hand on nightly: `cargo +nightly fuzz run -s none queries corpus/queries
  -- -max_len=20480`. A program needs room: the prelude alone takes up to
  6 KB, and libFuzzer's default of 4 KB would end most programs there.
- **Seeds:** `seed_corpus` writes 16 programs of 4 to 20 KB into
  `corpus/queries`.
- **The smoke run:** `RUSTFLAGS="--cfg fuzzing" cargo run --release
  --example queries_smoke -- <programs> [<seed>]`; a single program
  again with `--example queries_replay -- <file>`. It runs `run` on
  random programs of 4 to 20 KB, on stable, without libFuzzer.
  - A program that fails, or runs past a minute, is written to
    `artifacts/queries/smoke-<seed>-<n>`, for the target to replay.
  - At the end it counts the steps by kind and lists the plans that came
    up.
  - Built without the cfg, it says so. Every commit is then flushed, and
    a run is about 40× slower: the cfg skips the flush, as `cargo fuzz`
    builds do (§55.1).
- **CI** runs the smoke run in the fuzz job: 400 programs, the seed from
  the run's id, so each run tries new ones. A failing program is kept as
  an artifact. CI also runs `seed_corpus` now (105.6).

**Speed.** About 19 programs a second on an M1 Pro, each up to 64 steps
on 100 to 400 documents. The first version ran 470 a second, on programs
that ended inside their first dozen steps and on nearly empty
collections (105.5). Profiled:
- rule 1 took 62% while it compared documents as text, with their long
  strings and blobs formatted on every step; 26% compared as values;
  less since it runs in full on one write step in four;
- the reads, six ways each and again for each kept snapshot, and
  `check` and compaction, most of the rest.

## 105.5 Making it find things
The target is only as good as the programs it makes. Its first version
ran 100,000 programs on the unbroken code without a false alarm. Then
48 mutations from §100–§104's lists were put back one at a time; four
no longer applied to the code. Of the 44 left, it caught 20 in 20,000
programs each. The misses were all one blind spot: the shortcuts act
where a query's fields all sit in one index of several leaves, and
random programs rarely made one. The generator was widened step by step,
each time measured against the misses:
1. **The prelude, long strings with endings, and focused queries.** 10
   more caught.
2. **The prefix-named field `aa`, programs choosing how often values
   are long, and ranges on two fields.** One more caught: a slot found
   by a name's prefix.
3. **Programs that actually ran.** Instrumenting the library for a
   moment showed `Walk::whole_leaf` asked about once a program. The
   smoke run's inputs, up to 2 KB, ended most programs inside the
   prelude. At 4 to 20 KB, programs run their 64 steps: the inexact
   group skipped whole was caught.
4. **Collections that stay large.** `count_in` met ranges of about seven
   entries: an unlimited `delete_many` or `update_fields` emptied or
   flattened the collection. With them limited, a prelude of 100 to 300
   and batches half inserts, the interval rule on `ne` was caught.
5. **Runs, and bounds strictly between stored values,** for the interval
   rule's last two cases. Neither was caught yet (below); both stay, as
   they make those shapes possible.

Final count, 1,500 programs per mutation, on the finished generator:
**33 of 44 caught**, one of them as a hang (a backward walk that never
ends). Most took under 200 programs, some up to 1,100. The 11 missed:
- **Five change no answer**, only how much is read or the plan:
  - a cursor reading matches it skips;
  - a range's end leaf counted whole: entries outside a range never
    match, so its ends can't both decide true;
  - a walk on another index than the range read's;
  - `gt` on `_id` keeping its boundary id, which the filter rejects
    anyway;
  - a later sort key served after `_id`.
- **One can't happen on a file this build writes:** tombstoned slots
  counted. Since §52 a removal closes the gap; only older files have
  tombstones.
- **Two are in code the oracle shares:** a `contains` needle not
  case-folded, and `Ne` on `[*]` as "any value unequal". These are
  `Matcher` itself (105.1).
- **Three are rare shapes of §104's interval rule:** on a later field
  without its `eq`, on `ne` (caught at program 669 in an earlier run,
  not in this one), and a leaf decided false whole. A probe shows the
  last needs a short run strictly inside a leaf, between leaf-filling
  runs of both bounds' values. With 40 values of the lower bound it
  isn't reached; with 41 it is. §104.7's tests build that on purpose.

So of the 36 mutations that change answers in a file this build writes,
33 are caught by the smoke run alone.

## 105.6 What it found
- **The fuzz targets' own database didn't build.** `trunkdb_fuzz::base`,
  the valid database `open_file`, `open_raw` and `recover_wal` start
  from (§55.1), inserted plain numbers into a scratch collection. Every
  write refuses that since §95, so every one of those targets panicked
  at its start. CI built and linted the fuzz crate, which compiles
  either way. Writing `queries`' seeds through `seed_corpus` hit it. The
  scratch documents are objects now, and CI runs `seed_corpus`, which
  builds `base`.
- **`compact` beside a snapshot** returns `Ok` where the file is already
  as small as compaction would make it: nothing is rewritten, so
  nothing changes for the snapshot. The doc comment said it was refused
  whenever a snapshot was open. The comment now says both; the target
  accepts both.

- **A data page leaked** (`data.rs`). The smoke run's 25,626th
  program of seed 105105 failed `check` after a reopen: "page 62
  belongs to nothing (leaked)". It was a data page that held no
  documents and was neither a collection's current page nor free.
  - **Finding the step.** The replay example with `QUERIES_TRACE` ran
    `check` after every step. The first step to leave the file wrong was
    an `update_fields` of 20 documents, while a snapshot was kept.
    `cargo fuzz tmin` cut the program from 12 to 8 KB; most of the rest
    is the prelude's documents. Probes of the obvious suspects (overflow
    chains beside a snapshot, a refused compaction, a reopen) didn't
    reproduce it, so the snapshot was a bystander.
  - **The cause.** A page left without documents is freed
    (`write_or_free`), unless it is the collection's current page,
    which the next insert takes. A current page emptied by deletes
    keeps its slot directory, four bytes a dead slot. So a cell close to
    the inline maximum can be turned away by an empty page.
    `place_cell` then allocated a new current page and put the old one
    into the free-space map (§75), where nothing owned it and nothing
    would free it.
  - **The fix.** A page that stops being current is freed if it holds
    nothing, as any other emptied page is.
    `an_emptied_current_page_given_up_for_a_new_one_is_freed` builds
    it: 40 small documents deleted from the current page, then one as
    large as fits a fresh page. It failed before the fix.
  - **Files written before the fix** can hold such a page: space lost,
    no document. `check` reports it as leaked, and `compact` gives it
    back, as for any leaked page.

Nothing else was found in:
- **The smoke run:** 100,000 programs of seed 105105 on the fixed code,
  the leak's among them: 5,662 s, 18 a second.
- **Guided fuzzing:**
  - an hour before the fix: 30,699 programs, 2,915 inputs added,
    coverage to 9,137 edges;
  - half an hour after it, from that corpus: 13,951 programs, 249 added,
    9,226 edges.

  The hour didn't find the leak. Its `check` ran on one step in sixteen,
  and the leak's shape (a large document after many small ones deleted
  from the current page) is a narrow one.
- **The four damaged-input targets**, two minutes each once `base`
  built again: about 110,000 runs.

A failing program is replayed with `queries_replay`. With
`QUERIES_TRACE` set, it says each step and checks the file after every
one.

## 105.7 Rejected
- **A second matcher in the target**, as the oracle: a second meaning to
  keep in step, which would disagree in the corners (§34.1, §32) for
  reasons that aren't bugs.
- **Another database as the oracle:** SQLite's type order, null and
  missing differ.
- **Unique indexes** (§33), until §119: which writes they refuse would need the
  model to know when two values are equal across `Int` and `Float`,
  array elements and nulls.
- **`arbitrary`:** `Document` and `Filter` aren't `Arbitrary`, and
  wrappers would be most of the work. A byte reader with defaults keeps
  every input valid.
- **Threads, and damaged files:** runs that can't be repeated, and the
  other targets' ground.
- **Rule 1 in full on every step:** it was most of a run's time (105.4).

## 105.8 Limits
- **The meaning itself isn't tested** by the target, only every route
  to it (105.1, 105.5).
- **One collection, one thread, no damage;** no unique indexes until §119.
- **Random programs in CI, guided ones by hand.**
- **Rare shapes** of the interval rule are left to §104's own tests and
  to guided fuzzing (105.5).
- **Collections of at most 400 documents.** Long keys make up much of
  that for leaves, not for depth: trees stay two or three levels deep.
