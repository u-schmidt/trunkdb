# 171. Index leaves stored after an anchor (`index/leaf.rs`, `index/btree.rs`, `storage/slotted.rs`, `storage/mod.rs`)

An index leaf stored each entry whole: the key, then the document's
location as 10 bytes (§10, §28). Neighbouring keys repeat each other.
A compound index `(run, path)` holds the same run id at the start of
every key, and paths that share their leading directories; the primary
index holds ids made in the same second, whose first 6 bytes are their
timestamp (§4.2). In a real program's file of 415 MiB, two indexes of
(a run's id, a path's 230 bytes of hex) took 229 MiB of index pages,
against 172 MiB of documents.

Now each leaf keeps **one anchor key**, and every entry stores **how
many bytes it shares with the anchor, and the rest**. The location
becomes two varints. A program sees nothing of it: the same keys, the
same order, the same answers, in fewer pages.

## 171.1 The layout
A new page type, `IndexLeafPacked` (7). Its cells, in slot order:

- **slot 0, the anchor:** `[0][anchor key]`. Any key, chosen when the
  page is built; it needn't be one of the page's entries. The leading
  0 makes even an empty anchor a cell, not a tombstone.
- **every other slot, an entry:** `[varint shared][varint page][varint
  slot][rest]`. Its key is the anchor's first `shared` bytes followed
  by `rest`.

Branch pages, the sibling links between leaves and the slot directory
are what they were. The anchor is the **middle entry's key** when the
page is built (`anchor_of`): in a sorted run, the key in the middle
shares the most with the keys on both sides of it.

**Each entry reads on its own,** by its slot and the anchor
(`decode_packed`), and a key is never put back together to be
compared: `KeyParts` holds the anchor's part and the rest, and compares
them in place against a bound. So the binary search within a page
(§150) works as it did, a walk lends its keys (§101.1), and an insert
or a remove touches no other entry (§52).

## 171.2 Writes
- **An insert** encodes its key against the page's anchor and puts the
  cell at its slot, in place (§52, §151), if it fits.
- **A full leaf is rebuilt** first, from its entries with the new one,
  around a new anchor in their middle: a leaf that grew away from its
  anchor, keys inserted far from it sharing less, gets one that fits
  them again, and often room with it.
- **A split** follows, if the rebuilt page doesn't fit. Each half gets
  an anchor of its own, so the byte midpoint (§28.2) is a guide, not a
  guarantee: where a half doesn't fit, the entry count's midpoint is
  tried, then every other point. A key is at most a quarter page, so a
  point always exists; not finding one is an error, not a panic.
- **Appending** past the rightmost leaf's end leaves that leaf as it is,
  full, but for its sibling link, and starts a new one (§41): its
  anchor stays, so what was counted for the page stays true.
- **A remove** takes out the entry's cell and nothing else. The anchor
  stays, even if no entry has its key any more: it is only bytes to
  share.
- **A sorted build,** `compact` and filling a new index (§41,
  `SortedBuild`), packs leaves as it goes, anchored at each leaf's
  middle, so a rebuilt index is packed whole.

## 171.3 Older files
A leaf written before format 14 is a plain `IndexLeaf` and stays one
until it's written: lookups, walks, counts and removes read it as
before. The first insert into it rebuilds it packed, which also drops
any tombstones an older version left (§52). `compact` rewrites every
index packed. The format is the 14 of §170: both came in the same
release, and a build before it refuses a file with packed leaves.

## 171.4 Damage
A packed leaf is checked once, as the page's layout is (§66,
`check_packed` in `SlottedPage::check_layout`): its first cell must be
the anchor, and every entry's varints must end, its slot fit a `u16`,
and its `shared` be no longer than the anchor. A page that fails is
`InvalidData` from every read: "index page N: a packed index entry's
varint doesn't end". A cell may be as short as one byte on a packed
page; the minimum entry length (§101.3) stays for plain ones. Keys out
of order are still `check`'s to find (§39).

## 171.5 Measured
Release build. The bench's 100,000 tasks with indexes on `tenant`,
`created` and `(status, created)`, with §170:

| | before | now |
|---|---|---|
| index pages | 2,742 | 1,654 |
| file after inserting | 32.6 MB | 23.7 MB |
| file after `compact` | 23.1 MB | 17.9 MB |

And a workload shaped like the program above: 150,000 file entries in
three runs of the same 50,000 paths, each path about 55 characters
under a tree of directories and stored in hex, each run a 32-character
id, each hash 64; indexed by `(run, path)` and `(run, hash)`:

| | before | now |
|---|---|---|
| index pages | 9,250 | 4,970 |
| file after inserting | 114.1 MB | 77.7 MB |
| file after `compact` | 93.6 MB | 68.4 MB |

Against 0.33.0, with both changes, the bench's file is 42% smaller
after inserting and after `compact`, the paths' 37% and 33%. Inserts
went from 44k to 48k a second, updates from 16k to 23k. A walk got
slower at first, from the more entries per leaf; §172 made it faster
than before.

## 171.6 Rejected
- **Each key after the one before it,** with restart points every so
  many entries, as RocksDB, LevelDB and WiredTiger store blocks (the
  roadmap's plan): it shares a little more where keys drift through the
  page, but an entry reads only after the entries back to its restart
  point, a binary search lands on restart points and then walks, and an
  insert or a remove changes the entry after it. Those blocks are
  written once; trunkdb's leaves change in place (§52).
- **A prefix common to the whole page:** the anchor scheme with the
  anchor cut to what every entry shares. A page spanning two values of
  the first field shares nearly nothing; an anchor still shares with
  each side.
- **The first key as the anchor:** keys at the far end of the page share
  less with it than with the middle one.
- **Compressing the page** (zstd and the like): a compressed page can't
  be changed in place, and every read would decompress it.

## 171.7 Limits
- **An anchor is chosen when its page is built.** Keys inserted later
  far from it share less, until the page fills and is rebuilt (171.2).
- **Keys that share little,** random ids or hashes first in a key, save
  only the location's bytes: the varints instead of 10 bytes.
- **Old leaves stay plain** until written, or until `compact` (171.3).

## 171.8 Tests
- `index/leaf.rs`: an entry reads back as its key and location whatever
  the anchor, among keys that share nothing, part or all of it, at the
  extremes of a location, and compares with every other key as the key
  itself does; a shared run isn't stored; a leaf with no anchor first,
  an entry sharing more than the anchor has, a varint cut short and a
  slot past `u16` are refused.
- `index/btree.rs`: every test of the tree runs on packed leaves:
  random keys against a `BTreeMap` both ways, splits, removes, walks
  stopped anywhere, counts by leaf, keys in order filling their leaves.
  Leaves written plain, as before format 14, with tombstones, mixed
  among packed ones: they take lookups, removes and inserts, the first
  insert rewriting them packed, and walks and counts over them give
  what a `BTreeMap` gives. A packed leaf whose first entry's varints
  never end is an error from every read.
- `stats.rs`, `compact.rs`: the size a `compact` estimates for an index
  is the size it builds, within one page in a hundred.
- The old-file fixtures (§120) open, check, read and write their plain
  leaves.
