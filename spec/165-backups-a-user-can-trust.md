# 165. Backups a user can trust (`export.rs`, `bin/trunkdb.rs`, `GUIDE.md`)

trunkdb has had a backup since §30: an export, the whole database as
JSON Lines, every document with its id and every collection with its
indexes, from one snapshot (§80) while reads and writes go on, and an
import that builds the database again. What it lacked is what a user
asks of a backup:

- **How do I make one while my application runs?** The application
  holds the file locked (§21), so `trunkdb export` from outside is
  refused while it runs; only the application's own handle can export.
  Nothing said so.
- **How do I know it can be restored?** Nothing tells. Worse, nothing
  can: an export cut short between two lines, by a full disk or a
  crash while the application wrote it, reads as a complete one, and
  `import` restores what it has without a word.
- **What do I copy, if I copy the database itself?** The file and its
  WAL, and only while no process has it open: written nowhere a user
  would look.
- **How do I move between releases?** Which builds open which files,
  and what an export does there, is spread over the changelog.

This section answers each: an end line that makes an export say how
much it holds (§165.1), `trunkdb verify`, a restore tried and checked
(§165.2), and a section of the guide (§165.4). No file format changed.

## 165.1 An end line, export version 3
```text
{"$trunkdb_export":3}
{"$collection":"users","$indexes":["age"]}
{"name":"Ada","age":36,"_id":{"$id":"0199…"}}
{"$end":{"collections":1,"documents":1}}
```
- **The last line** names how many collections and documents came
  before it, as `Summary` counts them. `export` writes it once every
  document is written, so an export that stops early, for whatever
  reason, has none.
- **`import` of version 3** checks it: an input that ends without it
  fails with `Error::Import` at the line after the last, "the export
  ends before its end line: cut short?"; counts that differ from what
  was read fail at the end line; a line after it that isn't empty
  fails. A failed import has still written what came before (§30.3),
  so the safe way stays as it was: into a new file, switched to when
  it succeeds.
- **Versions 1 and 2** import as before, without an end line, and a
  `$end` line in them is an error rather than a document: no document
  has `$end` as its only field and one wrongly read as one would hide
  the cut. `Summary::ended` says whether the input closed with an end
  line, so a program, and `verify`, can tell an older export from a
  checked one.
- **Moving down:** 0.31.0 and older refuse version 3 by its header
  ("export format version 3 is not supported"), cleanly, before
  anything is written. Such an export becomes version 2 by changing
  its first line to `{"$trunkdb_export":2}` and dropping its last; the
  guide says how (§165.4). An option to write version 2 waits until
  asked.
- **Hand-written files** keep working: an export of version 1 or 2, or
  a version 3 one whose end line counts what it holds. The counts are
  what a person editing a file keeps right; a checksum would not be
  (§165.6).

## 165.2 `trunkdb verify`
```
trunkdb verify backup.jsonl
verified backup.jsonl: 3 collections, 1250 documents, 4 indexes
```
- **What it does:** imports the export into a new database in a
  temporary directory, as a restore would, then runs `check` on it
  (§39) and compares each collection's document count with the
  export's lines; deletes the file and its WAL, whatever the outcome.
  So it finds what a restore would: a line that doesn't read, a tag
  that doesn't decode, two documents with one id, a unique index two
  documents break, an export cut short, a count that doesn't match.
- **Exit codes:** 0 restorable; 1 not, with the reason and, where
  there is one, the line, or each of the `problems`; 2 a mistake in
  the call.
- **An export of version 1 or 2** has no end line: `verify` restores
  and checks it as far as it goes, exits 0, and says on stderr that it
  can't tell whether the export is complete.
- **Where:** the system's temporary directory (`std::env::temp_dir`),
  a file `trunkdb-verify-<process id>.trunkdb`; a restore takes room
  about the size of the database the export came from. `-` reads the
  export from standard input. Read-only towards the export, and it
  opens no database the user named.
- **In a program:** `trunkdb::verify_export(input, at)`, `at` the path
  of the database to restore into, which must not exist, deleted
  afterwards whatever the outcome: a backup an application makes can be
  checked by the application. It returns a `Verified` report, as
  `check` returns a `CheckReport` rather than failing (§39): the
  collections, documents and indexes restored, `ended`, and
  `problems`, what `check` found in the restored database and every
  collection whose count differs from the export's lines, empty for a
  backup that restores whole; `is_ok()` for both at once. An export
  that doesn't import is `Error::Import`, with its line, as for
  `import`; an `at` that exists is refused before anything is written.

## 165.3 What doesn't change
- `export` and `import`, but for the end line: the same lines, the same
  order, the same snapshot, the same chunks.
- `trunkdb export` writes through a temporary file and renames it
  (§84), so a cut-short export never takes the place of a good one.
- The file format: 12, opened by 0.20.0 and later.

