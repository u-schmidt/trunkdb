# 197. Python bindings, designed first (`bindings/python/`)

**Designed 2026-11-14; step 1 built the same day (§198), step 2 on 2026-11-15 (§199).** Four choices were made
before it was written: design first, as §177 was for the C ABI and
§185 for readers in other processes; documents as dicts of Python's own
types; live queries read by iteration and by `asyncio`; wheels built
and tested in CI, no PyPI. Its four questions were answered the same
day, each as proposed (197.14).

ROADMAP.md has planned PyO3 bindings since §177: "for Python apps:
`pip install` from wheels, documents as dicts, errors as exceptions, on
the embedding API the C ABI uses". Python is where scripts, notebooks,
small web services and agents live; an embedded store with live queries
is rare there. SQLite is in the standard library; TinyDB and shelve
are the plain alternatives, neither with indexes worth the name, a
query language, or changes as they happen.

## 197.1 On the library, not the C ABI
The module is Rust, built with PyO3 against the `trunkdb` crate, as
`trunkdb-ffi` is. It does not load `libtrunkdb` through `ctypes`:

- **Documents cross as Python objects,** converted in Rust from
  `Document` to `dict`, `datetime`, `bytes` and the rest, and back. The
  C ABI's JSON with its tags (`$date`, `$id`, §179) would be written by
  Rust, read by Python's `json`, and walked again to turn tags into
  types: three passes for one.
- **The GIL is let go where trunkdb waits** (197.7): around every read
  and write, and around a live query's wait. Through `ctypes` the GIL
  is let go for each foreign call too, but the conversions on both sides
  would run in Python.
- **A wheel carries everything,** the library compiled in: nothing to
  put beside the program, as the C# wrapper needs (§180.4).
- **The C ABI is unchanged,** and stays what C and C# use.

## 197.2 The package
- **`bindings/python/`,** in this repository, as the C# wrapper is
  (§177.9): a function changes in the library and both bindings in one
  commit, tested by one CI run. A crate `trunkdb-python`, `publish =
  false`, a member of the workspace.
- **Built with maturin,** a mixed project: the compiled module
  `trunkdb._trunkdb`, and a small Python package `trunkdb` around it for
  what is better written in Python (the `asyncio` side of live queries,
  197.8, and the event classes). The import is `import trunkdb`.
- **Type stubs** (`_trunkdb.pyi`, `py.typed`), so an editor and mypy
  know every signature; docstrings from the Rust.
- **PyO3 0.30,** which needs Rust 1.85, under trunkdb's 1.89.
- **Python 3.11 and later, one wheel per system:** PyO3's `abi3`
  builds against the stable ABI of 3.11, and one wheel serves every
  version after. 3.11 is the oldest still maintained, 3.10's support
  having ended in October 2026.
- **The package's version is trunkdb's,** 0.41.0 the first.

## 197.3 The API, by example
```python
import datetime as dt
import trunkdb

with trunkdb.open("app.trunkdb") as db:
    tasks = db.collection("tasks")
    tasks.ensure_index("status")

    task_id = tasks.insert({"title": "Write it", "status": "open",
                            "due": dt.date(2026, 11, 20)})
    for task in tasks.find("status = :s order by due limit 10", {"s": "open"}):
        print(task["_id"], task["title"])

    tasks.update_many('status = "open" and due < :today',
                      {"$set": {"status": "late"}}, {"today": dt.date.today()})
    rows = tasks.aggregate("", "count() as n group by status")

    with db.snapshot() as then:
        for task in then.collection("tasks").cursor():
            ...
```

The names follow the C# wrapper's (§180), in Python's spelling, so the
bindings read alike; the text is trunkdb's own, as the command and the
C ABI take it.

