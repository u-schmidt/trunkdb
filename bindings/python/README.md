# trunkdb for Python

trunkdb as a Python module, built with PyO3 on the library itself:
documents are dicts of Python's own types, errors are exceptions, and
live queries are read with `for` and `async for`. Python 3.11 and later,
one wheel per system. The design is in [SPEC §197](../../spec/197-python-bindings-designed-first.md),
what was built in [§199](../../spec/199-the-python-module.md),
[§200](../../spec/200-live-queries-in-python.md) and
[§201](../../spec/201-python-wheels-in-ci.md).

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

## Installing

Not on PyPI. Either:

- **A wheel from CI:** every CI run keeps one per system as an artifact,
  `trunkdb-wheel-<system>`; `pip install` the `.whl` inside. One wheel
  serves Python 3.11 and every version after.
- **From the repository,** which needs Rust 1.89 or later:
  ```bash
  pip install "git+https://github.com/u-schmidt/trunkdb#subdirectory=bindings/python"
  ```

## Documents

- **A document is a dict** of `str`, `int`, `float`, `bool`, `None`,
  `bytes`, lists, dicts, `datetime` (with a `tzinfo`; read back in UTC),
  `date`, `time` and `DocId`. A `datetime` without `tzinfo` is refused,
  not guessed. A tuple goes in as a list.
- **`_id`** is a `DocId`; leave it out and trunkdb chooses one. Where an
  id is asked for, a `DocId` or its text will do.
- **Filters and aggregates** are trunkdb's text, as the `trunkdb`
  command takes it; parameters a dict, `{"s": "open"}`, its values
  converted as a document's are.
- **`update(id, doc)`** replaces the document whole;
  **`update_many(filter, update)`** takes MongoDB's operators,
  `{"$inc": {"hits": 1}}`.
- **Reports** (`check()`, `stats()`, `compact()`, `indexes()`,
  `export(path)`) are dicts.

## Errors

Every error of trunkdb's own is a `trunkdb.Error`, its `kind` naming
which; catch `DuplicateValueError`, `ParseError`, `NotFoundError` and
the others by class, their details as attributes:

```python
try:
    users.insert({"email": "ada@example.com"})
except trunkdb.DuplicateValueError as e:
    print(e.field, "already used by", e.existing)
```

## Closing and threads

- **`with`** closes a database, a snapshot and a cursor; so does
  `close()`. Closing a database closes the snapshots and cursors made
  from it. One left open warns with a `ResourceWarning`.
- **A collection** is a name and the database: `db.collection(name)` is
  as cheap as that, with nothing to close.
- **One database for the whole program,** from any thread. The GIL is
  let go while trunkdb reads, writes and waits.

## Changes as they happen

A filter kept live tells you what comes to match it, what leaves, and
what is written and still matches, as each commit commits:

```python
with vans.watch("zone = :z", {"z": "depot"}) as in_depot:
    for event in in_depot:
        match event:
            case trunkdb.Entered(doc=doc):
                print(doc["name"], "arrived")
            case trunkdb.Left(id=id):
                print(id, "left")
            case trunkdb.Changed(doc=doc):
                print(doc["name"], "moved within")
```

- **First what matches now,** as `Entered`, then the changes. With a
  sort, `watch("order by speed desc limit 10")` is a list: each event's
  `from_` and `at` say where to take a document out and put it in.
- **`watch_aggregate(filter, aggregate)`** keeps an aggregate live, its
  rows `RowEntered`, `RowLeft` or `RowChanged`; **`db.subscribe()`**
  hears of every commit, as a `Commit` with each `Change`.
- **Read** with `next(timeout)`, `for`, or `async for` in asyncio. A
  `break`, or a task cancelled while it waits, loses no event: the next
  read goes on where the last stopped. Ctrl-C ends a wait.
- **Close** the feed, or leave its `with`, to stop it. Closing the
  database ends it too, once the events before are read: `ended` is
  then true.
- **Opened read-only beside the application** (`read_only=True`, while
  another process writes), a feed hears of the writer's commits by
  looking every `watch_every` seconds, a tenth of a second unless set.
- **Against the clock,** `watch("last_seen < now - 10m")` is read again
  as time passes, with nothing written: a device that stops reporting
  is `Entered` ten minutes after its last reading, `Left` when it
  reports again. An index on `last_seen` finds the moment; without one,
  it is read again every second.

## Testing it

In a virtual environment, with Rust, [maturin](https://www.maturin.rs)
and pytest, from the repository's root:

```bash
cd bindings/python
maturin develop
pytest
```
