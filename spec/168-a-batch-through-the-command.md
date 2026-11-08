# 168. A batch through the command (`batch.rs`, `collection.rs`, `database.rs`, `bin/trunkdb.rs`)

Each write command of `trunkdb` is a batch of its own (§156): an
`update`, then an `insert`, are two commits, and another write can come
between them. A script that marks every overdue task and logs that it
did, or takes the next queued job and records who took it, wants one
commit with no gap between reading and writing. A Rust program had
`Batch`, but a batch wrote by id only: what a filter finds had to be
read first, outside the writer's lock, and could change before the
commit.

So `Batch` takes filters too, and `trunkdb batch` reads a file of ops,
one JSON object a line, and commits them as one batch. No file format
changed.

```rust
let mut batch = db.batch();
batch.update_many(&tasks, Filter::new().lt("due", now).eq("done", false), &Update::new().set("overdue", true))?;
batch.insert(&log, entry)?;
let written = batch.commit()?;
println!("{} marked", written.counts()[0]);
```

```sh
trunkdb batch tasks.trunkdb - --param now='date("2026-11-06T12:00:00Z")' <<'EOF'
{"op": "update", "collection": "tasks", "filter": "due < :now and done = false", "update": {"$set": {"overdue": true}}}
{"op": "insert", "collection": "log", "document": {"what": "marked overdue tasks"}}
EOF
```

## 168.1 `update_many` and `delete_many` in a batch
- **`Batch::update_many(&collection, filter, &update)`:** the update's
  operators made to every document the filter finds at `commit`.
  **`Batch::delete_many(&collection, filter)`:** those documents
  deleted. Both take any collection handle of the batch's database,
  typed or not.
- **Found at their place:** at `commit`, after the ops added before
  them, on the catalog and pages as those leave them, under the
  writer's lock, as a precondition is checked (§162.1). An insert
  before an `update_many` is changed by it; one after isn't. Nothing
  can write between finding the documents and changing them.
- **As the `Collection` methods find them:** `update_fields` with a
  filter (§68) and `delete_many` (§37) share their code with these now,
  `update_matching_in` and `delete_matching_in`, so the sort, skip and
  limit of the filter count, "the oldest ten"; a document the operators
  leave as it was, judged by its encoding, isn't written or counted;
  reading goes through an index where one bounds the filter.
- **Errors:** an update refused as written (a path naming `_id`, an
  `inc` by a string) is refused when added, leaving the batch as it
  was. At `commit`, an operator that can't be made to a document found
  fails the whole batch with `Error::Update`, as does, on a typed
  collection, a changed document that no longer converts to `T`, or a
  unique index refusing what an update makes. Finding nothing is no
  error: a `require` before the op says that something must be there.
- **A missing collection** has nothing to change or delete, and isn't
  made by trying.
- **Operators, not a closure:** `Collection::update_many` takes a
  closure, `update_fields` operators. In a batch, `update_many` takes
  operators. A batch holds its ops until `commit`, so a closure would
  have to be `Send + 'static`, and it would run with every other write
  waiting for it (§136.7). The command and a C ABI have no closures to
  give, and the operators cover what a script needs. The name says what
  the op does, a write to many documents, as `delete_many` does; the
  operators are in its type.

## 168.2 What each op wrote
`Batch::commit` returns a `Written`, where it returned `()`:
`counts()` has an entry per op, in the order added: 1 for an insert,
an update or a delete by id; for `update_fields` 1, or 0 where the
operators changed nothing; the documents changed or deleted for
`update_many` and `delete_many`; 0 for a precondition. `total()` sums
them. `Written` is `#[non_exhaustive]`, and its counts are reached by a
method, so it can say more later.

A program needs this as `Collection::update_fields` gives its count:
how many tasks were marked, whether the job queue had one to take.

This is the one change a program may have to adapt to. `batch.commit()?;`
and `.unwrap()` compile as before. A function that returns
`batch.commit()` as a `Result<()>`, or a `match` on `Ok(())`, doesn't:
`.map(drop)`, or `Ok(_)`, makes it compile again. A second method,
`commit_counting`, would have kept every program compiling, at the
price of two ways to commit for as long as trunkdb lasts. Before 1.0,
the one method that says what it did is the better API, and the
changelog says how to adapt.

