# 183. A WAL bound to its file (`storage/file.rs`, `storage/mod.rs`, `durability/wal.rs`, `database.rs`)

The fourth finding of the review behind §182. The WAL's header was
magic and a version, nothing else, and the file's header said nothing
the WAL could be checked against. Recovery replayed whatever
structurally valid WAL it found beside the file. GUIDE.md says a
database can be backed up by copying the file and its WAL. So: the
application crashes, its WAL holds committed batches, and someone
restores yesterday's copy of the file without removing the WAL. The
next open wrote today's pages over yesterday's file, a B-tree page
here, the catalog there, before anything checked. The same went for
another database's WAL moved beside it, and for a WAL left behind when
its file was deleted, replayed into the new file the open made.

Now the WAL says which file it belongs to, and at which point of that
file's life it started. An open that finds a WAL that doesn't match
refuses, and says how to go on.

## 183.1 An id and a session in the header
The file's header gains two fields, in bytes that were zero, after
the releases of §125:

- **`file_id`, [44..60):** 16 random bytes, made the first time a
  batch is logged for the file, and kept by every header write after.
  A copy of the file has the same id; an import into a new file makes
  a new one.
- **`session`, [60..68):** a random `u64`, new each time the WAL
  starts again from empty, which is after each checkpoint.

`FileStore::start_session`, called by `write_batch` and by the open's
own batch whenever the WAL is empty, gives the header a new session
(and the file an id if it has none) and writes the header in that
batch. So the first record of every session holds the header page,
and every header image in a session carries the session. A checkpoint
writes it back, and from then on the file on disk says that session.

Random, not counted: two copies of one file that each go on writing
would count the same numbers and could not be told apart; random
values can. They come from UUID v7s' random bits: the `uuid` crate
already draws them from the OS, and the library needs no other
dependency for it.

## 183.2 WAL version 4
The WAL header goes on after magic and version with the `WalBinding`:
the file's id, `prev`, the session the file's header had when the WAL
started, and `session`, the new one. 44 bytes where it was 12. Written
with the first record after the WAL was empty, as the header always
was. Versions 2 and 3 are still read and replayed; they say nothing of
their file, so they are replayed as before, unchecked. That happens
once, for a WAL an older release left after a crash.

## 183.3 The check
Before recovery writes a page, both opens, writing and read-only,
read the id and session from the bytes of the file's header page
(`FileStore::disk_binding`), whether or not its checksum matches. A
crash can tear the header, and the WAL is there to repair it. A file
shorter than a page has none: zeros. Then `WalBinding::belongs_to`:

- **The same id, and the session `prev`:** a crash before the
  checkpoint wrote the session's batches back. Replayed, as ever.
- **The same id, and the WAL's own session:** the checkpoint wrote
  them back, header too, but didn't get to cut the WAL. Replayed;
  page images are idempotent (§19), so this changes nothing.
- **A file without an id** (a new one, or one from before this
  release): only a WAL started on a file without one, `prev` 0.
- **Anything else:** another file, or another copy of this one, older
  or newer. `FormatError::WalNotThisFiles`, and nothing written.

A WAL with nothing pending, or of version 2 or 3, passes.

## 183.4 The refusal
`Error::Format(FormatError::WalNotThisFiles { wal, file })`, which
says what happened and how to go on:

> app.trunkdb.wal: this WAL holds writes of another database file, or
> of another copy of this one, so it isn't replayed: its pages would
> overwrite app.trunkdb's. Nothing was changed. If app.trunkdb is the
> file you want as it is, move the WAL aside (rename it to
> app.trunkdb.wal.stale, say) and open again; the writes it holds are
> left out. If the WAL belongs to another copy of the file, put that
> copy back beside it and open it there, so its writes reach it.

`trunkdb` prints it as it prints any error, the C ABI returns it as
`TRUNKDB_FORMAT` with the message, and the C# wrapper throws a
`TrunkDbException` with code `Format` and the same message.

## 183.5 Rejected
- **Moving the WAL aside on its own,** to `.wal.stale`, and opening:
  friendlier, but it decides for the user which of the two to keep,
  and the writes in the WAL may be the ones they want. A refusal that
  says how is one step more and loses nothing.
- **A checkpoint counter instead of random sessions:** catches an
  older copy, but not two copies that both went on writing, which can
  reach the same count.
- **The id alone:** an older copy of the same file, the case that
  started this, has the same id.
- **Writing the header last in a checkpoint, with a flush before it,**
  so that a file one session ahead could skip the WAL: an extra flush
  per checkpoint, for nothing, since replaying it there is harmless.
- **A new file format:** the fields sit in bytes an older release
  leaves zero and doesn't read. A file this release has written opens
  in 0.37.0 as it is; that release's header writes zero them again,
  and the next session here makes a new id. Only the WAL's version
  changes: 0.37.0 refuses a WAL of version 4, which a crash under this
  release can leave, with `FormatError::WalVersion`.

## 183.6 Tests
`database.rs`:
- Crash images (the file and WAL copied while the database is open),
  in the first session and in one after a checkpoint, recover every
  batch.
- A WAL beside an older copy of its file is refused by both opens,
  the file and the WAL as they were, the message saying what to do.
  Beside the newer file it recovers; moved aside, the older copy opens
  as it was and `check`s clean.
- Another database's WAL is refused, and so is a WAL whose file was
  deleted, the new file left empty.
- A file whose header has no id (as before this release) recovers a
  WAL started on it, gets an id, and keeps it.
- The existing crash tests: after the log, mid-write-back, after the
  write-back (the WAL's own session), a torn header.

`durability/wal.rs`: a version 4 WAL replayed only into its file at
either session, not an older or newer one, another file, or a file
without an id; a version 3 WAL replayed into any; an empty one passes.

Mutations, each caught by a test: `write_batch` starting no session;
`belongs_to` taking only `prev`, or only the WAL's session, or
ignoring the id, or taking any WAL into a file without an id; either
open not checking; `start_session` keeping the session, never making
an id, or not writing the header; the WAL's binding not read; the
file's always read as zeros.

Not caught: the open's own batch (a fresh file's catalog, or the stamp
of a newer release) starting no session. It is written back and
checkpointed within the open, and the tests have no way to crash an
open between its log and its checkpoint. Without the session, that
batch's WAL would name no file, and a crash there would be refused
for a file with an id: a refusal, not a wrong replay.

## 183.7 Limits
- A file without an id, from before this release, takes any WAL
  started on a file without one: the guard begins with the file's
  first session under this release.
- A WAL that was refused stays where it is; so does the empty file an
  open makes where one was deleted. Nothing is removed.
- 128 random bits of id and 64 of session: two files or sessions that
  draw the same are not told apart.
