# 39. The `trunkdb` command and `Database::check` (`src/bin/trunkdb.rs`, `check.rs`)

Real and tested: a command to look into a file, check it, export it and
import into it — and the consistency check behind it, in the library.

```text
$ trunkdb info app.trunkdb
app.trunkdb
  format 5, 120 pages of 8192 bytes, 3 free
  users: 1200 documents, indexes: age, email (unique)
  runs: 40 documents, indexes: started_at
$ trunkdb check app.trunkdb
ok: 2 collections, 1240 documents, 120 pages
$ trunkdb export app.trunkdb backup.jsonl       # or to standard output
$ trunkdb import copy.trunkdb backup.jsonl      # `-` reads standard input
```

## 39.1 The command
A binary in the same package, so `cargo install` gives both the library
and the command. Arguments are parsed by hand: a package's dependencies
are compiled for every user of the library, and four subcommands don't
justify a parser crate. Exit codes: `0` fine, `1` an error or a failed
check, `2` a usage error. Counts go to standard error during `export`, so
standard output stays pure JSON Lines.

`Database::open` creates a missing file, so `info`, `check` and `export`
first make sure it exists — a typo mustn't leave an empty database
behind. A file another program has open is locked (§21.1); the command
says so in words instead of `WouldBlock`.

What it can't do: read an older format this build refuses (§21.2). To
migrate, export with the old version's command and import with the new
one — from now on each version has its own.

## 39.2 `Database::check`
Reads the whole file under the read lock and returns a `CheckReport`
with every problem found — problems are collected, not raised, so one
damaged index doesn't hide another:
- **Every page has exactly one owner**: the header, the catalog chain,
  the free list, or a collection's data pages (every page one of its
  documents is on, plus its current one), overflow chains, primary or
  secondary index trees. A page claimed twice, one past the file's end,
  and one nobody claims (a leak) are all problems. That's the check
  §37's drop test did by comparing file sizes, now exact.
- **Every document is readable** and stored under the id its primary
  index files it by.
- **Every index matches the documents**: its entries are in strictly
  ascending order, and they are exactly one per document with an indexed
  value — none missing, none extra, all pointing at the right place. A
  unique index (§33) holds no two equal non-null values.

To walk what a collection owns, the page-listing parts of `drop_index`
(`BTreeIndex::pages`) and `drop_collection` (`data::collection_pages`)
became functions of their own, used by both the freeing and the check —
so the two can't disagree about what a collection owns. `FileStore`
lists its free list, refusing one that loops or runs past the end.

`Database::file_info` gives the header's facts: format version, page
size, page count, free pages. Before the header is next written, a
format-4 file says 4 here (§33.4).

## 39.3 Every randomized test now checks the whole file
The helpers the randomized tests end with (§28.7, §34.3, §36.4) call
`check` as well, and so do the export round trip (§30.5) and the drop
test (§37.3). So every scenario since §28 — inserts, updates that move
documents, deletes, unique indexes, drops, `delete_many`, `update_many`,
import — now also verifies that no page leaked or is used twice and that
every index agrees with its documents. All of them passed as they were:
no feature so far leaked a page.

## 39.4 Tests
- `check.rs`: a consistent file reports nothing, also after a reopen and
  a drop; five kinds of damage made on purpose — a leaked page, an index
  missing an entry, an index entry that doesn't match its document, a
  page owned by two collections, a duplicate in a unique index — are
  each named. Checked by breaking `check` itself, four ways: no leak
  scan, no double-owner detection, no missing-entry detection, no unique
  check. Each fails a test. (The double-owner test first accepted
  another symptom as well and so let that mutation pass; it now demands
  the exact problem.)
- `tests/cli.rs`, running the real binary: usage and help, a missing
  file refused and not created, import → info → check → export to a file
  and to standard output → import from standard input, identical; a leak
  made in the file's bytes makes `check` exit with `1` and name the page
  (since §40 a changed byte instead, since changing the header's page
  count now breaks its checksum);
  a file held open by another handle is reported as in use.
- By hand: `info` and `check` on a copy of the sync workload's real
  database: format 4, consistent.

## 39.5 Limits
- `check` reads everything under the read lock: writers wait.
- It finds problems; it doesn't repair them. Export and import (§30)
  rebuild a file from its readable documents.
- The command reads only this build's formats (§39.1).