## 165.4 The guide: backups and upgrades
A section of `GUIDE.md`, "Backups, restores and upgrades", answering
the four questions with what there is:
- **While the application runs:** `db.export(file)` from its own
  handle, on a timer or on demand, into a temporary file, synced and
  renamed over the last backup, then `verify_export` if it wants to
  know; a code example that does exactly that. From outside, `trunkdb
  export` works only while the application is closed; the file is
  locked while it's open, by design.
- **Restoring:** `trunkdb import new.trunkdb backup.jsonl` into a new
  file, and the application pointed at it, or the old file replaced,
  once it succeeded.
- **Copying the database itself:** `app.trunkdb` and, if it's there,
  `app.trunkdb.wal`, both, only while no process has the file open: the
  WAL may hold committed batches not yet in the file (§16). A clean
  close checkpoints, and leaves the WAL empty, but a crash doesn't. A
  copy taken while the file is open may be torn: export instead. A
  `.compact` file beside it is a compaction's, never needed.
- **Releases:** a build opens the file formats its `Error::Format` names:
  today 6 to 12, so every file 0.6.0 or later wrote. A newer build that
  opens an older file, but for reading only (§88), stamps it with its
  own format there and then, and from then an older build refuses it
  with `Error::Format`: a copy of the file before the upgrade is the
  way back, or an export. Export and import move
  data across any of that, up or down, as far as the export version
  allows (§165.1). `trunkdb info` says a file's format and the releases
  that created it and last wrote it (§125).

## 165.5 Tests
In `tests/backups.rs`:
- **The end line:** an empty database's export is the header and an end
  line of zeros; one of three collections, one of them empty, ends
  with their counts; a snapshot's export is the same text; its import
  says `ended`.
- **Cut short:** the export cut after each of its lines, with and
  without the last newline, fails to import at the line after the
  last kept, "cut short"; empty lines after the end line are fine; each
  count off by one, at the end line; an end line with a count missing,
  one too many, a number instead of counts, or a negative count,
  likewise; a line after the end line, at that line.
- **Older exports:** versions 1 and 2 import, `ended` false; an end
  line in either fails at its line, naming the version; a document
  with `$end` among other fields is a document.
- **`verify_export`:** a good export, 3 collections, 8 documents, 2
  indexes, `ended`; one cut short, one with a damaged line, one with a
  document twice, one whose unique index two documents break, each
  failing as its import would; a version 2 one, restored, not `ended`;
  after each, the restore and its WAL gone; an `at` that exists, and a
  WAL there, refused and left as they were.

In `export.rs`: what `verify_export` holds a restore to, which a
correct import never trips, on a database of its own: a collection with
fewer documents than the export said and one missing, named; a page
with a changed byte, which `check` reports.

In `tests/cli.rs`: `trunkdb verify` of an export, from a file and from
standard input; one cut short, exit code 1, the line, nothing on
standard output; an export of version 1, restored with the note; a file
that isn't there; no file named; no restore left in the temporary
directory.

The example in the guide runs as `verify_export`'s doc test. Its
`sed` for taking an export to 0.31.0 was run by hand, and the result
imported and verified as version 2.

## 165.6 Mutations
Each made by hand and the tests run:
- **The end line not written,** **written before the collections,**
  **its counts not checked,** **a missing one taken as fine in version
  3,** **a line after it let through,** **an end line in version 1 or
  2 let through,** **the restore left behind,** and **an `at` that
  exists not refused:** caught by `tests/backups.rs`.
- **`verify` skipping `check`,** and **the restored counts not
  compared:** survived at first, as no export that imports trips them;
  caught once the checks after the import were a function of their
  own, tested on a database with the counts off and a damaged page.
- **The command ignoring the problems `verify_export` reports:**
  survives. Only an import that is wrong can make problems for it to
  print, and the command has no way to make one; its two lines were
  read instead.

## 165.7 Rejected
- **A checksum over the lines,** as the WAL's records have one (§40):
  it would catch a byte changed inside a string, which the end line
  doesn't, but an export is also a file people read, fix and write by
  hand, and every edit would need the sum made again. The counts catch
  what loses data silently, an export cut short; a byte flipped by a
  disk is what `check` and the page checksums guard the database
  against, and the same disk holds the export.
- **A trailer per collection** (`"$documents": n` on each collection
  line): it can't be written before the collection's documents are,
  without counting them twice, and the end line already catches a cut
  anywhere.
- **`verify` without a restore,** reading the lines and counting: it
  would miss what only building the database finds, a unique index two
  documents break, an id twice. A restore is the only proof that a
  restore works.
- **A native backup while the application runs,** `backup_to(path)`:
  deferred (ROADMAP.md); the export stays the portable one.
- **Exporting from outside a running application:** the lock is what
  keeps two writers off one file (§21), and a reader beside a writer in
  another process would need shared memory for snapshots (§80); the
  application exports, or offers a way to.
