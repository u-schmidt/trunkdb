# 127. A database opened through a symlink (`database.rs`, `bin/trunkdb.rs`)

The WAL is named after the path an open is given, with `.wal` on the
end (§19), and so is compaction's file, with `.compact` (§108). The
file lock (§21.1, §88) is on the file itself. So a symlink and the file
it names were one database to the lock but had two WALs:
`link.trunkdb.wal` and `real.trunkdb.wal`.

A robustness review found it (2026-10-26) and reproduced it: after a
crash that left a committed batch in `real.trunkdb.wal`, a read-only
open of `real.trunkdb` found two documents and one of `link.trunkdb`
found one. Both opens succeeded.

A writable open through the link does worse:
1. The WAL beside the link is empty, so the open starts from the
   main file as it was before the crash. The batch in
   `real.trunkdb.wal` is not there.
2. Commits through the link are logged beside the link and written
   back to the main file.
3. The next open of `real.trunkdb` replays its old WAL over those
   pages, putting back older images of pages that newer commits
   changed. The file can then contradict itself, with no error.

`trunkdb export` had the same gap: it refuses to write over the
database or its WAL (§84.1), but through a link it protected
`link.trunkdb.wal` and would have replaced the real WAL.

## 127.1 What changes
- **`Database::open_with` follows a symlink at `path`** to the file it
  names before anything is opened: the lock, the WAL, and compaction's
  file are all that file's, whichever name opened it. A link to a link
  is followed too, and a relative target is read from the link's
  directory, as the OS reads it.
- **Only the last part of the path.** A directory reached by two names
  (`/data` and a link to it) already holds one WAL: `.wal` is added to
  the file's name, so both names find the same one.
- **A path that isn't there** is left as it is, as before. **A link
  whose target isn't there** is followed: the open creates the target,
  as `open` through a link always did, and now its WAL beside it, not
  beside the link.
- **More than 40 links**, or a loop of them, is `Error::Io` of kind
  `InvalidInput`, before anything is created. 40 is Linux's limit for
  `open`.
- **`trunkdb export`** protects the WAL beside the file the database
  path resolves to.
- **Documented** on `Database::open`: a symlink is followed, a hard
  link is not supported.

## 127.2 Hard links
A hard link is a second name for the same file, and no name is the
real one. From one name the others can't be found without searching
the whole file system. So an open through a hard link still finds the
WAL beside that name. It is documented as unsupported, as SQLite does
for hard links to its database.

A guard was considered: refuse a file whose link count is above one.
It would refuse files that work today, where a backup tool or a
deduplicating file system leaves a second link and nothing opens the
database through it, and it has no counterpart on Windows. Not done.

## 127.3 Rejected
- **`std::fs::canonicalize`:** it resolves every symlink in the path,
  directories too, but fails on a path that isn't there yet, the usual
  case for a first open, and on Windows returns `\\?\` paths that some
  tools don't read. The last part is the only one that changes which
  WAL is found (127.1), so following that alone is enough and needs
  neither.
- **A lock on the WAL as well:** it would make a second WAL fail at
  open, but a read-only open after a crash would still find the empty
  one beside the link and trust it. The WAL has to be the right file,
  not only a locked one.
- **Storing the WAL's name in the header:** a moved or copied database
  would then point at the wrong place, and the header would have to be
  read before the WAL that may hold its newest version (§40.4).

## 127.4 Tests
- `an_open_through_a_symlink_recovers_from_the_files_wal`: a batch
  logged on the file and left by a crash. A read-only open through the
  link and then a writable one find it; the writable one writes
  through the link. No WAL appears beside the link, and an open of the
  file itself finds both the batch and the later write, and `check`
  passes.
- `symlinks_are_followed_to_the_file_or_refused`: a link to a link,
  with a relative target in another directory, to a file that isn't
  there. The open creates the file and its WAL in that directory, and
  compaction's file is named there. Nothing but the links is in the
  links' directory. A loop of two links is `InvalidInput`, and
  nothing is created.
- `tests/cli.rs`, `export_refuses_the_database_itself_and_replaces_the_output_whole`:
  `export` through a link to the database, into the file's WAL, is
  refused and the WAL is left as it was. Without the fix to the
  command it replaces the WAL.

The two library tests run on Unix only: a symlink on Windows needs
developer mode or administrator rights.

## 127.5 Mutations
Each made by hand and run against the two library tests:

| Mutation | Result |
|---|---|
| `open_with` doesn't follow (the path as given) | caught |
| A relative target taken from the working directory, not the link's | caught |
| A link to a file that isn't there refused | caught |
| Only one link followed | caught |
| No limit on links | caught (a loop never ends) |

## 127.6 Limits
- A hard link to the database is a separate WAL, as before (127.2).
- A symlink replaced by another between two opens points them at
  different files. Each open is consistent with its own target.
- **Moving up from 0.24.0:** a database 0.24.0 left after a crash,
  opened through a link, has its last batches in the WAL beside the
  link, which this build doesn't look at. Opened once with 0.24.0 and
  closed, those batches are in the file and the WAL beside the link is
  empty, a file of no bytes that can be deleted.
- A symlink in a directory part of the path is left to the OS, which
  is enough for the WAL (127.1); `Database` itself never shows its
  path.
