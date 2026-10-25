# 120. Old files as fixtures (`tests/old_files.rs`, `tests/fixtures/`)

A build opens every file an earlier one wrote (ROADMAP.md, A settled
API and format). Until now that was tested by rewriting the version in a new
file's header (§21.2, §33.4): it shows that a build accepts an older
format, not that it reads what an older build really wrote. A change to
how a value is stored that's made in the encoder and the decoder alike
passes every round-trip test, and leaves every file already written
unreadable.

Now each release, from 0.23.0, keeps a small database it wrote in the
repository, with what it holds, and every build reads all of them.

## 120.1 What a fixture is
`tests/fixtures/<release>/`:
- **`trunkdb.db`**, the database, closed cleanly, so with no WAL.
- **`documents.jsonl`**, one `{"collection": …, "document": …}` per
  line, each collection's documents in id order, as a `Document` prints
  (§71).
- **`indexes.jsonl`**, one `{"collection", "fields", "unique",
  "sparse"}` per index.

The two lists are read back from the closed file by the same run, so
they say what's in it, not what was meant to be. A text form rather
than the export (§30), because the export's format may move on; the
JSON a `Document` prints is a promise of its own (§71, §111.4).

## 120.2 What it holds
What a release can write, kept small: 168 KB and 21 pages since
§120.9, 270 KB and 34 before:
- **`kinds`**, every kind of value: null, both booleans, `i64`'s ends,
  `5.0`, `-0.0`, a subnormal, NaN and both infinities, the empty string
  and one with quotes, a newline and a NUL, binary empty and not,
  date-times before and after 1970 with nanoseconds, nested arrays and
  objects with the empty name and a dot in a name, an id, and two
  documents on overflow pages (§26).
- **`tasks`**, 60 documents with four indexes, one of each kind: plain
  (`status`), compound (`team`, `created`), unique (`n`) and sparse
  (`owner`), whose field is a string, null or missing. Team names of
  over 150 bytes give the compound index more entries than a leaf
  holds, so a branch above them. Every seventh
  deleted and every eleventh grown, so pages have free space and moved
  documents.
- A collection made and dropped, its pages on the free list.

The data come from a fixed generator, so the same release writes the
same documents; their ids differ each time it's run, which is why it's
run once.

## 120.3 What's checked
For every fixture, oldest first, on a copy:
- Opened read-only, `check` finds nothing (§39): every page, every
  index entry against its document.
- The collections, in order, and each one's documents, as printed, are
  those of `documents.jsonl`; its indexes those of `indexes.jsonl`.
- Through each index: a sort each way, `is null`, `>=` a value, and `=`
  values documents hold, each answering as `Filter::apply` does on the
  listed documents (§105), the same documents with the sort key in the
  same order. An `=` on a value an index holds is read through an index,
  not a scan.
- Opened to write: an insert, a delete of a document the release wrote,
  a duplicate refused by each unique index on one field, and `check`
  finding nothing after reopening.

## 120.4 A release's step
`cargo test --test old_files -- --ignored write_this_releases_fixture`
writes `tests/fixtures/<the version in Cargo.toml>/` and refuses to
write over one that's there. It's ignored otherwise, so `cargo test`
never writes into the repository. Run once at each release, after the
version is set, and committed with it, for a release that changes how
files are written (§120.9). A fixture is never rewritten by a later
build: a later build that can't read one has broken a file somebody
has.

The fixtures stay out of the crate's package, as `bench/` and `fuzz/`
do, and git keeps them byte for byte (`.gitattributes`), so a checkout
on Windows doesn't change their line ends.

## 120.5 Rejected
- **A fixture per format version** instead of per release: a release can
  change what it writes without a new format version, as long as older
  builds read it; that's the change worth catching.
- **Fixtures written by the old releases now,** for formats 6 to 11,
  which this build still opens as they are (§21.2): each would need the
  generator written again against that release's API, built from its
  commit. Those formats stay tested as before, by a new file's header
  rewritten; a fixture for one can still be added, under its release's
  name, if one of them ever breaks.
- **Bigger data,** many levels of B-tree: §105's target and the
  library's tests cover the structures; a fixture is for the bytes, and
  one per release adds up.

## 120.6 Tests
`tests/old_files.rs`: the 0.23.0 fixture read as in §120.3.

## 120.7 Mutations
Run against the fixture test, and against every other test:
- **Two stored type tags swapped,** Binary's and DateTime's, in the
  encoder and decoder alike: the fixture test (`check` can't read the
  documents), and **none of the other tests**, since every value still
  round-trips.
- **Two index key tags swapped,** a number's and a string's: the
  fixture test (`check` finds every entry not matching its document),
  and 9 others, because the sort order changes.

## 120.8 Limits
- One file per release, written by that release's build in one way; the
  WAL after a crash is tested by the library (§19), not by fixtures.
- A fixture doesn't catch a change that only an older build would
  notice, a newer file read by an older build; that's refused by the
  format version (§21.2).

## 120.9 Smaller, and only where storage changed
Added on 2026-10-25. A fixture per release is about 380 KB on disk,
47 KB in git's packs, most of it the data's size, not the bytes worth
keeping. Two changes:
- **The generator writes less**: 60 tasks instead of 300, the overflow
  documents just over a page each (9,000 characters, 1,000 numbers)
  instead of several, 40 dropped documents instead of 60, and long team
  names so the compound index still has a branch. What §120.2 lists is
  all still there. The 0.23.0 fixture: 168 KB and 39 KB of lines on
  disk, 9 KB compressed, from 272 KB, 104 KB and 32 KB.
- **A fixture only for a release that changes how files are written**:
  a document's or an index key's encoding, a page's layout, the
  catalog, the header, the format version. A release that changes
  queries, the command, the docs or the API alone writes what the one
  before it wrote, and that one's fixture covers it. In doubt, a
  fixture. Still one per such release, not per format version (§120.5).

**"Never rewritten" means by a later build.** The 0.23.0 fixture was
written again for the smaller generator, by 0.23.0's own code: its
commit (`331e3fe`) checked out beside the repository, the generator's
sizes changed there and nothing else, its `write_this_releases_fixture`
run, and its own reader test passed before the three files came back.
They are still the bytes 0.23.0 writes. Any fixture can be made smaller
that way; none is replaced by what a newer build writes.

The mutations of §120.7, run again: the Binary and DateTime type tags
swapped still fail the fixture test and no other.
