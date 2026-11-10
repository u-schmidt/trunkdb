# 182. Files made new, and their names synced (`export.rs`, `storage/mod.rs`, `durability/wal.rs`, `storage/file.rs`)

A review of writes, recovery, checkpoints and backups found three
places where trunkdb's own files could meet someone else's, or be lost
with their name:

- `export_to_file` (§84.2, §179.4) wrote to `<out>.tmp<pid>`, opened
  with `File::create`: a file already under that name was emptied, and
  a link under it was followed, its target emptied. The checks of §84.1
  look at `out` only. Two exports to the same file from one process
  shared the temporary file, so one could cut the other's short or mix
  its lines in, even after renaming it into place.
- `verify_export` (§165.2) checked that its database and WAL weren't
  there, then opened them, and removed both afterwards, whatever came
  of it. Another open could make them in between. Our open then failed
  on its lock, and the cleanup removed the other's files while it had
  them open; on Unix a lock doesn't stop an unlink.
- A file's sync doesn't promise its directory entry. On Linux, after a
  power cut a new file can be missing although its contents were
  synced, and a rename can be undone. A new database's file and WAL,
  and an export renamed into place, weren't followed by a sync of their
  directory.

The fourth finding, a WAL that belongs to another file or an older copy
of this one replayed over the file, is §183.

## 182.1 An export's temporary file is always a new one
`<out>.tmp<pid>-<n>`, where `n` counts up for each one this process
makes, opened with `create_new`: never a file that was there, and never
through a link (`O_EXCL` doesn't follow one). Under a name that's
taken, the next `n` is tried, and the file there is left alone. On a
failure it is removed, which is safe now that it is only ever ours.

## 182.2 A verify removes only what it made
`verify_export` creates its database file, then its WAL, each with
`create_new`, before it opens them: one that's there is refused with
`AlreadyExists`, as before, and from then on the files are its own. The
cleanup takes in each file once it is created, so a WAL that's there
leaves only the database file to remove. An empty file opens as a new
database (§22.1) and an empty WAL as one with nothing in it.

## 182.3 New names synced
`storage::sync_dir(path)` syncs the directory `path` is in.
`storage::open_or_create` opens a file for reading and writing as
`create(true)` did, but tries `create_new` first, and when that creates
the file it syncs the directory before handing the file out. The
database file (`FileStore::open_before_recovery`) and the WAL
(`WalDurability::open`) open through it, so their names are on disk
before the first batch is logged, let alone acknowledged. An open of
files that are there syncs no directory. `export_to_file` syncs the
directory after its rename. A checkpoint cuts the WAL but never removes
it, and a compaction goes through the WAL (§108), not a rename, so
neither makes a name.

Unix only: Windows can't open a directory as a file, and NTFS journals
the entry. Skipped under `--cfg fuzzing`, as `storage::sync` is
(§55.1).

## 182.4 Rejected
- **A temporary file from `tempfile`** for the export: a dependency the
  library doesn't have outside tests, for a name and a `create_new`.
- **A private directory for `verify_export`**, made with
  `create_dir` and removed whole: `at` is the caller's choice of file,
  documented as such, and the files can be made exclusively as they
  are.
- **A directory sync after every commit:** only a new name needs it.
  The WAL and the file keep theirs once made.

## 182.5 Tests
In `export.rs`: files and, on Unix, links under the next names an
export would try are left as they were, and so is the file a link
points to; eight threads exporting to the same file each succeed, the
file is a whole export, and no temporary file is left; a
`verify_export` whose files another verify is still working in is
refused, and leaves them, and the other then succeeds and removes them;
a new database syncs its directory twice, an open of one that's there
not at all, and an export once, counted through a test-only counter in
`sync_dir`.

Mutations, each caught: the temporary file opened with
`create(true).truncate(true)`; the same with one name for every export,
as before; `verify_export` creating its files without `create_new`;
`open_or_create` without its `sync_dir`; `export_to_file` without its
`sync_dir`.

## 182.6 Limits
- That a directory sync keeps a name through a power cut isn't tested:
  only that it runs.
- A file created by an older release that a crash then lost the name
  of is gone before this release can help.
- A temporary file left by a process that died mid-export stays, as
  before; its name says which process made it.