## 168.3 `trunkdb batch`
`trunkdb batch <file> <ops.jsonl | -> [--create] [--dry-run]` with any
number of `--param`. A line is one JSON object, read as `trunkdb
insert` reads a document, so `{"$date": …}` and `{"$id": …}` are values:

| `op` | takes | is |
|---|---|---|
| `insert` | `document`, an object | `Batch::insert` |
| `update` | `filter`, `update` as `trunkdb update` takes it (§156.2), `all` | `Batch::update_many` |
| `delete` | `filter`, `all` | `Batch::delete_many` |
| `require` | `filter` | `Batch::require` (§162) |
| `require_none` | `filter` | `Batch::require_none` |

Every line has `collection`. A filter is the text `trunkdb find`
reads, `""` for none, its `:name` parameters bound to the `--param`
values (§163), one set for the whole file; a value no filter uses is a
mistake, as for the other commands. Blank lines are passed over. A
field a line's op doesn't take is a mistake, so a typo doesn't quietly
do nothing: `"filtr"`, or `"upsert": true`, which isn't an op of a
batch.

- **All of it read first:** every line is read, its filter bound and its
  update checked before the file is opened for writing; then every
  collection named must exist, unless `--create`, which also makes the
  file if it's missing, as for `insert --create` (§156). A mistake is
  printed with its line and exit code 2, nothing written.
- **Every document:** an `update` or a `delete` whose filter is empty
  needs `"all": true`, as the commands need `--all` (§156.3). The
  commands count before they write; a batch can't, as what an op finds
  depends on the ops before it, so the guard is on the text: an empty
  filter, the line where a script forgot to fill one in.
- **Output:** each new id on standard output, a line each, as `trunkdb
  insert` prints them; on standard error, a line per `update` and
  `delete` with what it wrote, `line 2: updated 3 documents`, and one
  for the batch: `committed 4 ops: inserted 1 document, updated 3
  documents and deleted 1 document`. No ids are printed for a batch
  that fails, as none of them is stored.
- **A failure at commit** is exit code 1, with its line:
  `line 3: op 2 of the batch, a precondition on collection "tasks",
  failed: …; nothing written`.

## 168.4 A dry run
`Batch::dry_run(&self)` runs every op as `commit` would, under the
writer's lock on a copy of the catalog, and rolls back whatever comes
of it, through `Database::rehearse`: nothing staged is logged or
written, no subscriber or live query hears of it, and the batch can
commit after it. It returns the `Written` that `commit` would, or the
error it would fail with.

For a program, it is no promise: a write after it can change what
`commit` finds. For `trunkdb batch --dry-run`, it is: the file is
locked while the command has it open (§27), so no other process writes
in between. `--dry-run` prints what it would write, `line 2: would
update 3 documents`, and `4 ops would …; nothing written`, and no ids,
which would name nothing.

A read-only database refuses a dry run with `Error::ReadOnly`, as it
refuses a commit: it takes the writer's lock and stages pages, as a
commit does.

## 168.5 Naming the line that failed
A precondition's error names its op (§162.1), and so its line. Any
other error, a unique index refusing a write, an operator that can't be
made, says what went wrong but not which op: `Error` has no op for
them, and adding one to every variant a batch can fail with would
change what programs match on. The command finds it instead: dry runs
of ever shorter beginnings of the batch, halving the range each time,
find the shortest beginning that fails; its last op is the line. A
batch of 1,000 lines takes about ten dry runs, only when it fails. As
the file is locked, each dry run sees what the commit saw.

## 168.6 Left out
- **An upsert in a batch:** `Batch::upsert_fields`, "insert unless
  there is one". `Collection::upsert` reads the one match under its own
  lock; in a batch it is a `require_none` and an insert, or a
  `require` and an `update_many`, chosen by the program. It came with
  §175.
- **A closure in a batch's `update_many`:** see §168.1. A program
  with a change no operator makes reads, then writes by id, with a
  `require` holding what it read.
- **Replacing a whole document from the command:** the `update`
  command doesn't either (§156.2): an update is operators.
