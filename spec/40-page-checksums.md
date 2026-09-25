# 40. Page checksums (`storage/file.rs`, `crc32.rs`, `data.rs`, `check.rs`)

Real and tested: every page in the file ends with a checksum of its id
and its bytes. A page whose bytes don't match it — a disk error, or a
change made outside trunkdb — is an error when it's read, never
misread as data, and `check` names it. File format 6.

```text
$ trunkdb check app.trunkdb
problem: page 10 is damaged: its checksum doesn't match
problem: "users": 16 documents can't be read, on damaged page 10
problem: leaks not checked: not everything could be read
3 problems: 1 collection, 284 documents, 27 pages
```

## 40.1 Where the checksum goes
In each page's last 4 bytes. Only `FileStore` sees them: it appends the
checksum when it writes a page to the file and verifies and cuts it off
when it reads one. Everything above it works with pages of
`USABLE_PAGE_SIZE` (8188) bytes, and the WAL logs pages of that size
too, so the checksum is computed at write-back and at recovery, from
exactly the bytes that go to disk. The page's id is part of what's
summed: a page that is intact but in the wrong place (a misdirected
write, or one page copied onto another) doesn't pass. Neither does a
page of zeros, which is what a file extended by a crash can hold.

Rejected:
- **A checksum field in each page type's own header.** Every layer
  (slotted pages, overflow pages, the free list, the header) would
  compute and check its own. In `FileStore` it's done once, for every
  page type, including ones added later.
- **Checksums kept elsewhere**, in pages of their own. Pages would keep
  their layout, and a format-5 file could have been upgraded in place;
  but every page write also changes a checksum page, and the checksum
  pages need protecting too.
- **Pages of 8192 + 4 bytes on disk.** No layer above would notice. But
  pages would no longer line up with the operating system's 4 KB
  blocks, so each page read and write would touch three blocks
  instead of two.
- **A checksum next to each pointer to a page**, as ZFS and redb do. That
  also catches a lost write (a page left at an older, intact version,
  §40.7), but every pointer in every layer would carry one.
- **Checksums that can be turned off**: two formats to test for
  something that should always be on.

## 40.2 CRC-32C, in hardware
The checksum is CRC-32C (Castagnoli), the one iSCSI, ext4 and RocksDB
use, because x86-64 (SSE 4.2) and ARM64 compute it in hardware: eight
bytes per instruction. `crc32.rs` checks at run time for the
instruction, and otherwise uses a table-driven version that takes
eight bytes per step ("slicing-by-8"). Its tables are built by a
`const fn` at compile time, so there's nothing to initialize and no
dependency. The WAL uses the same CRC now, instead of the zlib CRC-32
it computed bit by bit (§19.3). Its version went to 2, since its page
images are 4 bytes shorter as well.

Rejected:
- **The zlib CRC-32**, which the first version used, table-driven.
  There's no instruction for it. Ten full scans of 50,000 documents
  took 3.2 s with it, against 0.8 s without checksums; CRC-32C in
  hardware took 1.5 s.
- **A crate** (`crc32fast`, `crc32c`): about 100 lines of code, and a
  library's dependencies are compiled for everyone who uses it (§39.1).
- **A stronger hash** (xxHash, 64 bits): the checksum catches damage,
  not an attacker, and 32 bits miss random damage once in four billion.

## 40.3 A scan reads each page once
That measurement showed where the time went: a scan read a data page
from the file again for every document on it, about 18 times per page
in the test. `data::Records` keeps the last data page it read, so
documents one after another on the same page cost one read. `find`,
sorting through an index (§34), `export` and `check` read through it.
It's only used by reads that change nothing in between, which its
`&dyn PageStore` borrow ensures. Ten full scans take 0.43 s now,
checksums included, against 0.8 s before §40. A lookup by id still
reads its page once.

## 40.4 Reading a damaged page
`read_page` returns an `InvalidData` error: "page 57 is damaged: its
checksum doesn't match its bytes (a disk error, or a change from
outside trunkdb)". A query that reads the page fails with it. Nothing
is decoded from the damaged bytes.

