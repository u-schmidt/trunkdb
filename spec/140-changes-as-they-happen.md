# 140. Changes as they happen: what §135–§139 share

The time-series workload (§5.1) asks when a position enters or leaves a
geofence, and readings sent every few seconds ask the same of a mean
over a window. Five steps answer it, each useful alone:

| Step | What a program gets |
|---|---|
| §135, a hook after each commit | `Database::subscribe`: the ids each commit inserted, updated or deleted |
| §136, a condition of its own | `Condition::custom`: a closure for what no operator says, a point in a fence among it |
| §137, a panic in `update_many`'s change | the writer's lock kept unpoisoned by the application's closures |
| §138, live queries | `Collection::watch`: the documents that came to match a filter, left it, or changed |
| §139, live aggregates, and a window | `Collection::watch_aggregate`: the rows that came into a result, left it, or changed; `Aggregate::window`, the last minutes |

The ROADMAP planned them together, and these rules held for all of
them. They are written here, once, as the steps that follow such a
subscription would keep them.

## 140.1 Kept in memory, never in the file
A subscription is registered by the program, as an event handler is,
and ends when it is dropped or the program does; the program registers
it again at its start. What reacts to a change is the application's
code, which a file can't hold, so a stored query would have no one
listening after a restart anyway.

## 140.2 The state first, then the changes
A live query or aggregate reports at its registration what matches
now, then only what comes to match and what no longer does, as Realm's
do: a position already in a fence at the start is reported so (§138.3,
§139.3). The state that decides it, the position in its document, is in
the file, so the answer is rebuilt after a restart, not remembered.

## 140.3 Nothing missed while the program is down
The file is locked to one writing process (§21), so with the program
gone nothing changes. The one gap is a crash between a commit and its
report. Where a report must not be lost (a message sent once), the
application writes the event as a document in the same batch as the
change that caused it, and works those off apart: the transactional
outbox, as .NET's MassTransit and NServiceBus have it. Both or neither
are in the file (§135.6).

## 140.4 Through a channel, not a callback
A report handed to a channel is read on the application's own thread,
which may then read and write freely (§135.5). A callback run inside the
commit would run under the writer's lock, and one that writes would
wait for the writer it runs in. Every channel is unbounded: a writer
never waits for a reader, and a reader that falls behind costs memory
instead (§135.6).

A `Condition::custom` is the one closure of the application's that runs
inside trunkdb, under the writer's lock where it checks a write or a
live query. So it mustn't use the database (§136.7), and a panic in it
is caught and ends what ran it alone (§136.2, §138.5, §139.5).

## 140.5 Inside one commit, in this order
After a batch is in the WAL and published, still under the writer's
lock, so in commit order:
1. **live queries** (§138.2): each document the batch changed in a
   watched collection read once from the snapshot just published, and
   checked against each filter there;
2. **live aggregates** (§139.1): each on a collection the batch changed
   run again, whole, on the same snapshot;
3. **the hook's reports** (§135.3): one `Commit` to each subscriber;
4. then a checkpoint, if one is due.

Each sees the state its own commit left, never a later one: a late
reader loses no transition. What the batch changed is recorded in its
own catalog while any of the three is open (§135.4), so a batch with
none records nothing.

## 140.6 Left to the application
- **Hysteresis** at a fence's edge or a limit: GPS noise flips a
  position in and out, a mean wobbles around 0.8.
- **Order of what is sent late:** pings a tracker sends after being
  offline are applied in the order of their time, not of their arrival,
  or a fence is entered after it was left.
- **Many fences:** one live query each (§138), or one for every
  position and the fences in the application's own memory, an R-tree
  among them, so a ping costs about log n tests instead of n. Either
  keeps trunkdb out of geometry.
