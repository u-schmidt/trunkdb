# 21. An exclusive file lock and a format version (`storage/file.rs`, `database.rs`)

Real and tested: a database file can only be open once at a time, and
its header says which on-disk format it's in. Both turn what used to be
silent corruption or a misread into a clear error.

## 21.1 The lock
`FileStore::open` takes an exclusive lock on the file with std's
`File::try_lock` (stable since Rust 1.89, hence `rust-version = "1.89"`
in `Cargo.toml`) and holds it until the `FileStore` is dropped. A second
open fails right away with `ErrorKind::WouldBlock` — from another
process, and also from a second handle in the same process: each
`Database` caches its own header and catalog, so two of them writing
one file corrupts it either way.

It's the OS's advisory lock (`flock` on Unix, `LockFileEx` on Windows):
it stops other trunkdb opens, not arbitrary programs writing the file,
and the OS drops it when the process dies — no stale lock file to clean
up after a crash, unlike a `.lock` sidecar file with a PID in it.
Known limits: advisory locks are unreliable on some network file
systems, and a platform without file locking fails the open (with
`Unsupported`) rather than silently skipping the lock.

Non-blocking on purpose: waiting for another process that holds a
database open for its whole lifetime would just hang the caller.

`Database::open` now opens the store *before* the WAL, so a refused
second open never reads the WAL either — the running instance may be
halfway through writing it. (The old order, WAL first, was already safe
from truncating that WAL, since the checkpoint only runs after the store
opened; this just makes "nothing happens before the lock" hold without
that argument.) Tested: `a_database_in_use_cannot_be_opened_again` logs
a record through the first instance, then checks the refused second
open left the WAL untouched.

The staging tests in `file.rs` inspect the file through a second
`FileStore` while the first is still open; they use a test-only
`open_with_lock(path, false)`.

## 21.2 The format version
The header gained `[28..32) format_version: u32`, first `1` — the
format of §20 (packed data pages, flags byte, catalog cells with
`current_data_page`), `2` since §26 (`u32` document lengths,
overflow pages), `3` since §28 (catalog cells with a kind byte,
index entries), `4` since §32 (indexes hold null and missing
fields), `5` since §33 (unique indexes; format 4 is still read as
it is, §33.4), `6` since §40 (a checksum on every page; 4 and 5
are no longer read), `7` since §42 (multikey indexes), and `8` since
§43 and §44 (compound and sparse indexes; 6 and 7 are still read as
they are). `Header::decode` checks it right after the magic,
before anything else in the header (another version may lay it out
differently), and only its own version, or one still read as it is,
is accepted. The error says
which case it is:
- `0` — the bytes every header had before the field existed: trunkdb
  0.1.0, or a development build between 0.1.0 and this change;
- higher than this build's — open it with a newer trunkdb;
- lower — an older format: export it with the version that wrote it,
  import it with this one (§30; the message said "no migration" until
  §32).

The magic stays `TRUNKDB1`: it answers "is this a trunkdb file at all",
the version answers "which layout". The version is bumped whenever a
page or cell layout changes; a file moves from one version to the next
by export and import (§30). The WAL keeps its own version (§19.3) — its
record framing is independent of the page layout inside the images it
carries. (It went to 2 with §40 anyway: its images got 4 bytes shorter,
and its CRC changed.)
