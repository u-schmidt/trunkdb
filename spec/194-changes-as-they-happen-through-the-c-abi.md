# 194. Changes as they happen, through the C ABI and in C# (`trunkdb-ffi/`, `bindings/dotnet/`, `changes.rs`, `live.rs`)

The C ABI (§177, §179) had every read and write but none of §140's:
no `subscribe`, no `watch`, no `watch_aggregate`. A .NET program could
not use them at all, though they are what sets trunkdb apart among
embedded stores. §177.10 planned them as "handles polled with a
timeout", and ROADMAP.md kept them deferred until asked. Asked now.

## 194.1 One handle, polled
Five functions, ABI version 2:

| Function | What it hands out |
|---|---|
| `trunkdb_subscribe(db, &live)` | every commit from now on (§135) |
| `trunkdb_watch(db, collection, filter, params, &live)` | a live query (§138, §145) |
| `trunkdb_watch_aggregate(db, collection, filter, aggregate, params, &live)` | a live aggregate (§139, §174) |
| `trunkdb_live_next(live, timeout_ms, &event, &ended)` | the next event as JSON, or null |
| `trunkdb_live_close(live)` | |

One opaque type, `trunkdb_live`, with its own tag ("trlv", §179), holds
any of the three: a program polls them all the same way. `timeout_ms`
is how long `next` waits: 0 not at all, a negative one until an event
comes or it ends. A null event with `ended` false is "nothing yet";
with `ended` true it is the end, for good, every event before it read.
The error that ends a live query or aggregate (§138.5, §139.5) is
returned once, as a code and the thread's last error, and the call
after it reports the end.

Filters, aggregates and their parameters are read as `trunkdb_find` and
`trunkdb_aggregate` read them: the aggregate's filter and text each take
their own parameters, a value neither uses refused. A filter that can't
be live, an `offset`, or a `limit` without a sort (§145), is
`TRUNKDB_INVALID_ARGUMENT` with its `reason` in the details;
`Error::NotLive` was `TRUNKDB_OTHER` before, as no call could make it.

## 194.2 Events as JSON
Each a JSON object, its kind in `event`, documents as an export writes
them (§179), ids as their text, as `trunkdb_insert` hands one out:

- **A commit:** `{"event": "commit", "changes": [{"change": "inserted",
  "collection": "tasks", "id": "…"}, …]}`; a change `inserted`,
  `updated`, `deleted`, or `dropped`, which has no `id`.
- **A live query:** `{"event": "entered", "id": …, "doc": {…}, "at":
  …}`, `{"event": "left", "id": …, "doc": … or null, "from": …}`,
  `{"event": "changed", "id": …, "doc": {…}, "from": …, "at": …}`.
  `from` and `at` are places in the list where the filter sorts (§145),
  null where it doesn't; always there, so a reader needn't test for
  them.
- **A live aggregate:** `{"event": "entered" | "left" | "changed",
  "group": {…}, "row": {…}}`, `row` null where a `left` row's group has
  no documents left, `group` `{}` without `group by` (§139.3).

## 194.3 Telling an end from nothing yet
`Changes`, `LiveQuery` and `LiveAggregate` had `recv_timeout`, whose
`None` was either: none came in time, or it had ended. The ABI must
say which, and a Rust program waiting with a timeout could not tell
either. Each now has `has_ended()`: true once a `recv`, `try_recv` or
`recv_timeout` returned `None` because the sender was gone, every
report before it read. The three share a small receiver of their own,
`Inbox`, which notes the end as it meets it; `std`'s channel says so in
its error, which `recv_timeout` dropped.

- **Closing the database ends its live handles.** A live handle holds
  no `Database`, as a cursor does (§179): when the last handle on the
  database closes, the writer and its senders go, and each live handle
  reads what was sent before, then its end.
- **Threads:** the handle is behind a `Mutex`, as a cursor is; one
  thread at a time reads it. A thread may wait in `trunkdb_live_next`
  while others write: the writer never waits for it (§140.4). Closing a
  handle while another thread is in a call with it is a use after free
  the library can't catch, as for every handle; the header says so.

## 194.4 The ABI's version
`TRUNKDB_ABI_VERSION` is 2. A function, once out, keeps its signature
(§177.8), so a program built against version 1 runs on this library;
but the C# wrapper calls the new functions, and against a library of
version 1 they wouldn't be there. It refuses a library whose version
differs from its own (§180), so it refuses that one with a message at
`Database.Open` instead of failing at the first `Watch`.

## 194.5 In C#
- `Database.Subscribe()` → `Changes`, of `Commit(IReadOnlyList<Change>)`,
  each `Change(ChangeKind Kind, string Collection, DocId? Id)`.
- `Collection<T>.Watch(filter, parameters)` → `LiveQuery<T>`, of
  `LiveEvent<T>`: `Entered(Id, Document, At)`, `Left(Id, Document?,
  From)`, `Changed(Id, Document, From, At)`, the document a `T`.
- `Collection<T>.WatchAggregate<TRow>(filter, aggregate, parameters)` →
  `LiveAggregate<TRow>`, of `RowEvent<TRow>`: `Entered(Group, Row)`,
  `Left(Group, Row?)`, `Changed(Group, Row)`, `Group` a `JsonObject`.

The events are records nested in an abstract one with a private
constructor, so the three are all there are, and a `switch` on them
reads as Rust's `match` does. The three feeds share `LiveFeed<TEvent>`,
which is `IDisposable` and is read three ways:

- **`Next(timeout)`:** the event, or null; `Ended` says whether that
  null is the end.
- **`ReadAll()`,** for `foreach` on a thread of its own: it waits for
  each event, and ends with the feed, or when another thread disposes
  it.
