# 199. The Python module (`bindings/python/`)

Step 2 of the Python bindings (§197.11): the module itself, as §197
designed it, without live queries (step 3) and without CI (step 4).
`import trunkdb` opens a database, reads and writes its collections,
takes snapshots, runs batches and reports, documents as dicts of
Python's own types and errors as exceptions.

## 199.1 The package
- **`bindings/python/`:** the crate `trunkdb-python` (`publish =
  false`), a member of the workspace, its library the compiled module
  `trunkdb._trunkdb`; `python/trunkdb/` the package around it, its
  `__init__.py` taking the module's names in, the stubs
  `_trunkdb.pyi` and `py.typed`; `pyproject.toml` maturin's.
- **PyO3 0.30, `abi3-py311`:** one wheel for Python 3.11 and every
  version after, as designed (§197.2); 0.30 reads a `datetime`'s
  fields through the stable ABI too.
- **Built with maturin:** `maturin develop` in a virtual
  environment, then `pytest` in `bindings/python`. The crate has no
  Rust tests: a test binary would need Python's library linked in;
  its tests are Python's, against the module as Python loads it.
- **The version is trunkdb's,** `trunkdb.version()` and
  `trunkdb.__version__` alike, read from the library.

## 199.2 What it has
§197.3's table, built as written, with these settled in the building:

- **`View`,** a snapshot's collection, as Rust's `View` is: the reads
  of `Collection` and none of its writes, so a write to a snapshot is
  an `AttributeError` an editor shows before it runs, not an error at
  run time.
- **`Upserted`,** what `upsert` returns: `id` and `inserted`, as C#'s
  record.
- **An id given to `get`, `update` or `delete`** is a `DocId` or its
  text: an id from a URL or a form needs no `DocId(…)` around it. A
  document's values are still never read from text (197.4).
- **`insert_many`** takes any iterable of dicts and inserts them in
  one batch, all or none, their ids in order; an empty one writes
  nothing.
- **`batch`** takes a list of op dicts, each one line, numbered from 1
  as their place in the list; or the JSON lines as a str. Its report's
  `id`s are `DocId`s (§198.4).
- **`closed`** on a database, a snapshot and a cursor, as a file has
  it; `name` on a collection and a view.
- **Left for step 3:** `watch`, `watch_aggregate`, `subscribe` and the
  `watch_every` option of `open`.

## 199.3 Documents
As §197.4, both ways. The path a mistake names starts at the document,
`tags[1].price`; an item of `insert_many` or of a list `find` returns
by its place, `[3].due`; a parameter's value by its name, `:since`; a
batch's op by `ops[2]`. A `datetime` is taken to UTC by its own
`utcoffset()`, without `astimezone`, and back as an aware one in
`timezone.utc`. A kind of value trunkdb gains after these bindings is a
`ValueError` naming it, never dropped.

A dict or list that holds itself, `doc["self"] = doc`, is a
`ValueError` at the path where it comes round again; one nested deeper
than a document may (`Document::MAX_NESTING`, 64 levels, the document
the first) is one where it goes past. Each container is checked as it
is entered, by the containers the value is inside, so neither recurses
until the stack ends and takes the interpreter with it; the same value
twice, side by side, is no loop. Found in review: before, both crashed
Python.

## 199.4 Errors
As §197.5, and:

- **`kind` on every class,** `"duplicate_id"`, `"snapshot_open"`, as
  the C ABI's details name them; the kinds without a class of their
  own (`poisoned`, `snapshot_open`, `hint`, `import`, `other`) are
  `trunkdb.Error`, a `hint`'s with `collection` and `index`, an
  `import`'s with `line`.
- **`ParseError`** has `what` (`"filter"`, `"aggregate"`, `"update"`,
  `"params"`, `"batch"`), `text`, `offset`, `line` and `parameter`,
  each `None` where it doesn't apply.
- **A batch's error names its line:** a line that doesn't read, an
  op refused as it is added, and a commit's failure, a
  `PreconditionFailedError` among them, all get `line`.
- **The file in use** is `BlockingIOError` with `errno.EAGAIN` and the
  file's name, as Python's own lock calls report it; an I/O error with
  an `errno` the `OSError` subclass Python picks for it, the path as
  `filename` where the call had one. What the library refuses as asked
  (`InvalidInput`) is `ValueError`.

## 199.5 Closing
- **A database keeps what it closes:** each snapshot and cursor made
  from it is kept, weakly, and `close` closes them before it lets go
  of the database, outside the GIL, as the last handle writes what its
  log holds. A call under way has its own handle, so the file closes
  when it returns (197.6).
- **A snapshot or cursor made as the database closes is closed too.**
  It is made outside the GIL, and a `close` on another thread may come
  between its making and its keeping. It is kept only while, its list
  locked, the database is still open; otherwise it is closed at once and
  the call is `ValueError("closed database")`. A `close` takes the
  database before the list, so each one is either in the list it closes
  or sees it closed. Found in review: before, one could be kept after
  the close and hold the file, so that it couldn't be opened again.