| | Methods |
|---|---|
| module | `open(path, *, read_only=False, cache_size=…, watch_every=…, …)`, `version()`, `DocId`, the errors, the events |
| `Database` | `collection(name)`, `collections()`, `drop_collection(name)`, `snapshot()`, `subscribe()`, `batch(ops, params=None, *, dry_run=False)`, `checkpoint()`, `compact()`, `check()`, `stats()`, `export(path)`, `import_file(path)`, `close()`; a context manager |
| `Collection` | `get(id)`, `find(filter="", params=None)`, `find_one`, `count`, `cursor`, `aggregate(filter, aggregate, params=None)`, `explain`, `insert(doc)`, `insert_many(docs)`, `update(id, doc)` (the whole document), `update_many(filter, update, params=None)` (MongoDB's operators), `delete(id)`, `delete_many(filter, params=None)`, `upsert(filter, update, params=None)`, `watch(filter="", params=None)`, `watch_aggregate(filter, aggregate, params=None)`, `ensure_index(fields, *, unique=False, sparse=False)`, `drop_index(fields)`, `indexes()`, `drop()` |
| `Snapshot` | `collection(name)` with the reads of `Collection`; a context manager |
| `Cursor` | an iterator, and a context manager that closes it |

- **A collection is a name and the database:** `db.collection(name)`
  costs nothing and holds nothing to close.
- **`params` is a dict,** its values converted as a document's are
  (197.4). A value no text uses is refused, as everywhere (§163.4).
- **`update_many` takes the operators as a dict,** `{"$inc": {"n":
  1}}`, read by the library's `Update` (§177.7); `batch` takes a list
  of op dicts, or the JSON lines `trunkdb batch` reads (§168).
- **Reports** (`check`, `stats`, `compact`, `indexes`, `export`) are
  dicts, the shapes the C ABI returns (§179).
- **`import_file`:** `import` is a keyword.

## 197.4 Documents
A document is a `dict` with `str` keys, its fields in their order.
Each value, both ways:

| trunkdb | Python | Into trunkdb also |
|---|---|---|
| `Null` | `None` | |
| `Bool` | `bool` | |
| `Int` | `int` | an `int` outside `i64`: `OverflowError` |
| `Float` | `float` | |
| `String` | `str` | |
| `Binary` | `bytes` | `bytearray`, `memoryview` |
| `Array` | `list` | `tuple` |
| `Object` | `dict` | keys `str` only: `TypeError` |
| `Id` | `trunkdb.DocId` | |
| `DateTime` | `datetime.datetime`, in UTC | one with any `tzinfo`, taken to UTC |
| `Date` | `datetime.date` | |
| `Time` | `datetime.time` | without `tzinfo` |

- **Checked as the C# wrapper's are:** a `bool` is never an `int`
  (Python's is a subclass); anything else, a `Decimal`, a `set`, a
  `UUID`, a dataclass, is a `TypeError` naming its path,
  `tasks[3].due`, never turned into a string unasked.
- **`datetime` keeps microseconds,** trunkdb nanoseconds: the rest is
  cut when read, as .NET's 100 ns cut it (§180.2). A date-time or a
  date outside years 1 to 9999, which trunkdb holds and Python can't,
  is a `ValueError` naming its path when read.
- **A `datetime` without `tzinfo` is refused** (`ValueError`), where
  C# takes an unspecified `DateTime` as UTC: Python's naive datetimes
  are local time as often as UTC, and the mistake would be stored.
  Decided so (197.14).
- **`DocId`:** made from its text, `DocId("0192f0c4-…")`; `str()` gives
  it back; equal, hashed and ordered as trunkdb orders ids, by when they
  were made. `insert` returns one; a document's `_id`, given, is kept
  (§59).

## 197.5 Errors
One base, `trunkdb.Error(Exception)`, and a class for each kind a
program catches, with the details as attributes, as the C# exceptions
have them (§180.3):

| Class | From | Attributes |
|---|---|---|
| `ParseError` | a filter's or an aggregate's text, an update's operators, a batch's line | `text`, `offset`, `line` |
| `DuplicateIdError` | `Error::DuplicateId` | `collection`, `id` |
| `DuplicateValueError` | `Error::DuplicateValue`, a unique index | `collection`, `field`, `id`, `existing` |
| `NotFoundError` | `Error::NotFound` | `collection`, `id` |
| `MultipleMatchesError` | `Error::MultipleMatches`, an upsert's filter | `collection`, `count` |
| `UpdateError` | `Error::Update`, an operator that can't apply (`$inc` on a string) | `collection`, `id`, `message` |
| `PreconditionFailedError` | `Error::PreconditionFailed`, a batch's `require` | `collection`, `op`, `matched`, `line` |
| `NotLiveError` | `Error::NotLive` | `reason` |
| `SnapshotTooOldError` | `Error::SnapshotTooOld` | |
| `ReadOnlyError` | `Error::ReadOnly` | |
| `FormatError` | a file this build can't read | |
| `Error` | the others | `kind` |

An I/O error is Python's own `OSError` with its `errno`, the file in
use by another program the `BlockingIOError` that `WouldBlock` is in
Python. A value that doesn't convert is `TypeError`, `ValueError` or
`OverflowError` (197.4). A closed database or cursor: `ValueError`, as
a closed file's.

## 197.6 Closing
- **`db.close()`, or leaving the `with`,** closes the database once
  every call under way has returned. A call after it raises
  `ValueError("closed database")`, through any collection or snapshot
  made from it.
- **Without either,** it closes when Python frees it, as a file does,
  with a `ResourceWarning`.
- **A snapshot and a cursor** close the same ways; a snapshot also
  ends with its database.

## 197.7 Threads and the GIL
- **One `Database` for the program,** from any thread, as in Rust.
- **The GIL is let go around each call into trunkdb:** the arguments are
  converted with it, the call runs without it, the result is converted
  with it again. Another Python thread runs while one waits for a disk
  read, a commit's flush or a live query.
- **Ctrl-C works in a wait:** a live query's `next` without a timeout
  waits in slices of 100 ms, and between two it lets Python look for
  a signal, so `KeyboardInterrupt` comes as it does from `time.sleep`.
- **Free-threaded Python (3.13t, 3.14t)** is left for later: `abi3`
  doesn't cover it, and it needs a wheel of its own. Its module would
  declare itself safe without the GIL; nothing in it needs the GIL for
  more than converting.

## 197.8 Changes as they happen
```python
with vans.watch('zone = :z', {"z": "depot"}) as in_depot:
    for event in in_depot:                     # waits, the GIL let go
        match event:
            case trunkdb.Entered(id=id, doc=doc):
                print(doc["name"], "arrived")
            case trunkdb.Left(id=id):
                print(id, "left")

async for event in in_depot:                   # in asyncio
    ...
```

- **Three feeds,** as in C# (§194.5): `collection.watch(...)`,
  `collection.watch_aggregate(...)` and `db.subscribe()`.
- **Read three ways:** `next(timeout=None)`, the event or `None`, and
  `ended` saying whether that `None` is the end; `for`, which waits for
  each and stops at the end; `async for`.
- **Events are frozen dataclasses,** matched with `match` as Rust's
  `match` matches: `Entered(id, doc, at)`, `Left(id, doc, from_)`,
  `Changed(id, doc, from_, at)`; `RowEntered(group, row)`,
  `RowLeft(group, row)`, `RowChanged(group, row)`;
  `Commit(changes)`, each `Change(kind, collection, id)`. `from_`, as
  `from` is a keyword (PEP 8's way).
- **`async for`** reads on a thread of its own for the loop's life,
  handing each event to the loop with `call_soon_threadsafe`, as C#'s
  `ReadAllAsync` feeds a channel (§194.5). What it read and the loop
  didn't take, when the loop is left, is kept for the next read: the
  lesson of §194.5 and its review, from the start.
- **Closing** the feed, or its database, ends it, once what was sent
  before is read.
- **A read-only open beside a writer** gets that writer's commits
  (§195), `watch_every` an option of `open`.

## 197.9 Built and tested
- **Locally:** `maturin develop` in a virtual environment, then
  `pytest bindings/python/tests`.
- **CI:** on Linux, macOS and Windows, a wheel built with maturin, put
  into a fresh virtual environment, and the tests run against it; on
  the oldest Python (3.11) and the newest. The wheels are kept as the
  run's artifacts.
- **No PyPI upload.** Whether trunkdb goes on PyPI is decided with
  crates.io (ROADMAP.md, Open); until then a wheel comes from a CI run,
  or from `pip install` of the repository, which needs Rust.

## 197.10 What moves into the library first
§179 kept the reports' JSON (`check`, `stats`, `compact`, an export's
or import's summary, the indexes, a batch's ops) in `trunkdb-ffi`,
"until a second user comes". This is the second: they move into the
library as a `to_document` on each report, the C ABI writing those
documents as its JSON, unchanged, and the module turning them into
dicts. The parameters' binding that refuses a value no text uses
(`text::unused` in `trunkdb-ffi`) moves too. No behaviour changes; the
C ABI's tests and the C program hold it.

## 197.11 Steps
Each its own section, tested before the next:

1. **197.10:** the reports and the binding into the library.
2. **The module:** documents both ways, `DocId`, the errors,
   `Database`, `Collection`, `Snapshot`, `Cursor`; the tests.
3. **Live queries:** the three feeds, `next`, `for`, `async for`, the
   events; the tests.
4. **CI:** wheels on the three systems, the tests against each; the
   README, GUIDE and the agent skill.

## 197.12 Rejected
- **`ctypes` or `cffi` over the C ABI:** no compiler for whoever builds
  the package, but JSON both ways with its tags turned into types in
  Python, the conversions under the GIL, and the C library put beside
  the program by hand (197.1).
- **A `Document` class** instead of `dict`: every program turns it into
  a dict anyway, and every library it hands documents to takes dicts.
- **Models: dataclasses, Pydantic, TypedDicts** read and written as C#
  has `T`: when asked. A dict goes into either with `Model(**doc)`.
- **An `asyncio`-native library,** each call a coroutine: trunkdb's calls
  are short, and `asyncio.to_thread` serves the slow ones; an async
  runtime in Rust (`pyo3-async-runtimes`, on tokio) is a second
  scheduler for one feature. Live queries get `async for` (197.8).
- **Callbacks for live queries,** a Rust thread calling Python: it would
  take the GIL from trunkdb's own thread, under the writer's lock
  (§140.4, §177.11).
- **pymongo's names,** `insert_one`, `replace_one`, `count_documents`:
  trunkdb isn't MongoDB, and its C# and Rust names are what the docs
  use.
- **A repository of its own** from the start: as §177.9 for C#.
- **A wheel per Python version:** `abi3` makes one serve them all, at
  the cost of a few PyO3 functions only the full API has; none of them
  is needed here.

## 197.13 Limits
- **Nanoseconds cut to microseconds,** and dates outside 1–9999
  unreadable as Python's types (197.4).
- **`find` builds a list;** `cursor` hands them out one at a time, as
  in Rust.
- **Conversions hold the GIL:** a `find` of 100,000 documents lets
  other threads run while trunkdb reads, not while the dicts are made.
- **No free-threaded wheel yet** (197.7).

## 197.14 Decided
Four questions, answered on 2026-11-14, each as this section had it:

1. **A `datetime` without `tzinfo` is refused,** not taken as UTC
   (197.4).
2. **C#'s names:** `update` the whole document, `update_many` with
   operators (197.3).
3. **`from_`** for an event's old place (197.8).
4. **Live queries after the module,** a step of their own (197.11).
