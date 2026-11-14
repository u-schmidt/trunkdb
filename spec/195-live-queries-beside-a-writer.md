# 195. Live queries beside a writer (`live.rs`, `database.rs`)

A live query (§138, §145) and a live aggregate (§139) are checked inside
the writer's commit, in the writer's process. A read-only open beside a
writer in another process (§187) reported their first state, and
nothing after: it commits nothing (§194.7). A `trunkdb watch` (§196),
an admin tool, or a .NET program reading the file the application has
open could not see it change. Now they are read again as that writer
commits.

## 195.1 Read again, compared
The WAL beside the file holds the pages each commit wrote, not which
documents (§16): a reader in another process can tell *that* the
writer committed, not *what*. So each live query reads its matches
again, and compares them with what it read before:

- **A thread looks** (`trunkdb-live`), one per read-only open beside a
  writer, from its first live query or aggregate until a look finds
  none left. It holds the database weakly, and ends with it.
- **Every `OpenOptions::watch_every`** (default 100 ms) it takes a
  snapshot, which catches up with the WAL (§188, §192): the few bytes
  the WAL gained. Where the snapshot is the one it read last, the same
  pages and the same commit, it reads nothing more. Both: after a
  checkpoint the reader builds its pages anew, its commits counted
  again from the file, so a number alone may come back.
- **Where the writer committed,** each live query reads its matches
  whole: its conditions, sort and limit, without its `select` (as the
  list is read again, §145.2), and without its hint, which steers the
  first read only, here as anywhere (§132). It keeps each match's id and its
  document encoded; a match that is new or not as it was, and one that
  matched and no longer does, read as it is now, are what changed.
- **Those are handed to what a commit's changes go through:**
  `Watcher::changed` for each, then `settle` (§138.2, §145.2). A sorted
  list's places, a pushed-out member, `select`, a `custom` condition's
  panic ending that query alone: all as in the writer's process.
- **A live aggregate runs again,** as it does in the writer's process
  at each commit to its collection (§139.1), and reports its rows that
  came, went or changed.

The rest of a live query is as before: `LiveQuery` and `LiveAggregate`,
their events and their ends, the C ABI (§194) and C#. Closing the
database ends them at the next look.

## 195.2 What it can't tell
- **Several commits between two looks come as one.** A document
  inserted and updated is one `Entered`, as the last commit left it.
- **A document that came and went between two looks isn't seen,** nor
  one written and written back as it was.
- **A document written as it was is no `Changed`,** where the writer's
  process reports one (§138.3): the comparison has only contents.
- **No `subscribe`.** A `Commit` names each document a batch changed;
  a reader can't know them without reading every collection again.
  `Database::subscribe` beside a writer reports nothing, as its docs
  say.

## 195.3 Errors
- **A snapshot ended under a look** (`Error::SnapshotTooOld`, asked to
  let go, §189), and a writer's open at work (`WouldBlock`): the look
  reports nothing, and the next tries again.
- **Every read comes first,** for every live query and aggregate,
  before any reports: so that one ended under the reads changes nothing.
  A sorted list's settling, where the writer's process may read the
  filter again (§145.2), takes the list the look read, from the same
  snapshot. The first version read it again there, and a let-go between
  the two ended the live query; a reviewer found it.
- **Any other error taking the snapshot,** a damaged WAL say, ends every
  live query and aggregate of the open, each with an I/O error of the
  same kind and message, as a document that can't be read does (§138.5).
- **An error reading a live query's matches** ends that one, as in the
  writer's process.

## 195.4 Rejected
- **Which documents a commit changed, written into the WAL** for
  readers: exact, `subscribe` included, but a format change, a record
  per commit read by readers that may come late (after a checkpoint
  emptied the WAL), and more written by every commit for readers few
  files have. Asked first, and this chosen.
- **The comparison in the `trunkdb` command only:** C#, the C ABI and
  an admin tool reading the file beside the application would have
  nothing, and the command would need what is the library's: a
  filter's `select`, an aggregate's groups, `Left`'s document.
- **A thread per live query:** they look at the same WAL, so one look
  serves them all, under the lock that keeps them.
- **Each match kept as a hash, not its encoded document:** less memory,
  and a change missed where two hash alike. Kept whole: memory as the
  matches take, which a `limit` bounds.
- **Reading again at every look,** without the snapshot's state: an
  idle reader would read every live query's matches ten times a
  second.

## 195.5 Limits
- **A live query keeps its matches encoded,** in the reading process:
  the first `limit` of a sorted one, every match of one without.
- **A look holds the readers' lock** while it reads (§185.4), as any
  read does: a checkpoint due then waits for it, or is left for later.
- **Reports come up to `watch_every` late,** plus the reads.

## 195.6 Tests
`tests/live_beside.rs`, a writer and a read-only open of the same file,
in one process as two would be:

- a live query reports what the writer commits: entered, left as it is
  now, changed, left gone; a write that matches neither then nor now,
  and a commit to another collection, report nothing;
- several commits between two looks come as one, a document that came
  and went unseen;
- a sorted list with a limit, kept from the places, is `find`'s after
  each of 60 random writes, with a checkpoint every 15;
- `select` cuts what is handed out; a field it doesn't hand out,
  written, is a `Changed`;
- a dropped collection leaves each match, and one made again enters;
- a live aggregate's rows, a group changed and one come in;
- a look without a commit reads no document, a condition of the
  application's own counting those it is given, nor does a checkpoint's;
- a live query ends with its database;
- a panic in a `custom` condition ends that query alone;
- a live query after the last one ended starts the looks again;
- a commit after a checkpoint, before the next look, read though the
  reader's new pages count it as the last look's was;
- `live.rs`, `a_sorted_list_beside_a_writer_settles_without_reading`:
  a full list losing its first member, settled against an empty
  database's reading, still reports the member that came in; read
  again there, it would find none.

## 195.7 Mutations
Each alone, its tests run, the file restored:

| Mutation | Test |
|---|---|
| every look reads again, the state not compared | caught: `a_look_without_a_commit_reads_nothing` |
| a state told by its commit alone, not its pages | passed at first; caught by the test added for it, `a_commit_after_a_checkpoint…` |
| what no longer matches not reported | caught: the live query's test, the sorted list |
| a document written, its contents compared not, only its id | caught: `changed` in the live query's test, `select`'s |
| what a look read not kept for the next | caught: `quiet` after a look, the sorted list |
| the looks not started again after they ended | caught: `a_live_query_after_the_last_one_ended…` |
| no thread looks | caught: every test that writes |
| `watch_every` ignored, 100 ms always | caught: `several_commits_between_two_looks…` |
| the first read not kept, its matches reported again | caught: `quiet` after the first state |
| a snapshot let go (`SnapshotTooOld`) ending the queries | passes: no test lets go of a snapshot during a look, which holds one for microseconds |
| reads ended under a look reported as an error | passes: the same |
| a sorted list settled by reading again, not from the look's list | caught: `a_sorted_list_beside_a_writer_settles_without_reading` |
| `watch`: no flush before it waits | caught: every `watch` test hangs, killed |
| `watch`: a reader gone an error | caught: `watch_refuses_…_and_ends_with_its_reader` |
| `watch`: `left` without its document | caught: `watch_prints_what_the_program…` |
| `watch`: a row without its group | caught: `watch_prints_places_and_rows` |
