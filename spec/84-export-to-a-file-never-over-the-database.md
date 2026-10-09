# 84. Export to a file, never over the database (`src/bin/trunkdb.rs`)

Found in a report on the command of §39: `trunkdb export <db> <out>`
opened `out` with `File::create`, which truncates. With `out` the
database's own path, or a hard link to it, a valid 32,768-byte database
was cut to 59 bytes before the first line of the export was written,
and could not be opened again. The same truncation also destroyed an
existing backup when an export failed halfway.

## 84.1 The destination is checked
`export` refuses a destination that is the same file as the database or
as its log (`<db>.wal`, §19), with "choose another file", before it
touches anything. "The same file" is the same device and inode on Unix,
so hard links and symlinks are caught as well as a different spelling of
the path; elsewhere it is the same canonical path, which does not see a
hard link.

## 84.2 The output is written whole or not at all
The export goes to `<out>.tmp<pid>` in the destination's directory, is
synced, and is renamed over `out`. If anything fails, the temporary file
is removed and `out` is as it was. A symlink as `out` is resolved first,
so the target is replaced and the link stays. Standard output is as
before.

## 84.3 Rejected
- Checking only the path text: misses `./db`, links and aliases.
- Relying on the lock: the export holds the database open, but the
  truncation happens through a second handle, which the lock does not
  stop.
- Writing the temporary file in the system temp directory: a rename
  across file systems fails, and a copy brings the half-written file
  back.

## 84.4 Tests
`tests/cli.rs`: an export to the database's own path and to a hard link
is refused, the file is byte for byte as before and `check` passes; an
export over an existing file replaces it and leaves no `.tmp` file; an
export into a missing directory fails.

## 84.5 Limits
- Not tested: a failure partway through writing leaving `out` alone. It
  follows from the rename coming last.
- A hard link on a platform without inodes is not recognized (§84.1)
  and not refused: the export replaces the link's name, and the database
  keeps its own. The test checks only that on Windows, where CI found it.
- A second hard link to `out` itself is cut loose, not written through:
  the rename replaces the name, not the file.
