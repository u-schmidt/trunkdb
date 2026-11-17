# 207. The last handle closes the file (`database.rs`)

Found by the fuzzer on `main` after §206: a program that drops its
database and opens the file again was refused, `WouldBlock`, "already
open for writing". Its live query compared with the clock, and
`trunkdb-clock` (§204) was reading it again just then.

## 207.1 Why
trunkdb's threads, `trunkdb-clock` and `trunkdb-live` (§195), hold the
database weakly and make it strong for one turn: a tick, or a look.
Where the application dropped its last handle during a turn, the
thread's was the last, and the database closed on that thread when the
turn ended: the application's drop returned with the file still open
and its writer's lock held. An application that closes and opens again,
as the fuzzer's `reopen` does, or a C# `Dispose` followed by an open,
was refused for up to a turn. A turn rereads live queries, so it lasts
as long as their reads.

`trunkdb-live` has had it since §195 (0.41.0): there, in a read-only
open, what stayed open a moment longer was the reader's lock, which no
open is refused for.

## 207.2 The last handle waits for a turn
- **The application's handles counted apart:** every clone of a
  `Database` the application holds shares one count; a thread's turn
  holds the database apart from it.
- **The last dropped:** no turn begins after it, and one under way is
  waited for. The database closes as before, on the application's
  thread, before the drop returns: the WAL written back, the file and
  its locks let go.
- **A turn ends** by letting go of the database, and only then counts as
  ended, so the last handle never closes it under the thread.
- **The drop in a turn of its own:** where a custom condition drops the
  application's last handle, run by a turn, it doesn't wait for that
  turn, which would never end; the database closes when the turn does.
- **A thread asleep** isn't waited for: it holds nothing, and ends at
  its next turn.

## 207.3 Rejected
- **Joining the threads at the last drop:** a thread asleep for a
  second, as `trunkdb-clock` can be, would hold up every drop; and a
  drop in its own turn can't join itself.
- **A retry in the fuzzer:** the drop says the file is closed; an
  application shouldn't have to retry for a thread of trunkdb's (§98's
  child process is the one moment it may).

## 207.4 Tests
`tests/clock_live.rs`:
- the last handle dropped while `trunkdb-clock` is in a turn, held open
  300 ms by a condition of the test's own: the file opens again at once;
- a condition that drops the last handle in a turn: no deadlock, and the
  file opens again once the turn ends.

## 207.5 Mutations
| Mutation | Caught by |
|---|---|
| The last handle waits for no turn | in the middle of a tick (`WouldBlock`, the fuzzer's error) |
| It waits for its own turn too | dropped in a tick (a deadlock) |

`trunkdb-live`'s turns go through the same two calls; no test times a
read-only open's drop against a look.