- **A JSON array as the file:** JSON Lines are what `export`, `insert -`
  and `find` read and write, can be written a line at a time by a
  script, and name a mistake by its line.
- **Reading in a batch:** an op that hands out documents, a `find` at
  its place. A batch writes; `require` is the read it needs, and
  `dry_run` says what it would do.
- **A guard on "every document" by count:** see §168.3.

## 168.7 Tests
In `tests/batch_filters.rs`:
- **At their place:** the roadmap's overdue tasks, an insert before the
  `update_many` marked and one after it not, a `delete_many` and a
  `require` after them; the counts of each.
- **A sort and a limit** choose ("the first two queued by time"), and a
  document the operators leave as it was isn't counted.
- **What each kind of op wrote,** an empty batch's counts too.
- **Rolled back whole:** an operator that can't be made, a
  precondition after the filters' ops, a unique index refusing what an
  `update_many` makes.
- **Refused when added,** and a typed document that no longer converts.
- **A missing collection** written to by neither op, and not made.
- **A dry run** gives the counts `commit` gives, writes nothing, to the
  file or an index, tells no subscriber, fails as commit would, and
  leaves the batch to commit; the file checks after a reopen. A
  read-only database refuses it.
- **A model:** 300 random batches of up to six inserts, `update_many`s
  that raise `v`, `update_many`s that move `k` (no write when it stays),
  `delete_many`s by a range and `require`s, with and without an index
  on `k`, against a map the model applies each op to: counts, the dry
  run's counts, the failed op and the state after each batch agree.
- **Four workers** take the next queued job and log it in one batch,
  until a `require` finds none queued: 200 jobs, each taken once, the
  log naming each worker as often as the jobs do.

In `tests/cli.rs`:
- **A file and standard input:** a dry run's lines and nothing written,
  then the commit, its lines and the new id, a `$date` read as a value;
  a precondition failing at line 3 writes nothing and prints no id.
- **The failed line found by dry runs:** a duplicate in a unique index
  at line 14 of 19, an operator on a string at line 2.
- **Mistakes in a line:** not JSON, not an object, no op, an op that
  isn't one, no collection, a document that isn't an object, a filter
  on an insert, no filter, an empty filter without `all`, a field no op
  takes, a filter that doesn't read, no update, an update that isn't
  operators, a precondition with a limit, a parameter not given and one
  given and not used: each exit code 2 with its line, nothing written.
  No lines, a file that isn't there, no source, an option of another
  command.
- **`--create`,** for a missing collection and a missing file.

## 168.8 Mutations
Seventeen hand-made mutations, all caught:
- the `update_many`s run before the other ops (caught by the place
  test, the dry run's, the model and the workers);
- `update_matching_in` counting the documents found, not those changed
  (the sort-and-limit test, the model);
- it writing a document the operators left as it was (the same);
- the typed `accept` not called in a batch's `update_many` (the
  conversion test);
- a batch's `delete_many` with its filter's limit dropped (the
  sort-and-limit test);
- a precondition counted as 1, an insert as 0 (every op's counts, the
  place test, the model, the workers);
- `total` as the number of ops (the place test);
- `update_by_id` saying it wrote a document it left as it was (every
  op's counts);
- `rehearse` committing; `rehearse` rolling back only on an error (the
  dry run's test, the model);
- `commit` and `dry_run` swapped in the command (the CLI's batch test,
  `--create`'s);
- the search for the failed line moving the wrong end (line 14);
- the empty filter's guard dropped, the unused `--param` not checked
  (the mistakes' test);
- ids printed on a dry run (the CLI's batch test);
- the collections not checked without `--create` (`--create`'s test).

## 168.9 Limits
- **The whole batch in memory,** its ops and its staged pages, as for
  any batch; a batch over 32 MiB of pages is logged in several records
  (§91).
- **One lock for all of it:** every other write waits while the batch
  runs, and a filter no index bounds reads the whole collection with
  the lock held, as `update_many` does (§162.3). An index is the cure.
- **A dry run is a promise only while nothing else writes:** in the
  command, as the file is locked; in a program, until the next write.