- **A collection, a snapshot and a cursor keep their database alive,**
  as a file's methods do: `trunkdb.open(p).collection("t")` is a
  collection that works, not one of a database already freed.
- **A cursor closes itself at its end:** `for doc in tasks.cursor()`,
  read through, warns of nothing. A `next` after the end ends again,
  as any iterator does; after `close`, it is `ValueError("closed
  cursor")`. A cursor left by a `break` or an exception warns, as an
  open file would; `with tasks.cursor() as c` doesn't.
- **A warning puts the pending exception aside.** Python frees an
  object as an exception passes, the cursor of the `for` it leaves
  among them; warning then, with the exception still set, put the
  warning in its place, and the interpreter crashed reading it. Found
  by a smoke test; the warning now takes the exception aside and puts
  it back, and a test runs that program in a process of its own.

## 199.6 Threads and the GIL
As §197.7: every call converts its arguments with the GIL, runs
without it (`Python::detach`), and converts the result with it again.
The classes are `frozen`, their state behind Rust's locks, so a
`Database` and anything made from it is used from any thread.

## 199.7 Checked
- **pytest, 64 tests,** on Python 3.11 and 3.14, warnings errors
  (`filterwarnings = ["error"]`), so a database, snapshot or cursor a
  test leaves open fails it.
- **The stubs against the module:** mypy's `stubtest` finds nothing;
  `mypy --strict` on the package, and on the tests, none.
- `cargo clippy --workspace --all-features` with the crate in it, and
  `cargo +1.89 build -p trunkdb-python`.

## 199.8 Tests
- `test_documents.py`: every type back as it went in, a `bool` never
  an `int`, field order, tuples and bytes-likes, an `int` past `i64`,
  a type trunkdb doesn't store and a key that isn't a `str` by path, a
  naive `datetime` refused, zones east and west across midnight,
  `datetime` order in a filter, a time with a zone refused,
  nanoseconds cut, a year past 9999 refused by path, `DocId` both ways,
  hashed, pickled and in time order, a given `_id` kept; a dict and a
  list that hold themselves, the same value twice; 64 levels kept, 65
  and 100,000 refused.
- `test_collection.py`: every read and write of §197.3 with its
  parameters, a value no text uses, a parameter without a value, a
  filter's offset, a cursor's end and its snapshot, an aggregate's
  texts each with their own values, a `having` with one, `insert_many` all or none, the
  error classes with their attributes.
- `test_database.py`: options, a read-only open, a second writer, a
  file that isn't trunkdb's, a missing one; snapshots and `compact`
  beside one; batches as dicts and as lines, a dry run, a failed
  precondition's and an unread line's `line`; reports, export and
  import; a closed database through every kind of call; a close
  during a call; a close while another thread takes snapshots and
  cursors, twenty times, the file opened after; the three warnings; the exception through a
  cursor's `for`, in a process of its own; four threads writing; and
  another thread running inside one long read, away from its ends.

## 199.9 Mutations
Each alone, the module rebuilt and the tests run, the file restored:

| Mutation | Test |
|---|---|
| a `bool` taken as an `int` | caught: `update_many`'s, `flag = true` matching none |
| a zone's offset added, not taken off | caught: a datetime in any zone back in UTC |
| a naive `datetime` taken as UTC | caught: a parameter's value checked |
| a path written inside out | caught: the overflow's path, `counts[1]` |
| a time's microseconds kept as nanoseconds | caught: every type back as it went in |
| a batch's ids left as text | caught: a batch of op dicts |
| a warning over a pending exception | caught: the exception through a cursor's `for`, the process crashing |
| a cursor's end not kept | caught: a cursor's end |
| `next` after the end a closed error | caught: the same |
| a database's close leaving its snapshots and cursors | caught: a closed database through every call |
| a batch line's parse error without its line | caught: an unread line's `line` |
| a failed commit without its line | caught: a failed precondition's `line` |
| an aggregate's filter bound to every value | passed at first, the test's values all the filter's; caught once the aggregate's `having` had one of its own |
| a read holding the GIL | passed at first, the other thread running between five calls; caught once its times were looked for inside one call, away from its ends |
| a duplicate's `existing` its own id | caught: a unique index's error |
| a container that holds itself not looked for | caught: a dict and a list that hold themselves |
| no limit to nesting | caught: 100,000 levels |
| the limit one level late | caught: 65 levels |
| a snapshot or cursor kept after the close | caught: a close while another thread takes them, the file still held |

## 199.10 Rejected
- **One `Collection` class for snapshots too,** its writes raising
  there: a snapshot's collection that offers `insert` is a mistake
  found at run time.
- **`astimezone(timezone.utc)` to take a `datetime` to UTC:** a call
  into Python per value, and a `datetime` of year 1 east of UTC
  overflows Python's range on the way; the offset is subtracted in
  Rust, where it fits.
- **`Upserted` as a tuple:** `result.inserted` reads better than
  `result[1]`, and a tuple can't gain a field.
- **A cursor that warns after its end:** reading one through is how a
  cursor is used; only one left part-read holds what it reads.
