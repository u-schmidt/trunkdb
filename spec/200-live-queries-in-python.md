# 200. Live queries in Python (`bindings/python/`)

Step 3 of the Python bindings (§197.11): the three feeds of §197.8, read
by `next`, `for` and `async for`, and their events. CI is step 4.

```python
with vans.watch("zone = :z", {"z": "depot"}) as in_depot:
    for event in in_depot:                     # waits, the GIL let go
        match event:
            case trunkdb.Entered(doc=doc):
                print(doc["name"], "arrived")
            case trunkdb.Left(id=id):
                print(id, "left")
```

## 200.1 The feeds
- **Three classes, as C#'s and Rust's:** `LiveQuery` from
  `Collection.watch(filter="", params=None)`, `LiveAggregate` from
  `Collection.watch_aggregate(filter, aggregate, params=None)`, and
  `Changes` from `Database.subscribe()`; one core in Rust under them.
  The texts are bound as `find`'s and `aggregate`'s are (§199.2), a
  filter that can't be live a `NotLiveError` with its `reason`.
- **`next(timeout=None)`:** the event, or `None` if none came within
  `timeout` seconds, or at the end; `ended` says which. `0` doesn't
  wait; `None` waits as long as it takes. A negative or NaN timeout is a
  `ValueError`.
- **`for event in feed`** waits for each and stops at the end.
- **`async for event in feed`** (200.3).
- **`close()`, or leaving the `with`,** ends the feed at once: what it
  held and wasn't read is let go, `next` gives `None`, and `ended` and
  `closed` are true. A `for` waiting on another thread stops.
- **Closing the database ends its feeds once what was sent before is
  read,** as the library does when its last handle goes (§138): a feed
  holds no handle of its own, so `db.close()` doesn't wait for it, and
  `next` hands out what was sent, then `None`, `ended` true. A feed
  isn't one of the things a database closes (§199.5): those hold the
  file, and closing them would drop what was sent.
- **A feed left open doesn't warn.** It holds no file; freed, it ends,
  as a dropped `LiveQuery` does in Rust. A `for` left by `break` frees
  it, and a warning there would be noise.
- **`watch_every`,** an option of `open` in seconds, for a read-only
  open beside a writer in another process (§195): more than 0, at most
  a day.

## 200.2 Waiting
Every wait is in slices of 100 ms, each without the GIL and with the
feed held for that slice only. Between two:

- **Python looks for a signal** (`check_signals`), so Ctrl-C ends a
  `next()` that waits as long as it takes, as it ends `time.sleep`
  (§197.7).
- **A `close` on another thread gets in,** and the next slice ends the
  wait.

## 200.3 `async for`
Each `__anext__` hands one wait to a thread the loop's iterator starts
for itself, and awaits a future that thread resolves with
`call_soon_threadsafe`. The thread reads one event per wait, **never
one ahead**: a loop left by `break` leaves nothing read behind, so the
next `next` or loop goes on where it stopped. §194.5's C# reader ran
ahead into a channel, and had to give back what it read when the loop
was left; here there is nothing to give back.

- **A wait cancelled after its event came keeps it:** the event is put
  back in front of the feed, handed out first by the next read. The
  thread hands an event over while it still holds the feed, so no read
  after it overtakes it; a cancel marks the wait abandoned, and an event
  that comes for an abandoned wait goes back too.
- **`ended` counts what is kept:** true only once the source ended and
  nothing is kept, so a feed whose last event came back after a cancel
  hasn't ended (the lesson of §194.5's review, kept from the start).
- **The thread ends with the iterator:** it holds the queue of waits,
  not the iterator, so when the loop lets go of it, its `__del__` ends
  the thread.
- **No async runtime:** the loop's own futures, and a thread per loop,
  not per wait (§197.12).

## 200.4 The events
Frozen dataclasses with `slots`, in `trunkdb/_live.py`, written in
Python as §197.2 planned, made by the Rust core from the library's
events:

| Feed | Events |
|---|---|
| `LiveQuery` | `Entered(id, doc, at)`, `Left(id, doc, from_)`, `Changed(id, doc, from_, at)` |
| `LiveAggregate` | `RowEntered(group, row)`, `RowLeft(group, row)`, `RowChanged(group, row)` |
| `Changes` | `Commit(changes)`, each `Change(kind, collection, id)` |