`check` first reads every page and lists each damaged one
(`FileStore::damaged_pages`). After that come the problems of whatever
couldn't be read because of them, each said once:
- The documents on a damaged data page are one problem, "16 documents
  can't be read, on damaged page 10". They aren't read one by one, and
  their index entries aren't compared with anything: before this, one
  damaged page showed up as 34 problems, most of them index entries
  "that don't match" documents nobody could read.
- A damaged index or catalog page stops the check of what it belongs
  to, with the error that stopped it.
- Leaks aren't checked once anything couldn't be read: whatever that
  was may own pages, and each of them would look leaked. The report
  says so instead.

Found by damaging each page of a small file in turn and reading the
reports.

The header is handled differently, in two ways:
- **Order of checks.** Its magic, format version and page size are
  checked before its checksum. A format-5 file has no checksum where
  format 6 has one, and "format 5: export it with the version that
  wrote it" helps, where "page 0 is damaged" wouldn't.
- **When the checksum is checked.** `FileStore::open` reads the header
  before WAL recovery (§19.4), and a crash can tear the header's write:
  §22.4 relied on the header's fields sitting in its first 512 bytes,
  but the checksum sits in its last ones. So `Database::open` checks the
  header's checksum only after recovery, which rewrites the header from
  the WAL if a batch was logged. A torn header with no batch in the WAL
  can't come from a crash, so it's reported as damage.

## 40.5 Format 6, and no reading format 5
Pages in formats 4 and 5 use the bytes where the checksum now goes:
slotted pages fill from the end. So this build doesn't read them
(`COMPATIBLE_OLDER_FORMATS` is empty), and a file moves up by export
and import (§30): `trunkdb export` of the version that wrote it, then
`trunkdb import` of this one (§39.1). That's the other reason this came
now: while few files exist, a format change costs little.

## 40.6 Tests
- `crc32.rs`: the standard check value; the table version, and the
  hardware version where the CPU has one, both agree with a bit-by-bit
  CRC for every length from 0 to 300; a CRC over pieces equals one over
  the whole.
- `storage/file.rs`:
  - every page on disk ends with the CRC of its id and bytes;
  - a changed bit makes that page unreadable and only that one, wherever
    the bit is: the page's first or last byte, or its checksum;
  - a page copied onto another fails, and so does a zeroed page;
  - a damaged header fails `open`;
  - a format-5 header is named as format 5, not as damaged;
  - restored WAL pages get their checksum;
  - formats 4 and 5 are refused with the export message.
- `data.rs`: `Records` reads a page once for the documents on it in a
  row, and still the right page after moving to another and back.
- `database.rs`: a header torn by a crash mid-write-back, with its batch
  in the WAL, is recovered. The same tear with an empty WAL fails `open`
  as damage.
- `check.rs`: a byte changed on disk in a data page, a collection's
  current data page, or an index page gives exactly three problems:
  that page, by id; what couldn't be read because of it; "leaks not
  checked". With the byte restored, the file checks clean.
- `tests/cli.rs`: `trunkdb check` on a file with a changed byte exits
  with `1` and names the page. This replaces the leak test there, which
  edited the header's page count and now breaks its checksum instead;
  leaks are still tested in `check.rs`.
- Checked by breaking it on purpose, twenty ways. Each fails a test:
  - the id left out of the sum;
  - every checksum accepted;
  - reads not verified;
  - the checksum computed over the wrong bytes;
  - the header's checksum not checked, or checked before its format;
  - the header checked before WAL recovery, or not at all;
  - damaged pages not listed, or not reported by `check`;
  - the hardware CRC skipping the leftover bytes;
  - three wrong steps in the tables or the table lookup;
  - `Records` reusing its page for any location, or never;
  - `check` reading documents on damaged pages anyway, comparing their
    index entries, checking leaks regardless, or reading a damaged
    current page.

  One of the twenty, the wrong bytes, made every page damaged and hung
  a test instead of failing it.

## 40.7 Limits
- **A lost write** passes: a page that a write never reached, left
  intact at an older version, still matches its own checksum. Catching
  that needs a checksum next to each pointer (§40.1).
- **Damage in the header or the catalog stops `open`**, so `check` can't
  run on such a file. Repairing is open (ROADMAP.md).
- **Only bytes read from the file are checked.** A page damaged in
  memory isn't caught. Neither is a bug that writes wrong bytes, since
  they get a matching checksum; finding that is `check`'s job (§39.2).
