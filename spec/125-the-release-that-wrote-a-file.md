# 125. The release that wrote a file (`storage/file.rs`, `release.rs`)

The header said a file's format version (§21.2), and that's all it said
of where it came from. Format 12 has been written by 0.20.0 to 0.23.0
and the next release alike, so a damaged or odd file in a bug report
couldn't say which build made it, or which one wrote it last. SQLite
keeps the version of the library that last wrote a file in its header,
for the same reason.

The header now names two releases: the one that **created** the file,
and the one that **last wrote** it. Diagnosis only: nothing reads them
to decide anything, and no file is refused for what they say.

## 125.1 Where they go
The header page is 8 KiB less its checksum; its fields took 32 bytes,
the rest zeros. After them, little-endian like the others:

| Bytes | Field |
|---|---|
| `[32..34)` `[34..36)` `[36..38)` | created by: major, minor, patch, `u16` each |
| `[38..40)` `[40..42)` `[42..44)` | last written by: major, minor, patch |

From `CARGO_PKG_VERSION_MAJOR`, `_MINOR` and `_PATCH`, parsed at compile
time (`release.rs`); no string in the file and none to parse. All three
`0`, `0.0.0`, means unknown: no release of trunkdb was ever 0.0.0, so it
can't be mistaken for one.

**No new format version.** Every build from §21 on reads bytes `[0..32)`
only and ignores the rest, so a format-12 file with the two releases is
still a format-12 file to 0.20.0. What follows from that is in §125.3.

## 125.2 When they're written
- **A new file**: both are this build's release (`Header::fresh`). That
  includes the new database an import writes into (§30), a new file,
  not a copy of the export's source.
- **The first page a batch writes**, where the header names another
  release as the last to write, writes the header too, with this
  build's release: the same rule §59 and §62 use for an older format
  (`Header::is_stale`), in the same batch, logged and recovered like any
  header change (§19, §85), taken back by a rollback. The header is
  written only when its fields change (page count, free list) or when it
  is stale, not at every commit, so this is one header write per release
  per file, the first time that release writes.
- **"Created by" is never changed** once written. `compact` keeps it:
  `replace_all` writes the header it has (§41, §108), with the new page
  count and free list; a compaction that finds nothing to do writes
  nothing, and stamps nothing.
- **A read-only open** (§88) writes nothing, so neither changes on disk.
  Nor does an open for writing that writes nothing: the stamp rides on a
  page written, not on the open. Where a read-only open keeps changes of
  its own in memory (an older format's, §88), `file_info` shows this
  build as the writer, as it shows the format those changes stamp.

## 125.3 Older builds
A build from before §125 that writes to a file, 0.20.0 to 0.23.0 on
format 12, encodes its header from a zeroed page whenever it writes it,
so it **zeroes both releases**:
- "Last written by" then reads unknown, which is true enough: an older
  build wrote it since. The next §125 build to write stamps it again.
- "Created by" stays unknown for good. A file made before §125 has it
  unknown from the start.

Kept so: the releases are for diagnosis, and refusing the file to
older builds (a format 13) would cost every new file its readability by
0.23.0 for two stamps. Older builds become rarer; an unknown value is
honest.

## 125.4 What a caller sees
`FileInfo` (§39) has two more fields, both `Option`, `None` for unknown:
`created_by` and `written_by`. `FileInfo` is `#[non_exhaustive]` (§60),
so they aren't breaking. A read through a snapshot sees the header of
its commit, as the other fields do.

`Release` is new: three `u16`s, `major`, `minor`, `patch`, `Display` as
`0.24.0`, ordered as releases are (`0.9.0` before `0.10.0`),
`#[non_exhaustive]` so a part can be added. `Copy` and `Eq`, unlike the
reports (§118): a value, which nothing will add a field to that
couldn't be copied or compared. `Release` rather than `Version`, which
would read as the format's or the WAL's version beside them.

`trunkdb::RELEASE` is this build's own, so a tool can show "written by
0.23.0, this build 0.24.0": a program's `CARGO_PKG_VERSION` is its own,
not trunkdb's.

`trunkdb info` prints, right after the format line, with the file's
other facts:

```
  format 12, 4 pages of 8192 bytes, 0 free
  written by 0.24.0, created by 0.24.0
```

`unknown` where it's `None`. The lines that were there before keep their
words.

## 125.5 Builds between releases
`Cargo.toml` names the last release until the next bump, so a build of
`main` after 0.23.0 writes 0.23.0, though it has what came since.
Cargo's pre-release part (`0.24.0-dev`) isn't used by trunkdb and isn't
stored, nor is a commit. "0.23.0" then means "0.23.0, or a build of
`main` after it", which narrows a bug report enough; if it doesn't, the
fix is a version policy (the next version right after a release), not
more bytes.

## 125.6 Rejected
- **The release as text**, `"0.24.0"`: a length or a terminator, room
  to reserve, and parsing on the way out, for what three numbers say.
- **One packed number** as SQLite's (`major × 1,000,000 + minor × 1,000
  + patch`): compact, but each part capped at 999 and hard to read in a
  hex dump; three `u16`s cost two bytes more.
- **A format 13**, so older builds refuse the file and can't zero the
  stamps: §125.3.
- **Only one of the two.** "Last written" is what a bug report needs;
  "created" tells a file that went through many releases from a new
  one, for six bytes.
- **Every release that ever wrote the file**: a list that grows, and
  nothing asks for it.
- **Checking them at open** (a release newer than this build's, say):
  the format version decides what a build reads (§21.2); these only
  describe.

## 125.7 Tests
`release.rs`: `RELEASE` is the version Cargo has; bytes round-trip,
little-endian, major first, zeros unknown; releases order by number, not
text.

`storage/file.rs`:
- A new file is created and written by this release, on disk and after
  a reopen.
- Another release's header (created 0.19.0, written 0.20.0): the first
  page written puts the header in the batch, "written by" this
  release, "created by" kept; a rollback takes it back; the next batch
  writes no header page; on disk after.
- Zeros, as an older build leaves them: unknown; opening alone leaves
  the file's bytes as they were; an allocation stamps "written by" and
  "created by" stays unknown.

`database.rs`:
- `file_info` on a new file; with the releases rewritten by hand, a
  read-only open and an open that writes nothing keep them; an insert
  stamps "written by", kept after a reopen; a compaction that rewrites
  keeps "created by" and stamps "written by"; an import's new file is
  this build's for both.
- A crash after the stamp's batch was logged, before it was written
  back: the open restores it with the rest (§85), and `check` is
  content.

`tests/old_files.rs`: a fixture from before 0.24.0 says unknown for
both, and after this build writes to its copy, "written by" this build
and "created by" still unknown; a later release's fixture says its own
release for both.

`tests/cli.rs`: `trunkdb info` prints the line for a new file, and
`unknown` for both on a copy of the 0.23.0 fixture.

Mutations, each run against these tests:
- "Created by" and "written by" swapped in `encode`: 4 tests fail.
- The release left out of `is_stale`, so no stamp: 2 fail.
- `replace_all` stamping "created by" too: 1 fails, the compaction's.

## 125.8 Docs
`DESIGN.md`'s header row; `FileInfo`'s and `Release`'s doc comments; the
`trunkdb info` line in the README and the Agent Skill.