- **`ReadAllAsync(token)`,** for `await foreach`: it reads on a thread of
  its own, started for the loop, into a channel the loop awaits, and
  throws `OperationCanceledException` when the token is cancelled.

The methods are those of `System.Threading.Channels`' `ChannelReader`.
Each wait in the two loops lasts at most 100 ms, after which the loop
looks whether it was disposed or cancelled; the `SafeHandle` holds the
handle open through a wait, so a dispose during one closes it once the
wait returns.

**What a loop read and didn't hand out is kept.** `ReadAllAsync`'s
reader runs ahead of the loop: a `break` leaves events in the channel
that the handle has handed out. They are put first in a queue of the
feed's own, which every read takes from before it asks the library, so
nothing is lost between two loops. The first version did this inside
the iterator's `finally`, and dropped them: in an async iterator left
by a `break`, the code after a `try`/`finally` nested in that `finally`
(here a `lock`, and a `foreach` over the queue) did not run, and the
test that leaves a loop early failed every time. It runs in a method of
its own now, where the `finally` only calls it.

## 194.6 Rejected
- **A callback the library calls,** as SQLite's `sqlite3_update_hook`:
  it would run under the writer's lock, where a callback that writes
  waits for itself (§140.4), and on trunkdb's thread, which .NET must be
  told about; a delegate passed to C must also be kept alive by hand.
  A handle polled with a timeout is what §177.10 planned.
- **A handle type, and a `next` and `close`, for each kind:** three
  times the functions for one way of reading them.
- **Events as C structs:** a struct per kind, its strings freed one by
  one, where JSON is how every other result comes back (§179), and a
  field added later breaks no layout.
- **`IEnumerable` and `IAsyncEnumerable` on the feed itself,** for
  `foreach (var e in live)`: a type with both makes LINQ's and xUnit's
  calls on it ambiguous, and with .NET 10's `System.Linq.AsyncEnumerable`
  every LINQ call. `ReadAll` and `ReadAllAsync` are explicit, as
  `ChannelReader` is.
- **`ReadAllAsync` as `await Task.Run(() => Next(slice))` a step at a
  time:** a thread-pool thread blocked in every wait, ten times a second
  for every idle feed; a thread of its own for the loop's life blocks
  nothing shared.
- **`TRUNKDB_ABI_VERSION` kept at 1:** a wrapper of this release would
  load a library of the last one, and fail at the first `Watch` with
  `EntryPointNotFoundException`.

## 194.7 Limits
- **No `Condition::custom` from C:** a filter is text (§178), so a
  geofence is a live query for every position, the fences kept by the
  program (§140.6).
- **Events wait in memory without limit** until read, and are lost if
  the process stops (§138.7): the transactional outbox (§140.3) where
  one mustn't be.
- **An `await foreach` holds a thread** for its life, as a Rust program
  does a thread per receiver. A dispose or a cancel is noticed within
  100 ms.
- **A read-only open** reports a live query's first state, and nothing
  after: it commits nothing, as §138.7 says.

## 194.8 Tests
- `tests/changes.rs`, `tests/live.rs`, `tests/live_aggregates.rs`,
  `a_wait_with_a_timeout_tells_an_end_from_nothing_yet`: a wait that
  times out is not an end; what was sent before the end is read first;
  then `has_ended`, by the database dropped and, for a live query, by a
  panic in a `Condition::custom`.
- `trunkdb-ffi/src/tests.rs`: `changes_are_polled_from_a_live_handle`,
  `live_queries_report_what_enters_leaves_and_changes` (a sorted list's
  places, a refused `offset`, texts that don't read),
  `live_aggregates_report_their_rows`, `a_live_handle_ends_with_its_database`,
  `a_live_handle_waits_on_another_thread`,
  `live_handles_refuse_what_isnt_one`.
- `trunkdb-ffi/tests/c/every_function.c`: the five functions from C,
  under AddressSanitizer, and the subscription's end after
  `trunkdb_close`.
- `bindings/dotnet/tests/TrunkDb.Tests/LiveTests.cs`: each feed's
  events as records; a sorted query's places; `Ended` after the
  database is disposed, and `Next` refused after the feed is; an
  `await foreach` that reads as commits come, keeps what it read ahead
  when left, and stops when cancelled; a `foreach` on another thread
  that ends when the feed is disposed.

## 194.9 Mutations
Each alone, its tests run, the file restored:

| Mutation | Test |
|---|---|
| `recv_timeout` notes an end when it times out | caught: the three `a_wait_with_a_timeout…` |
| `try_recv` never notes the end | caught: the same |
| a negative `timeout_ms` doesn't wait | caught: `a_live_handle_waits_on_another_thread` |
| `trunkdb_live_next` never says `ended` | caught: `a_live_handle_ends_with_its_database` |
| `left` without its document | caught: the live query's test |
| `entered`'s place one further on | caught: the sorted list in the live query's test |
| `Error::NotLive` as `TRUNKDB_OTHER` | caught: the refused `offset` |
| a change without its id | caught: `changes_are_polled_from_a_live_handle`, and the C program |
| C#: what `ReadAllAsync` read ahead dropped when the loop is left | caught: `Await_foreach_…` |
| C#: `Next` after `Dispose` not refused | caught: `A_feed_ends_with_its_database…` |
| C#: `Left` without its document | caught: `A_live_query_reports…` |
| C#: `Ended` never set | caught: `A_feed_ends_with_its_database…` |
| C#: a loop that doesn't look whether the feed was disposed | caught: `A_foreach_on_another_thread…` |
| a `timeout_ms` of 0 waiting 1 ms | passes: no test can tell, and no program would |