- **The places** `at` and `from_` are `None` without an `order by`.
  `Left`'s `doc` is `None` for a document deleted; `RowLeft`'s `row`
  is `None` where no document of its group is left.
- **`Commit.changes` is a tuple;** `Change.kind` a `ChangeKind`, a
  `StrEnum`, so `change.kind == "inserted"` holds; `id` is `None` for a
  collection dropped.
- **`LiveEvent` and `RowEvent`** name the three of each, for a type
  hint.
- **A kind the library gains after these bindings** is a `ValueError`
  naming it, as for a document's value (§199.3).
- **An error ending a feed** is raised by the read that meets it, as its
  exception (§199.4); the feed has ended after it.

## 200.5 Checked
- **pytest, 85 tests,** 21 of them live, on Python 3.11 and 3.14,
  three runs each.
- **The stubs against the module:** `stubtest` finds nothing; `mypy
  --strict` on the package, and `mypy` on the tests, none. The wait
  class of `async for` is internal: it is not one of the module's
  names, and the stubs don't name it.
- The workspace's clippy and the oldest Rust (1.89) with the crate.

## 200.6 Tests
`test_live.py`: a live query's `Entered`, `Changed`, `Left` with and
without its document, matched with `match`; a sorted one's places, a
`Changed` moving among them; a limit without a sort and a stray value
refused; a live aggregate's rows with a `having` and its parameter; a
subscription's four kinds and a batch's two collections; frozen events;
`next`'s timeout and its refusals; a writer on another thread committing
while a `next()` waits; Ctrl-C ending a wait; a `for` ended by a close
on another thread; a closed database's three feeds read to their end; a
feed freed without a warning; `async for`, a `break` and a second loop
going on where the first stopped; a wait cancelled after its event came,
the source ending meanwhile, the event kept and `ended` false until it
is read; a kept event let go by a close; an event that comes for a wait
already cancelled, kept; `async for` stopping at the end; the loop running while it
waits; a read-only open beside a writer in another process, at 20 ms
and at 30 s; `watch_every`'s refusals.

## 200.7 Mutations
Each alone, the module rebuilt and the live and database tests run, the
file restored; a test that hangs counts as caught, its process group
killed at 90 s:

| Mutation | Test |
|---|---|
| a wait holding the GIL | caught, hung: a writer on another thread committing while `next()` waits |
| no look for a signal between slices | caught, hung: Ctrl-C ending a wait |
| a timeout never reached | caught, hung: `next`'s timeout |
| a cancelled wait's event dropped | caught: a wait cancelled after its event came |
| an event that comes for a cancelled wait dropped | survived at first, nothing cancelling a wait before its event; caught once a test cancels it and then writes, inside the reader's slice; caught three runs of three |
| `ended` not counting what is kept | caught: a wait cancelled after its event came, the source ended meanwhile |
| what is kept read from the back | survived: one event is kept at a time unless two `async for` loops over one feed are cancelled together, and their order between them is no one's |
| an update reported as a delete | caught: a subscription's four kinds |
| `Changed`'s places swapped | caught: a sorted one's `Changed` |
| a close keeping what was kept | survived at first; caught once a kept event is closed away |
| a close leaving the source open | caught, hung: a `for` ended by a close on another thread |
| `watch_every` not passed on | caught: the reader at 30 s seeing the commit |
| a cancelled wait not abandoned | caught: a wait cancelled after its event came |

## 200.8 Rejected
- **One `Feed` class for the three:** its `next` would return any event,
  and `match` on a live query's would list kinds it never hands out.
- **A reader that runs ahead,** as C#'s: the events it read when the
  loop is left have to go back, in order, under a lock the loop shares;
  reading one per wait has nothing to give back, at the cost of a
  hand-over per event.
- **`loop.run_in_executor` per wait:** a thread of the shared pool
  blocked for every waiting feed (§194.6's reasoning for C#).
- **A database's `close` closing its feeds:** it would drop what was
  sent before the close, which §197.8 keeps.
- **A `ResourceWarning` for a feed left open** (200.1).
